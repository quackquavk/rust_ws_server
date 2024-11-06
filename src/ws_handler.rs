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
    pub result: String,         // Added this field for game result
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
    },
    Resign { 
        game_id: String,
        username: String 
    },
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
    Resign { 
        game_id: String,
        username: String 
    },
    GameFull {
        message: String
    },
    OpponentJoined {
        username: String
    },
    GameResigned {
        username: String,  // who resigned
        winner: String    // opponent's username
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
    },
    GameCompleted {
        game_id: String,
        white_player: Option<String>,
        black_player: Option<String>,
        fen: String,
        pgn: String,
        moves: Vec<String>,
        result: String,           // e.g., "1-0", "0-1", "1/2-1/2"
        status: String,           // "completed"
        winner: Option<String>,   // username of winner, None for draw
        reason: String,           // e.g., "resignation", "checkmate", "stalemate", "time"
        time_control: i32,        // initial time in seconds
        increment: i32,           // increment in seconds
        white_time_left: i64,     // remaining time in ms
        black_time_left: i64,     // remaining time in ms
    },
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
                        },
                        ClientMessage::Resign { game_id, username } => {
                            handle_resign(
                                &game_id,
                                &username,
                                &db,
                                &connections
                            ).await;
                        },
                    }
                },
                Err(e) => {
                    println!("Failed to parse client message: {}", e); // Better error logging
                }
            }
        }
    }

    // Clean up only the WebSocket connection on disconnect, not the player assignment
    println!("Connection closed, cleaning up WebSocket connection");
    if let Ok(mut conns) = connections.try_lock() {
        conns.retain(|_, conn| conn.id != connection_id);
        println!("Cleaned up WebSocket connection for ID: {}", connection_id);
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
    
    // First check if game exists
    let existing_game = games.find_one(doc! { "_id": game_id }, None).await.ok().flatten();
    
    match existing_game {
        Some(game) => {
            // Handle existing game
            println!("Found existing game with status: {}", game.status);
            
            match game.status.as_str() {
                "completed" => {
                    send_completed_game(&game, sender).await;
                    return;
                },
                "active" | "waiting" => {
                    // Check if player is part of the game
                    let is_white = game.white_player.as_deref() == Some(username);
                    let is_black = game.black_player.as_deref() == Some(username);
                    
                    if !is_white && !is_black {
                        // New player trying to join
                        if game.white_player.is_some() && game.black_player.is_some() {
                            let msg = ServerMessage::GameFull {
                                message: "Game is already full".to_string()
                            };
                            sender.send(WarpMessage::text(serde_json::to_string(&msg).unwrap())).ok();
                            return;
                        }
                    }

                    // Determine player's color from game data
                    let color = if is_white {
                        "white"
                    } else if is_black {
                        "black"
                    } else if game.white_player.is_none() {
                        "white"
                    } else {
                        "black"
                    };

                    // Store the connection
                    let player_conn = PlayerConnection {
                        id: connection_id.to_string(),
                        game_id: game_id.to_string(),
                        username: username.to_string(),
                        color: color.to_string(),
                        sender: sender.clone(),
                    };

                    // Update only connections, not player assignments
                    {
                        if let Ok(mut conns) = connections.try_lock() {
                            // Remove any existing connection for this username
                            conns.retain(|_, conn| conn.username != username);
                            conns.insert(username.to_string(), player_conn);
                        }
                    }

                    // Only update database for new players, not reconnections
                    if !is_white && !is_black {
                        let mut update = doc! {
                            format!("{}_player", color): username,
                            "updated_at": chrono::Utc::now().to_rfc3339()
                        };

                        // Check if this completes the game setup
                        let both_players_assigned = match color {
                            "white" => game.black_player.is_some(),
                            "black" => game.white_player.is_some(),
                            _ => false
                        };

                        if both_players_assigned && game.status == "waiting" {
                            update.insert("status", "active");
                            update.insert("last_move_time", chrono::Utc::now().to_rfc3339());
                            update.insert("last_move_timestamp", current_timestamp_ms());
                        }

                        games.update_one(
                            doc! { "_id": game_id },
                            doc! { "$set": update },
                            None
                        ).await.ok();
                    }

                    // Send game state and notify opponent
                    send_game_state(color, &game, username, sender);
                    notify_opponent(&game, username, connections).await;
                },
                _ => {
                    println!("Invalid game status: {}", game.status);
                    return;
                }
            }
        },
        None => {
            // Handle new game creation
            if time_control <= 0 || increment < 0 {
                let msg = ServerMessage::Error("Invalid time controls".to_string());
                sender.send(WarpMessage::text(serde_json::to_string(&msg).unwrap())).ok();
                return;
            }

            let time_control_ms = (time_control as i64) * 1000;
            let increment_ms = (increment as i64) * 1000;
            
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
                white_time_ms: time_control_ms,
                black_time_ms: time_control_ms,
                last_move_timestamp: current_timestamp_ms(),
                increment_ms,
                result: String::new(),
            };

            // Create new game in database
            if let Ok(_) = games.insert_one(&new_game, None).await {
                let player_conn = PlayerConnection {
                    id: connection_id.to_string(),
                    game_id: game_id.to_string(),
                    username: username.to_string(),
                    color: "white".to_string(),
                    sender: sender.clone(),
                };

                if let Ok(mut conns) = connections.try_lock() {
                    conns.insert(username.to_string(), player_conn);
                }

                send_game_state("white", &new_game, username, sender);
            }
        }
    }
}

fn send_game_state(
    color: &str,
    game: &Game,
    username: &str,
    sender: &tokio::sync::mpsc::UnboundedSender<WarpMessage>
) {
    // Get opponent from game data, not from connections
    let opponent = match color {
        "white" => game.black_player.clone(),
        "black" => game.white_player.clone(),
        _ => None
    };

    let msg = ServerMessage::GameJoined {
        game_id: game._id.clone(),
        color: color.to_string(),
        opponent,  // Use database information
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
    
    // Determine opponent from game data
    let opponent_username = if game.white_player.as_deref() == Some(username) {
        game.black_player.as_ref()
    } else {
        game.white_player.as_ref()
    };

    if let Some(opponent) = opponent_username {
        if let Ok(conns) = connections.try_lock() {
            // Only send notification if opponent is connected
            if let Some(conn) = conns.get(opponent) {
                conn.sender.send(WarpMessage::text(msg_str)).ok();
            }
        }
    }
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
    
    if let Ok(Some(game)) = games.find_one(doc! { "_id": game_id }, None).await {
        // Update game status in database
        games.update_one(
            doc! { "_id": game_id },
            doc! { 
                "$set": {
                    "status": "completed",
                    "result": &result,
                    "updated_at": chrono::Utc::now().to_rfc3339()
                }
            },
            None
        ).await.ok();

        // Get updated game document
        if let Ok(Some(updated_game)) = games.find_one(doc! { "_id": game_id }, None).await {
            // Notify all players in the game
            if let Ok(conns) = connections.try_lock() {
                for conn in conns.values() {
                    if conn.game_id == game_id {
                        send_completed_game(&updated_game, &conn.sender).await;
                    }
                }
            }
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

async fn handle_resign(
    game_id: &str,
    username: &str,
    db: &Database,
    connections: &Connections
) {
    let games = db.collection::<Game>("games");
    
    if let Ok(Some(game)) = games.find_one(doc! { "_id": game_id }, None).await {
        // Determine winner (opponent of the resigning player)
        let winner = if game.white_player.as_deref() == Some(username) {
            game.black_player.clone()
        } else {
            game.white_player.clone()
        };

        let result = format!("{} resigned", username);
        
        // Update game in database
        games.update_one(
            doc! { "_id": game_id },
            doc! { 
                "$set": {
                    "status": "completed",
                    "result": &result,
                    "updated_at": chrono::Utc::now().to_rfc3339()
                }
            },
            None
        ).await.ok();

        // First send GameResigned message
        let resign_msg = ServerMessage::GameResigned {
            username: username.to_string(),
            winner: winner.unwrap_or_default()
        };

        // Then get updated game and send GameCompleted
        if let Ok(Some(updated_game)) = games.find_one(doc! { "_id": game_id }, None).await {
            if let Ok(conns) = connections.try_lock() {
                for conn in conns.values() {
                    if conn.game_id == game_id {
                        // Send both messages
                        conn.sender.send(WarpMessage::text(
                            serde_json::to_string(&resign_msg).unwrap()
                        )).ok();
                        
                        send_completed_game(&updated_game, &conn.sender).await;
                    }
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

// Add this function to handle time-out
async fn check_time_out(game: &Game, db: &Database, connections: &Connections) {
    if game.status != "active" {
        return;
    }

    let now = current_timestamp_ms();
    let elapsed_ms = now - game.last_move_timestamp;
    
    let (white_time_remaining, black_time_remaining) = if game.turn == "white" {
        (game.white_time_ms - elapsed_ms, game.black_time_ms)
    } else {
        (game.white_time_ms, game.black_time_ms - elapsed_ms)
    };

    if white_time_remaining <= 0 || black_time_remaining <= 0 {
        let result = if white_time_remaining <= 0 {
            "Black wins on time"
        } else {
            "White wins on time"
        };

        let games = db.collection::<Game>("games");
        games.update_one(
            doc! { "_id": &game._id },
            doc! { 
                "$set": {
                    "status": "completed",
                    "result": result,
                    "updated_at": chrono::Utc::now().to_rfc3339(),
                    "white_time_ms": white_time_remaining.max(0),
                    "black_time_ms": black_time_remaining.max(0)
                }
            },
            None
        ).await.ok();

        // Get updated game document and send to players
        if let Ok(Some(updated_game)) = games.find_one(doc! { "_id": &game._id }, None).await {
            if let Ok(conns) = connections.try_lock() {
                for conn in conns.values() {
                    if conn.game_id == game._id {
                        send_completed_game(&updated_game, &conn.sender).await;
                    }
                }
            }
        }
    }
}

// Helper function to determine winner and standardize result format
fn get_game_result_info(result_str: &str, white_player: &Option<String>, black_player: &Option<String>) 
    -> (String, Option<String>) { // Returns (standardized_result, winner)
    
    if result_str.contains("resigned") {
        // Check who resigned
        if let Some(username) = result_str.split_whitespace().next() {
            let is_white_resigned = white_player.as_deref() == Some(username);
            if is_white_resigned {
                return ("0-1".to_string(), black_player.clone())
            } else {
                return ("1-0".to_string(), white_player.clone())
            }
        }
    } else if result_str.contains("time") {
        if result_str.contains("White wins") {
            return ("1-0".to_string(), white_player.clone())
        } else if result_str.contains("Black wins") {
            return ("0-1".to_string(), black_player.clone())
        }
    } else if result_str.contains("checkmate") {
        if result_str.contains("White wins") {
            return ("1-0".to_string(), white_player.clone())
        } else if result_str.contains("Black wins") {
            return ("0-1".to_string(), black_player.clone())
        }
    } else if result_str.contains("stalemate") || result_str.contains("draw") {
        return ("1/2-1/2".to_string(), None)
    }
    
    ("*".to_string(), None) // Default for unknown result
}

// Add this helper function to construct a complete PGN
fn construct_complete_pgn(
    white_player: &Option<String>,
    black_player: &Option<String>,
    result: &str,
    base_pgn: &str,
    time_control: i32,
    increment: i32,
) -> String {
    let date = chrono::Utc::now().format("%Y.%m.%d");
    let time_control_str = format!("{}+{}", time_control, increment);
    
    let mut pgn = String::new();
    
    // Add all the standard PGN tags
    pgn.push_str(&format!("[Event \"Casual Game\"]\n"));
    pgn.push_str(&format!("[Site \"chessdream.vercel.app\"]\n"));
    pgn.push_str(&format!("[Date \"{}\"]\n", date));
    pgn.push_str(&format!("[White \"{}\"]\n", white_player.as_deref().unwrap_or("?")));
    pgn.push_str(&format!("[Black \"{}\"]\n", black_player.as_deref().unwrap_or("?")));
    pgn.push_str(&format!("[Result \"{}\"]\n", result));
    pgn.push_str(&format!("[WhiteElo \"1200\"]\n"));
    pgn.push_str(&format!("[BlackElo \"1200\"]\n"));
    pgn.push_str(&format!("[TimeControl \"{}\"]\n", time_control_str));
    pgn.push_str(&format!("[Variant \"Standard\"]\n"));
    pgn.push_str("\n");  // Empty line between tags and moves
    pgn.push_str(base_pgn);
    pgn.push_str(&format!(" {}", result));  // Append result at the end

    pgn
}

// Update send_completed_game to use the new PGN construction
async fn send_completed_game(game: &Game, sender: &tokio::sync::mpsc::UnboundedSender<WarpMessage>) {
    let (standardized_result, winner) = get_game_result_info(
        &game.result, 
        &game.white_player, 
        &game.black_player
    );
    
    let reason = if game.result.contains("resigned") {
        "resignation"
    } else if game.result.contains("time") {
        "time"
    } else if game.result.contains("checkmate") {
        "checkmate"
    } else if game.result.contains("stalemate") {
        "stalemate"
    } else if game.result.contains("draw") {
        "draw"
    } else {
        "unknown"
    };

    // Construct complete PGN with metadata
    let complete_pgn = construct_complete_pgn(
        &game.white_player,
        &game.black_player,
        &standardized_result,
        &game.pgn,
        game.white_time,  // initial time control
        game.increment
    );

    let msg = ServerMessage::GameCompleted {
        game_id: game._id.clone(),
        white_player: game.white_player.clone(),
        black_player: game.black_player.clone(),
        fen: game.fen.clone(),
        pgn: complete_pgn,  // Use the complete PGN instead of base moves
        moves: game.moves.clone(),
        result: standardized_result,
        status: game.status.clone(),
        winner,
        reason: reason.to_string(),
        time_control: game.white_time,
        increment: game.increment,
        white_time_left: game.white_time_ms,
        black_time_left: game.black_time_ms,
    };

    sender.send(WarpMessage::text(serde_json::to_string(&msg).unwrap())).ok();
}

// ... other handler functions remain similar but use MongoDB instead