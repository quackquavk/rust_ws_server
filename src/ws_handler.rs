use mongodb::{Client, Database, Collection, bson::{doc, Document}};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::Mutex;
use tokio_tungstenite::tungstenite::Message as TungsteniteMessage;
use futures::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use jsonwebtoken::{decode, DecodingKey, Validation, Algorithm};
use warp::ws::{Message as WarpMessage, WebSocket};
use uuid::Uuid;
use chrono::{DateTime, Utc};
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Debug, Serialize, Deserialize)]
pub struct Game {
    pub _id: String,                // game ID from URL
    pub white_player: Option<String>, // username
    pub black_player: Option<String>, // username
    pub fen: String,
    pub pgn: String,
    pub status: String,
    pub created_at: String,
    pub updated_at: String,
    pub turn: String,              // "white" or "black"
    pub moves: Vec<String>,        // List of moves in UCI format (e.g., "e2e4")
    pub white_time: i32,           // Time remaining in seconds
    pub black_time: i32,
    pub last_move_time: String,    // ISO timestamp of last move
    pub increment: i32,            // Time increment in seconds
    pub white_time_ms: i64,     // Time in milliseconds
    pub black_time_ms: i64,     // Time in milliseconds
    pub last_move_timestamp: i64, // Unix timestamp in milliseconds
    pub increment_ms: i64,      // Increment in milliseconds
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum ClientMessage {
    JoinGame { 
        game_id: String,
        username: String,
        time_control: i32,
        increment: i32
    },
    Move { 
        game_id: String,
        username: String,
        from: String,
        to: String,
        pgn: String,
        fen: String
    },
    RequestTimeSync {
        game_id: String
    },
    GameOver { 
        game_id: String,
        result: String 
    }
}

#[derive(Debug, Serialize, Deserialize)]
pub enum ServerMessage {
    GameJoined {
        game_id: String,
        color: String,
        opponent: Option<String>, // username of opponent
        fen: String,
        pgn: String,
        turn: String,
        moves: Vec<String>,
        white_time: i32,
        black_time: i32,
        increment: i32
    },
    GameFull {
        message: String
    },
    OpponentJoined {
        username: String
    },
    MoveMade {
        from: String,
        to: String,
        fen: String,
        pgn: String,
        by_username: String
    },
    Error(String),
    TimeUpdate {
        white_time_ms: i64,
        black_time_ms: i64
    },
    GameOver {
        result: String
    }
}

#[derive(Debug)]
pub struct PlayerConnection {
    pub id: String,
    pub game_id: String,
    pub username: String,
    pub color: String,
    pub sender: tokio::sync::mpsc::UnboundedSender<WarpMessage>,
}

pub type Connections = Arc<Mutex<HashMap<String, PlayerConnection>>>;

#[derive(Debug, Serialize, Deserialize)]
struct Claims {
    sub: String,  // user id
    name: Option<String>,
    email: Option<String>,
    // Add other claims as needed
    exp: usize,   // expiration timestamp
}

async fn verify_session(access_token: &str, db: &Database) -> Option<String> {
    // Get JWT secret from environment
    let jwt_secret = std::env::var("NEXTAUTH_SECRET")
        .expect("NEXTAUTH_SECRET must be set");

    // Decode and verify the JWT
    let token = decode::<Claims>(
        access_token,
        &DecodingKey::from_secret(jwt_secret.as_bytes()),
        &Validation::new(Algorithm::HS256)
    ).ok()?;

    // Get username from user ID in JWT claims
    let user_id = token.claims.sub;
    let users = db.collection::<Document>("users");
    let user = users
        .find_one(doc! { "_id": user_id }, None)
        .await
        .ok()??;
    
    user.get_str("username").ok().map(String::from)
}

pub async fn handle_connection(
    ws_stream: WebSocket,
    db: Database,
    connections: Connections
) {
    println!("New WebSocket connection established");

    let (mut ws_sender, mut ws_receiver) = ws_stream.split();
    let (sender, mut receiver) = tokio::sync::mpsc::unbounded_channel();
    let connection_id = Uuid::new_v4().to_string();

    // Spawn a task to handle outgoing messages
    tokio::spawn(async move {
        while let Some(msg) = receiver.recv().await {
            if ws_sender.send(msg).await.is_err() {
                break;
            }
        }
    });

    // Handle incoming WebSocket messages
    while let Some(Ok(msg)) = ws_receiver.next().await {
        println!("Received message: {:?}", msg);
        if let Ok(text) = msg.to_str() {
            println!("Parsed text message: {}", text);
            match serde_json::from_str::<ClientMessage>(text) {
                Ok(client_msg) => {
                    println!("Successfully parsed client message: {:?}", client_msg);
                    match client_msg {
                        ClientMessage::JoinGame { game_id, username, time_control, increment } => {
                            println!("Processing join game request: {} by {}", game_id, username);
                            handle_join_game(
                                &game_id,
                                &username,
                                &sender,
                                time_control,
                                increment,
                                &db,
                                &connections,
                                &connection_id
                            ).await;
                        },
                        ClientMessage::Move { game_id, username, from, to, pgn, fen } => {
                            println!("Before handling move - game_id: {}, username: {}", game_id, username);
                            handle_move(
                                &game_id,
                                &username,
                                from,
                                to,
                                pgn,
                                fen,
                                &db,
                                &connections
                            ).await;
                            println!("After handling move");
                        },
                        ClientMessage::RequestTimeSync { game_id } => {
                            handle_time_sync(
                                &game_id,
                                &db,
                                &connections
                            ).await;
                        },
                        ClientMessage::GameOver { game_id, result } => {
                            handle_game_over(
                                &game_id,
                                result,
                                &db,
                                &connections
                            ).await;
                        }
                    }
                },
                Err(e) => {
                    println!("Failed to parse client message: {}", e); // Better error logging
                }
            }
        }
    }

    // Clean up on disconnect with try_lock
    println!("Connection closed, cleaning up");
    if let Ok(mut conns) = connections.try_lock() {
        conns.retain(|_, conn| conn.id != connection_id);
        println!("Cleaned up connection");
    } else {
        println!("Failed to acquire lock for cleanup");
    }
}

async fn handle_join_game(
    game_id: &str,
    username: &str,
    sender: &tokio::sync::mpsc::UnboundedSender<WarpMessage>,
    time_control: i32,
    increment: i32,
    db: &Database,
    connections: &Connections,
    connection_id: &str
) {
    println!("Starting handle_join_game for: {}", username);
    let games = db.collection::<Game>("games");
    
    if let Ok(Some(mut game)) = games.find_one(doc! { "_id": game_id }, None).await {
        let color = if game.white_player.as_deref() == Some(username) {
            "white"
        } else if game.black_player.as_deref() == Some(username) {
            "black"
        } else if game.white_player.is_none() {
            game.white_player = Some(username.to_string());
            "white"
        } else if game.black_player.is_none() {
            game.black_player = Some(username.to_string());
            "black"
        } else {
            return; // Game is full
        };

        // Store the connection
        let player_conn = PlayerConnection {
            id: connection_id.to_string(),
            game_id: game_id.to_string(),
            username: username.to_string(),
            color: color.to_string(),
            sender: sender.clone(),
        };
        
        // Store connection and check if both players are now connected
        let mut both_players_connected = false;
        {
            println!("Storing connection for player: {}", username);
            let mut retry_count = 0;
            let max_retries = 5;
            
            while retry_count < max_retries {
                match connections.try_lock() {
                    Ok(mut conns) => {
                        conns.insert(username.to_string(), player_conn);
                        // Check if both players are connected
                        both_players_connected = conns.values()
                            .filter(|conn| conn.game_id == game_id)
                            .count() == 2;
                        println!("Successfully stored connection. Both players connected: {}", both_players_connected);
                        break;
                    },
                    Err(_) => {
                        println!("Failed to acquire lock for connection storage, attempt {}", retry_count + 1);
                        retry_count += 1;
                        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
                    }
                }
            }
        }

        // Update game in database
        if color == "white" || color == "black" {
            let mut update = doc! {
                format!("{}_player", color): username,
                "updated_at": chrono::Utc::now().to_rfc3339()
            };

            // Start the clock only when both players are connected
            if both_players_connected {
                update.insert("last_move_time", chrono::Utc::now().to_rfc3339());
                update.insert("status", "active");
                println!("Starting game clock as both players are connected");
            }

            games.update_one(
                doc! { "_id": game_id },
                doc! { "$set": update },
                None
            ).await.ok();
        }

        send_game_state(color, &game, username, sender);
        notify_opponent(&game, username, connections).await;
    } else {
        // Create new game with proper time initialization
        let time_control_ms = (time_control as i64) * 1000; // Convert seconds to milliseconds
        let increment_ms = (increment as i64) * 1000;       // Convert seconds to milliseconds
        
        let new_game = Game {
            _id: game_id.to_string(),
            white_player: Some(username.to_string()),
            black_player: None,
            fen: "rnbqkbnr/pppppppp/8/8/8/8/PPPPPPPP/RNBQKBNR w KQkq - 0 1".to_string(),
            pgn: String::new(),
            status: "waiting".to_string(),
            created_at: chrono::Utc::now().to_rfc3339(),
            updated_at: chrono::Utc::now().to_rfc3339(),
            turn: "white".to_string(),
            moves: Vec::new(),
            white_time: time_control,
            black_time: time_control,
            last_move_time: chrono::Utc::now().to_rfc3339(),
            increment,
            white_time_ms: time_control_ms,     // Initialize with full time
            black_time_ms: time_control_ms,     // Initialize with full time
            last_move_timestamp: current_timestamp_ms(),
            increment_ms
        };
        
        // Store the connection for new game
        let player_conn = PlayerConnection {
            id: connection_id.to_string(),
            game_id: game_id.to_string(),
            username: username.to_string(),
            color: "white".to_string(),
            sender: sender.clone(),
        };
        
        println!("Storing connection for new player: {}", username);
        // Use try_lock with timeout
        let mut retry_count = 0;
        let max_retries = 5;
        
        while retry_count < max_retries {
            match connections.try_lock() {
                Ok(mut conns) => {
                    conns.insert(username.to_string(), player_conn);
                    println!("Successfully stored connection");
                    break;
                },
                Err(_) => {
                    println!("Failed to acquire lock for connection storage, attempt {}", retry_count + 1);
                    retry_count += 1;
                    tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
                }
            }
        }

        if let Ok(_) = games.insert_one(&new_game, None).await {
            send_game_state("white", &new_game, username, sender);
        }
    }
}

fn send_game_state(
    color: &str,
    game: &Game,
    username: &str,
    sender: &tokio::sync::mpsc::UnboundedSender<WarpMessage>
) {
    let opponent = match color {
        "white" => game.black_player.clone(),
        "black" => game.white_player.clone(),
        _ => None
    };

    let msg = ServerMessage::GameJoined {
        game_id: game._id.clone(),
        color: color.to_string(),
        opponent,
        fen: game.fen.clone(),
        pgn: game.pgn.clone(),
        turn: game.turn.clone(),
        moves: game.moves.clone(),
        white_time: game.white_time,
        black_time: game.black_time,
        increment: game.increment
    };
    
    sender.send(WarpMessage::text(serde_json::to_string(&msg).unwrap())).ok();
}

async fn notify_opponent(
    game: &Game,
    username: &str,
    connections: &Connections
) {
    let msg = ServerMessage::OpponentJoined {
        username: username.to_string()
    };
    let msg_str = serde_json::to_string(&msg).unwrap();
    
    let mut retry_count = 0;
    let max_retries = 5;
    
    while retry_count < max_retries {
        match connections.try_lock() {
            Ok(conns) => {
                for conn in conns.values() {
                    if conn.game_id == game._id && conn.username != username {
                        conn.sender.send(WarpMessage::text(msg_str.clone())).ok();
                    }
                }
                return;
            },
            Err(_) => {
                println!("Failed to acquire lock for opponent notification, attempt {}", retry_count + 1);
                retry_count += 1;
                tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
            }
        }
    }
    println!("Failed to notify opponent after {} attempts", max_retries);
}

async fn handle_move(
    game_id: &str,
    username: &str,
    from: String,
    to: String,
    pgn: String,
    fen: String,
    db: &Database,
    connections: &Connections
) {
    let games = db.collection::<Game>("games");
    
    match games.find_one(doc! { "_id": game_id }, None).await {
        Ok(Some(mut game)) => {
            if game.status != "active" {
                return;
            }

            let is_white = game.white_player.as_ref().map(|s| s.as_str()) == Some(username);
            let is_black = game.black_player.as_ref().map(|s| s.as_str()) == Some(username);
            let is_correct_turn = (is_white && game.turn == "white") || (is_black && game.turn == "black");
            
            if !is_correct_turn {
                return;
            }

            // Calculate elapsed time with millisecond precision
            let now = current_timestamp_ms();
            let elapsed_ms = now - game.last_move_timestamp;

            // Update times with millisecond precision
            if game.turn == "white" {
                game.white_time_ms = (game.white_time_ms - elapsed_ms).max(0);
                if game.moves.len() > 1 {
                    game.white_time_ms += game.increment_ms;
                }
            } else {
                game.black_time_ms = (game.black_time_ms - elapsed_ms).max(0);
                if game.moves.len() > 1 {
                    game.black_time_ms += game.increment_ms;
                }
            }

            // Update game state
            game.moves.push(format!("{}{}", from, to));
            game.fen = fen.clone();
            game.pgn = pgn.clone();
            game.turn = if game.turn == "white" { "black".to_string() } else { "white".to_string() };
            game.last_move_timestamp = now;

            // Update in database
            games.update_one(
                doc! { "_id": game_id },
                doc! { 
                    "$set": {
                        "fen": &game.fen,
                        "pgn": &game.pgn,
                        "turn": &game.turn,
                        "moves": &game.moves,
                        "white_time_ms": game.white_time_ms,
                        "black_time_ms": game.black_time_ms,
                        "last_move_timestamp": game.last_move_timestamp
                    }
                },
                None
            ).await.ok();

            // Notify players with millisecond precision
            notify_move(
                game_id,
                &from,
                &to,
                &fen,
                &pgn,
                username,
                &game.white_time_ms,
                &game.black_time_ms,
                connections
            ).await;
        },
        Ok(None) => println!("Game not found: {}", game_id),
        Err(e) => println!("Database error: {}", e)
    }
}

async fn notify_move(
    game_id: &str,
    from: &str,
    to: &str,
    fen: &str,
    pgn: &str,
    by_username: &str,
    white_time: &i64,
    black_time: &i64,
    connections: &Connections
) {
    println!("Starting notify_move for game: {}", game_id);
    
    let move_msg = ServerMessage::MoveMade {
        from: from.to_string(),
        to: to.to_string(),
        fen: fen.to_string(),
        pgn: pgn.to_string(),
        by_username: by_username.to_string()
    };
    
    let time_msg = ServerMessage::TimeUpdate {
        white_time_ms: *white_time,
        black_time_ms: *black_time
    };

    println!("Preparing messages");
    let move_str = serde_json::to_string(&move_msg).unwrap();
    let time_str = serde_json::to_string(&time_msg).unwrap();

    // Try to get the lock with timeout
    let mut retry_count = 0;
    let max_retries = 5;
    
    while retry_count < max_retries {
        println!("Attempting to acquire lock (attempt {})", retry_count + 1);
        
        match connections.try_lock() {
            Ok(conns) => {
                println!("Lock acquired successfully");
                let recipients: Vec<_> = conns.values()
                    .filter(|conn| conn.game_id == game_id)
                    .map(|conn| (conn.username.clone(), conn.sender.clone()))
                    .collect();
                
                // Lock is automatically released here
                
                // Send messages without holding the lock
                for (username, sender) in recipients {
                    println!("Sending messages to {}", username);
                    if let Err(e) = sender.send(WarpMessage::text(move_str.clone())) {
                        println!("Failed to send move message to {}: {}", username, e);
                    }
                    if let Err(e) = sender.send(WarpMessage::text(time_str.clone())) {
                        println!("Failed to send time message to {}: {}", username, e);
                    }
                }
                
                println!("Finished notify_move for game: {}", game_id);
                return;
            },
            Err(_) => {
                println!("Failed to acquire lock, retrying...");
                retry_count += 1;
                tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
            }
        }
    }
    
    println!("Failed to acquire lock after {} attempts", max_retries);
}

async fn handle_game_over(
    game_id: &str,
    result: String,
    db: &Database,
    connections: &Connections
) {
    let games = db.collection::<Game>("games");
    
    // Update game status in database
    games.update_one(
        doc! { "_id": game_id },
        doc! { 
            "$set": {
                "status": "completed",
                "result": &result,
                "updated_at": chrono::Utc::now().to_string()
            }
        },
        None
    ).await.ok();

    // Notify all players in the game
    let conns = connections.lock().await;
    for conn in conns.values() {
        if conn.game_id == game_id {
            // You might want to create a new ServerMessage variant for game over
            let msg = ServerMessage::GameOver {
                result: result.clone()
            };
            conn.sender.send(WarpMessage::text(serde_json::to_string(&msg).unwrap())).ok();
        }
    }
}

async fn handle_time_sync(
    game_id: &str,
    db: &Database,
    connections: &Connections
) {
    let games = db.collection::<Game>("games");
    
    if let Ok(Some(game)) = games.find_one(doc! { "_id": game_id }, None).await {
        if game.status != "active" {
            return;
        }

        let now = current_timestamp_ms();
        let elapsed_ms = now - game.last_move_timestamp;

        let (white_time_ms, black_time_ms) = if game.turn == "white" {
            ((game.white_time_ms - elapsed_ms).max(0), game.black_time_ms)
        } else {
            (game.white_time_ms, (game.black_time_ms - elapsed_ms).max(0))
        };

        let time_msg = ServerMessage::TimeUpdate {
            white_time_ms,
            black_time_ms
        };

        if let Ok(conns) = connections.try_lock() {
            for conn in conns.values() {
                if conn.game_id == game_id {
                    conn.sender.send(WarpMessage::text(
                        serde_json::to_string(&time_msg).unwrap()
                    )).ok();
                }
            }
        }
    }
}

// Helper function to get current timestamp in milliseconds
fn current_timestamp_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64
}

// ... other handler functions remain similar but use MongoDB instead