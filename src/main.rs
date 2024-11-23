use tokio;
use warp::{Filter, ws::WebSocket, Reply};
use mongodb::{Client, Database};
use std::sync::Arc;
use tokio::sync::Mutex;
use std::collections::HashMap;
use dotenv::dotenv;
use serde_json::json;
use serde::Deserialize;

mod ws_handler;
use ws_handler::{handle_connection, PlayerConnection, Connections, generate_game_id};

#[derive(Debug, Deserialize)]
struct CreateGameRequest {
    time_control: i32,
    increment: i32,
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

    // Initialize shared connections state
    let connections: Connections = Arc::new(Mutex::new(HashMap::new()));

    // Create WebSocket route
    let ws_route = warp::path("ws")
        .and(warp::ws())
        .and(with_db(db.clone()))
        .and(with_connections(connections.clone()))
        .and_then(ws_handler);

    // Create game creation route
    let create_game = warp::path("api")
        .and(warp::path("create-game"))
        .and(warp::post())
        .and(warp::body::json())
        .and(with_db(db.clone()))
        .and_then(create_game);

    // CORS configuration
    let cors = warp::cors()
        .allow_any_origin()
        .allow_headers(vec!["content-type"])
        .allow_methods(vec!["GET", "POST", "OPTIONS"]);

    // Combine routes and start server
    let routes = ws_route
        .or(create_game)
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

async fn ws_handler(
    ws: warp::ws::Ws,
    db: Database,
    connections: Connections,
) -> Result<impl warp::Reply, warp::Rejection> {
    Ok(ws.on_upgrade(|socket| handle_connection(socket, db, connections)))
}

// Updated create_game handler that accepts time control parameters
async fn create_game(
    request: CreateGameRequest,
    db: Database
) -> Result<impl warp::Reply, warp::Rejection> {
    // Validate time control values
    if !ws_handler::is_valid_time_control(request.time_control, request.increment) {
        return Ok(warp::reply::json(&json!({
            "status": "error",
            "message": "Invalid time control values"
        })));
    }

    let game_id = generate_game_id();
    
    if !ws_handler::is_valid_game_id(&game_id, &db).await {
        return Ok(warp::reply::json(&json!({
            "status": "error",
            "message": "Invalid game ID generated"
        })));
    }
    
    // Convert time control to milliseconds
    let time_control_ms = (request.time_control as i64) * 1000;
    let increment_ms = (request.increment as i64) * 1000;
    
    let games = db.collection::<ws_handler::Game>("games");
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
    };

    match games.insert_one(new_game, None).await {
        Ok(_) => Ok(warp::reply::json(&json!({ 
            "game_id": game_id,
            "status": "success",
            "time_control": request.time_control,
            "increment": request.increment
        }))),
        Err(e) => Ok(warp::reply::json(&json!({
            "status": "error",
            "message": format!("Failed to create game: {}", e)
        })))
    }
}