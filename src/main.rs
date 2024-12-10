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
}

// Add security validation for time control
fn is_valid_time_control(time_control: i32, increment: i32) -> bool {
    // Minimum 30 seconds, maximum 180 minutes
    let valid_time = time_control >= 30 && time_control <= 10800;
    // Maximum increment 60 seconds
    let valid_increment = increment >= 0 && increment <= 60;
    valid_time && valid_increment
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
        .allow_origins(vec!["http://localhost:3000", "https://your-production-domain.com"])
        .allow_headers(vec!["content-type"])
        .allow_methods(vec!["GET", "POST", "OPTIONS"])
        .max_age(Duration::from_secs(3600));

    // Add rate limiting to routes
    let ws_route = warp::path("ws")
        .and(warp::ws())
        .and(with_db(db.clone()))
        .and(with_connections(connections.clone()))
        .and(warp::addr::remote())
        .and(with_rate_limit(ws_rate_limit.clone()))
        .and_then(ws_handler);

    let create_game = warp::path("api")
        .and(warp::path("create-game"))
        .and(warp::post())
        .and(warp::body::json())
        .and(with_db(db.clone()))
        .and(warp::addr::remote())
        .and(with_rate_limit(game_rate_limit.clone()))
        .and_then(create_game);

    // Combine routes and start server
    let routes = ws_route
        .boxed()
        .or(create_game.boxed())
        .with(cors);

    let addr = ([127, 0, 0, 1], 8080);
    println!("Server listening on: {:?}", addr);
    
    warp::serve(routes).run(addr).await;
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

    if !is_valid_time_control(request.time_control, request.increment) {
        return Ok(warp::reply::with_status(
            warp::reply::json(&json!({
                "status": "error",
                "message": "Invalid time control values"
            })),
            warp::http::StatusCode::BAD_REQUEST,
        ));
    }

    let game_id = generate_game_id();
    
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