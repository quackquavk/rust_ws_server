use tokio;
use warp::{Filter, ws::WebSocket};
use mongodb::{Client, Database};
use std::sync::Arc;
use tokio::sync::Mutex;
use std::collections::HashMap;
use dotenv::dotenv;

mod ws_handler; // Import your handler module
use ws_handler::{handle_connection, PlayerConnection, Connections};

#[tokio::main]
async fn main() {
    dotenv().ok(); // Load .env file
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

    // CORS configuration
    let cors = warp::cors()
        .allow_any_origin()
        .allow_headers(vec!["content-type"])
        .allow_methods(vec!["GET", "POST", "OPTIONS"]);

    // Combine routes and start server
    let routes = ws_route.with(cors);

    let addr = ([127, 0, 0, 1], 8080);
    println!("WebSocket server listening on: {:?}", addr);
    
    warp::serve(routes).run(addr).await;
}

// Helper function to share database connection
fn with_db(db: Database) -> impl Filter<Extract = (Database,), Error = std::convert::Infallible> + Clone {
    warp::any().map(move || db.clone())
}

// Helper function to share connections state
fn with_connections(connections: Connections) -> impl Filter<Extract = (Connections,), Error = std::convert::Infallible> + Clone {
    warp::any().map(move || connections.clone())
}

async fn ws_handler(
    ws: warp::ws::Ws,
    db: Database,
    connections: Connections,
) -> Result<impl warp::Reply, warp::Rejection> {
    Ok(ws.on_upgrade(|socket| async move {
        let ws_stream = socket;
        handle_connection(ws_stream, db, connections).await
    }))
}