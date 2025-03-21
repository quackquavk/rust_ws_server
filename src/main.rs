use tokio;
use warp::{Filter, ws::WebSocket, Reply, Rejection};
use mongodb::{Client, Database, IndexModel, bson::doc, options::IndexOptions};
use std::sync::Arc;
use tokio::sync::Mutex;
use std::collections::HashMap;
use dotenv::dotenv;
use serde_json::json;
use serde::Deserialize;
use warp::filters::BoxedFilter;
use std::time::Duration;
use tokio::time::sleep;
use std::net::{IpAddr, SocketAddr};
use futures_util::future::join_all;
use warp::path;
use serde::Serialize;
use futures_util::TryStreamExt;
use mongodb::bson;
use warp::http::HeaderMap;

mod ws_handler;
use ws_handler::{handle_connection, PlayerConnection, Connections, generate_game_id, ChatMessage};

#[derive(Debug, Deserialize)]
struct CreateGameRequest {
    time_control: i32,
    increment: i32,
}

// Add rate limiting structure
#[derive(Debug, Clone)]
struct RateLimit {
    requests: Arc<Mutex<HashMap<IpAddr, Vec<i64>>>>,
    max_requests: usize,
    window_ms: i64,
}

impl RateLimit {
    fn new(max_requests: usize, window_ms: i64) -> Self {
        RateLimit {
            requests: Arc::new(Mutex::new(HashMap::new())),
            max_requests,
            window_ms,
        }
    }

    async fn check(&self, ip: IpAddr) -> bool {
        let now = chrono::Utc::now().timestamp_millis();
        let mut requests = self.requests.lock().await;
        
        requests.entry(ip)
            .and_modify(|timestamps| {
                timestamps.retain(|&t| now - t < self.window_ms);
            })
            .or_insert_with(Vec::new);

        let timestamps = requests.get_mut(&ip).unwrap();
        if timestamps.len() >= self.max_requests {
            return false;
        }

        timestamps.push(now);
        true
    }

    async fn cleanup(&self) {
        let now = chrono::Utc::now().timestamp_millis();
        let mut requests = self.requests.lock().await;
        
        requests.retain(|_, timestamps| {
            timestamps.retain(|&t| now - t < self.window_ms);
            !timestamps.is_empty()
        });
    }
}

// Add security validation for time control
fn is_valid_time_control(time_control: i32, increment: i32) -> bool {
    // Minimum 30 seconds, maximum 180 minutes
    let valid_time = time_control >= 30 && time_control <= 10800;
    // Maximum increment 60 seconds
    let valid_increment = increment >= 0 && increment <= 60;
    valid_time && valid_increment
}

#[derive(Debug, Serialize)]
struct GameHistory {
    games: Vec<ws_handler::Game>,
    total: usize,
}

fn validate_username(username: &str) -> bool {
    let username_length = username.chars().count();
    // Only allow alphanumeric characters and underscores, length between 3-30
    username_length >= 3 
        && username_length <= 30 
        && username.chars().all(|c| c.is_alphanumeric() || c == '-')
}

#[tokio::main]
async fn main() {
    dotenv().ok();
    
    // Set up MongoDB connection
    let mongo_uri = std::env::var("MONGODB_URI")
        .expect("MONGODB_URI must be set");
    let client = Client::with_uri_str(&mongo_uri)
        .await
        .expect("Failed to connect to MongoDB");
    let db = client.database("chessdream");

    // Create index for chat messages
    db.collection::<ChatMessage>("chat_messages")
        .create_index(
            IndexModel::builder()
                .keys(doc! {
                    "game_id": 1,
                    "timestamp": 1
                })
                .options(None)
                .build(),
            None
        )
        .await
        .expect("Failed to create chat messages index");

    // Initialize shared connections state
    let connections: Connections = Arc::new(Mutex::new(HashMap::new()));

    // Initialize rate limiters
    let game_rate_limit = RateLimit::new(5, 60000); // 5 requests per minute
    let ws_rate_limit = RateLimit::new(30, 60000);  // 30 connections per minute

    // Create secure CORS configuration
    let cors = warp::cors()
        .allow_any_origin()
        .allow_headers(vec![
           "Content-Type"
        ])
        .allow_methods(vec!["GET", "POST", "OPTIONS"])
        .allow_credentials(true)
        .build();

    // Add rate limiting to routes
    let ws_route = warp::path("ws")
        .and(warp::ws())
        .and(with_db(db.clone()))
        .and(with_connections(connections.clone()))
        .and(warp::addr::remote())
        .and(with_rate_limit(ws_rate_limit.clone()))
        .and(warp::header::headers_cloned())
        .map(|ws: warp::ws::Ws, 
             db: Database, 
             connections: Connections, 
             addr: Option<SocketAddr>, 
             rate_limit: RateLimit,
             headers: HeaderMap| {
            // Configure WebSocket with available options
            let reply = ws.max_send_queue(1024)
               .max_message_size(1024 * 1024); // 1MB limit

            reply.on_upgrade(move |socket| handle_connection(socket, db, connections))
        });

    let create_game = with_timeout(
        warp::path("api")
            .and(warp::path("create-game"))
            .and(warp::post())
            .and(warp::body::json())
            .and(with_db(db.clone()))
            .and(warp::addr::remote())
            .and(with_rate_limit(game_rate_limit.clone()))
            .and_then(create_game)
    );

    let get_player_games = with_timeout(
        warp::path!("api" / "games" / String)
            .and(warp::get())
            .and(with_db(db.clone()))
            .and(warp::addr::remote())
            .and(with_rate_limit(game_rate_limit.clone()))
            .and_then(get_games_by_player)
    );

    // Combine routes and start server
    let routes = ws_route
        .boxed()
        .or(create_game.boxed())
        .or(get_player_games.boxed())
        .with(cors);

    let addr = ([0, 0, 0, 0], 8080);
    
    warp::serve(routes).run(addr).await;

    // In main(), add periodic cleanup
    let rate_limit_clone = game_rate_limit.clone();
    tokio::spawn(async move {
        loop {
            sleep(Duration::from_secs(300)).await;  // Clean every 5 minutes
            rate_limit_clone.cleanup().await;
        }
    });
}

// Helper functions remain the same
fn with_db(db: Database) -> impl Filter<Extract = (Database,), Error = std::convert::Infallible> + Clone {
    warp::any().map(move || db.clone())
}

fn with_connections(connections: Connections) -> impl Filter<Extract = (Connections,), Error = std::convert::Infallible> + Clone {
    warp::any().map(move || connections.clone())
}

// Updated handler functions
async fn ws_handler(
    ws: warp::ws::Ws,
    db: Database,
    connections: Connections,
    addr: Option<SocketAddr>,
    rate_limit: RateLimit,
) -> Result<impl Reply, Rejection> {
    let ip = addr.map(|a| a.ip()).ok_or_else(warp::reject::not_found)?;
    
    if !rate_limit.check(ip).await {
        return Err(warp::reject::custom(RateLimitError));
    }

    Ok(ws.on_upgrade(move |socket| handle_connection(socket, db, connections)))
}

async fn create_game(
    request: CreateGameRequest,
    db: Database,
    addr: Option<SocketAddr>,
    rate_limit: RateLimit,
) -> Result<impl Reply, Rejection> {
    let ip = addr.map(|a| a.ip()).ok_or_else(warp::reject::not_found)?;
    
    if !rate_limit.check(ip).await {
        return Err(warp::reject::custom(RateLimitError));
    }

    if !ws_handler::is_valid_time_control(request.time_control, request.increment) {
        return Ok(warp::reply::with_status(
            warp::reply::json(&json!({
                "status": "error",
                "message": "Invalid time control values"
            })),
            warp::http::StatusCode::BAD_REQUEST,
        ));
    }

    let game_id = ws_handler::generate_game_id();
    
    let games = db.collection::<ws_handler::Game>("games");
    if games.find_one(doc! { "_id": &game_id }, None).await.unwrap().is_some() {
        return Ok(warp::reply::with_status(
            warp::reply::json(&json!({
                "status": "error",
                "message": "Invalid game ID generated"
            })),
            warp::http::StatusCode::INTERNAL_SERVER_ERROR,
        ));
    }
    
    // Convert time control to milliseconds
    let time_control_ms = (request.time_control as i64) * 1000;
    let increment_ms = (request.increment as i64) * 1000;
    
    let new_game = ws_handler::Game {
        _id: game_id.clone(),
        white_player: None,
        black_player: None,
        fen: "rnbqkbnr/pppppppp/8/8/8/8/PPPPPPPP/RNBQKBNR w KQkq - 0 1".to_string(),
        pgn: String::new(),
        status: "waiting".to_string(),
        created_at: chrono::Utc::now().to_rfc3339(),
        updated_at: chrono::Utc::now().to_rfc3339(),
        turn: "white".to_string(),
        moves: Vec::new(),
        white_time: request.time_control,
        black_time: request.time_control,
        last_move_time: chrono::Utc::now().to_rfc3339(),
        increment: request.increment,
        white_time_ms: time_control_ms,
        black_time_ms: time_control_ms,
        last_move_timestamp: chrono::Utc::now().timestamp_millis(),
        increment_ms,
        result: String::new(),
        draw_offered_by: None,
        reason: None,
    };

    match games.insert_one(new_game, None).await {
        Ok(_) => Ok(warp::reply::with_status(
            warp::reply::json(&json!({ 
                "game_id": game_id,
                "status": "success",
                "time_control": request.time_control,
                "increment": request.increment
            })),
            warp::http::StatusCode::CREATED,
        )),
        Err(e) => Ok(warp::reply::with_status(
            warp::reply::json(&json!({
                "status": "error",
                "message": format!("Failed to create game: {}", e)
            })),
            warp::http::StatusCode::INTERNAL_SERVER_ERROR,
        ))
    }
}

async fn get_games_by_player(
    username: String,
    db: Database,
    addr: Option<SocketAddr>,
    rate_limit: RateLimit,
) -> Result<impl Reply, Rejection> {
    let ip = addr.map(|a| a.ip()).ok_or_else(warp::reject::not_found)?;
    
    if !rate_limit.check(ip).await {
        return Err(warp::reject::custom(RateLimitError));
    }

    if !validate_username(&username) {
        return Ok(warp::reply::with_status(
            warp::reply::json(&json!({
                "status": "error",
                "message": "Invalid username format"
            })),
            warp::http::StatusCode::BAD_REQUEST,
        ));
    }

    let games = db.collection::<ws_handler::Game>("games");
    
    let filter = doc! {
        "$or": [
            { "white_player": &username },
            { "black_player": &username }
        ]
    };


    match games.find(filter, None).await {
        Ok(cursor) => {
            match cursor.try_collect::<Vec<ws_handler::Game>>().await {
                Ok(games_list) => {
                    let total = games_list.len();
                    Ok(warp::reply::with_status(
                        warp::reply::json(&GameHistory {
                            games: games_list,
                            total,
                        }),
                        warp::http::StatusCode::OK,
                    ))
                },
                Err(e) => {
                    Ok(warp::reply::with_status(
                        warp::reply::json(&json!({
                            "status": "error",
                            "message": format!("Failed to collect games: {}", e)
                        })),
                        warp::http::StatusCode::INTERNAL_SERVER_ERROR,
                    ))
                }
            }
        },
        Err(e) => {
            Ok(warp::reply::with_status(
                warp::reply::json(&json!({
                    "status": "error",
                    "message": format!("Failed to query games: {}", e)
                })),
                warp::http::StatusCode::INTERNAL_SERVER_ERROR,
            ))
        }
    }
}

// Helper function for rate limiting
fn with_rate_limit(
    rate_limit: RateLimit,
) -> impl Filter<Extract = (RateLimit,), Error = std::convert::Infallible> + Clone {
    warp::any().map(move || rate_limit.clone())
}

// Custom error for rate limiting
#[derive(Debug)]
struct RateLimitError;
impl warp::reject::Reject for RateLimitError {}

// Add a custom timeout wrapper for each handler
fn with_timeout<T: Reply + Send>(
    route: impl Filter<Extract = (T,), Error = Rejection> + Clone + Send + Sync + 'static,
) -> impl Filter<Extract = (impl Reply,), Error = Rejection> + Clone {
    route.and_then(|reply| async move {
        let timeout_duration = Duration::from_secs(30);
        match tokio::time::timeout(timeout_duration, async move { Ok(reply) }).await {
            Ok(result) => result,
            Err(_) => Err(warp::reject::custom(TimeoutError)),
        }
    })
}

// Add this error type with your other error types
#[derive(Debug)]
struct TimeoutError;
impl warp::reject::Reject for TimeoutError {}