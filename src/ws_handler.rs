use mongodb::{Client, Database, Collection, bson::{self, doc, Document}};
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
use shakmaty::{Chess, Position, Move as ChessMove, Square, Role, Color, Setup, CastlingMode, FromSetup, PositionError};
use shakmaty::fen::Fen;
use std::str::FromStr;
use rand::Rng;
use base32::{Alphabet, encode};
use futures::TryStreamExt;
use lazy_static::lazy_static;

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
    pub draw_offered_by: Option<String>,  // Username of player who offered draw
    pub reason: Option<String>,  // Add this field
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
        fen: String,
        timestamp: i64
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
    OfferDraw {
        game_id: String,
        username: String
    },
    AcceptDraw {
        game_id: String,
        username: String
    },
    DeclineDraw {
        game_id: String,
        username: String
    },
    ChatMessage {
        game_id: String,
        username: String,
        content: String,
        recipient: Option<String>,
    },
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum ServerMessage {
    GameNotFound{
        message: String
    },
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
        by_username: String,
        turn: String,              // Added field
        white_time_ms: i64,        // Added field
        black_time_ms: i64         // Added field
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
        reason: String,           // e.g., "resignation", "checkmate", "stalemate", "time", "abandonment"
        time_control: i32,        // initial time in seconds
        increment: i32,           // increment in seconds
        white_time_left: i64,     // remaining time in ms
        black_time_left: i64,     // remaining time in ms
    },
    DrawOffered {
        by_username: String
    },
    DrawDeclined {
        by_username: String
    },
    ChatMessageReceived {
        id: String,
        game_id: String,
        sender: String,
        content: String,
        #[serde(with = "mongodb::bson::serde_helpers::chrono_datetime_as_bson_datetime")]
        timestamp: DateTime<Utc>,
        is_private: bool,
        recipient: Option<String>,
    },
    ChatHistory {
        messages: Vec<ChatMessage>,
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

// At the top of the file, add this new enum for game completion reasons
#[derive(Debug)]
pub enum GameEndReason {
    Abandonment(String),  // String contains the username who abandoned
    Timeout(String),      // String contains the player who ran out of time
    Resignation(String),  // String contains the player who resigned
    Checkmate,
    Stalemate,
    Draw,
}

// Add this at the top with other structs
#[derive(Debug, Clone)]
struct DisconnectionInfo {
    username: String,
    game_id: String,
    disconnect_time: i64,
    reconnect_window: i64, // milliseconds
}

// Add this as a static/global variable
lazy_static::lazy_static! {
    static ref DISCONNECTED_PLAYERS: Arc<Mutex<HashMap<String, DisconnectionInfo>>> = 
        Arc::new(Mutex::new(HashMap::new()));
}

// Update handle_connection function
pub async fn handle_connection(
    ws_stream: WebSocket,
    db: Database,
    connections: Connections
) {
    
    // Better connection logging
    
    let (mut ws_sender, mut ws_receiver) = ws_stream.split();
    
    let (sender, mut receiver) = tokio::sync::mpsc::unbounded_channel();
    let connection_id = Uuid::new_v4().to_string();
    let mut player_info: Option<(String, String)> = None;

    // Send initial handshake message with more detailed error handling
    let handshake_msg = ServerMessage::Error("connection_established".to_string());
    if let Ok(msg_str) = serde_json::to_string(&handshake_msg) {
        match ws_sender.send(WarpMessage::text(msg_str)).await {
            Ok(_)=> {
                println!("🤝 Handshake message sent successfully");
            },
            Err(e) => {
                println!("🔍 Error details: {:?}", e);
            }
        }
    } else {
        println!("❌ Failed to serialize handshake message");
    }

    // Spawn task for sending messages
    let ws_sender_task = tokio::spawn(async move {
        while let Some(msg) = receiver.recv().await {
            match ws_sender.send(msg).await {
                Ok(_) => println!("📤 Message sent successfully"),
                Err(e) => {
                    break;
                }
            }
        }
    });

    // Handle incoming WebSocket messages
    while let Some(result) = ws_receiver.next().await {
        match result {
            Ok(msg) => {
                if let Ok(text) = msg.to_str() {
                    println!("📥 Received message: {}", text);
                    match serde_json::from_str::<ClientMessage>(text) {
                        Ok(ClientMessage::JoinGame { game_id, username, time_control, increment }) => {
                            println!("👤 Join game request from {}", username);
                            player_info = Some((game_id.clone(), username.clone()));
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
                            handle_time_sync(
                                &game_id,
                                &db,
                                &connections
                            ).await;
                        },
                        Ok(ClientMessage::Move { game_id, username, from, to, pgn, fen, timestamp }) => {
                            handle_move(
                                &game_id,
                                &username,
                                &from,
                                &to,
                                None,
                                &pgn,
                                &fen,
                                timestamp,
                                &db,
                                &connections
                            ).await;
                            handle_time_sync(
                                &game_id,
                                &db,
                                &connections
                            ).await;
                        },
                        Ok(ClientMessage::RequestTimeSync { game_id }) => {
                            handle_time_sync(
                                &game_id,
                                &db,
                                &connections
                            ).await;
                        },
                        Ok(ClientMessage::GameOver { game_id, result }) => {
                            handle_game_over(
                                &game_id,
                                result,
                                &db,
                                &connections
                            ).await;
                        },
                        Ok(ClientMessage::Resign { game_id, username }) => {
                            handle_resign(
                                &game_id,
                                &username,
                                &db,
                                &connections
                            ).await;
                        },
                        Ok(ClientMessage::OfferDraw { game_id, username }) => {
                            handle_draw_offer(&game_id, &username, &db, &connections).await;
                        },
                        Ok(ClientMessage::AcceptDraw { game_id, username }) => {
                            handle_draw_accept(&game_id, &username, &db, &connections).await;
                        },
                        Ok(ClientMessage::DeclineDraw { game_id, username }) => {
                            handle_draw_decline(&game_id, &username, &db, &connections).await;
                        },
                        Ok(ClientMessage::ChatMessage { game_id, username, content, recipient }) => {
                            handle_chat_message(
                                &game_id,
                                &username,
                                &content,
                                &recipient,
                                &db,
                                &connections
                            ).await;
                        },
                        Err(e) => println!("❌ Failed to parse client message: {}", e)
                    }
                }
            },
            Err(e) => {
                break;
            }
        }
    }

    // Handle disconnection
    if let Some((game_id, username)) = player_info {
        println!("👋 Player {} disconnected from game {}", username, game_id);
        handle_player_disconnection(&game_id, &username, &db, &connections).await;
    }

    // Clean up connection
    if let Ok(mut conns) = connections.try_lock() {
        conns.retain(|_, conn| conn.id != connection_id);
        println!("🧹 Cleaned up connection {}", connection_id);
    }

    // Ensure sender task is terminated
    ws_sender_task.abort();
    println!("🛑 Connection handler completed");
}

// Update handle_player_disconnection function
async fn handle_player_disconnection(
    game_id: &str,
    username: &str,
    db: &Database,
    connections: &Connections
) {
    println!("🔌 Player disconnection detected - Game: {}, User: {}", game_id, username);
    
    // Check if player is already in disconnected state
    let should_handle = {
        let mut disconnected = DISCONNECTED_PLAYERS.lock().await;
        if !disconnected.contains_key(username) {
            // Add player to disconnected list with 15 second window
            disconnected.insert(username.to_string(), DisconnectionInfo {
                username: username.to_string(),
                game_id: game_id.to_string(),
                disconnect_time: current_timestamp_ms(),
                reconnect_window: 15000, // 15 seconds in milliseconds
            });
            
            // Spawn a task to handle abandonment after timeout
            let username = username.to_string();
            let game_id = game_id.to_string();
            let db = db.clone();
            let connections = connections.clone();
            
            tokio::spawn(async move {
                tokio::time::sleep(tokio::time::Duration::from_secs(15)).await;
                
                // Check if player has reconnected
                let should_abandon = {
                    let mut disconnected = DISCONNECTED_PLAYERS.lock().await;
                    match disconnected.get(&username) {
                        Some(info) if info.game_id == game_id => {
                            // Only remove and abandon if this is the same disconnection instance
                            disconnected.remove(&username);
                            true
                        }
                        Some(_) => false,  // Different game, don't abandon
                        None => false,     // Already reconnected
                    }
                };

                if should_abandon {
                    println!("⏰ Abandonment timer expired for player {} in game {}", username, game_id);
                    handle_abandonment(&game_id, &username, &db, &connections).await;
                } else {
                    println!("✅ Player {} reconnected or playing different game", username);
                }
            });
            
            false // Don't handle abandonment immediately
        } else {
            false // Player already in disconnected state
        }
    };

    if should_handle {
        handle_abandonment(game_id, username, db, connections).await;
    }
}

// Add this new function to handle the actual abandonment
async fn handle_abandonment(
    game_id: &str,
    username: &str,
    db: &Database,
    connections: &Connections
) {
    let games = db.collection::<Game>("games");
    
    // First check if game is still active
    match games.find_one(doc! { 
        "_id": game_id,
        "status": "active"
    }, None).await {
        Ok(Some(game)) => {
            // Determine winner (opponent of disconnected player)
            let (winner, result) = if game.white_player.as_deref() == Some(username) {
                (game.black_player.clone(), "0-1")
            } else {
                println!("⚫ Black player disconnected, White wins");
                (game.white_player.clone(), "1-0")
            };

            println!("🏆 Winner determined: {:?}", winner);

            // Create abandonment message
            let result_message = format!("{} abandoned", username);

            // Update game status with winner information
            let update_doc = doc! { 
                "$set": {
                    "status": "completed",
                    "result": &result_message,
                    "reason": "abandonment",
                    "winner": &winner,
                    "updated_at": chrono::Utc::now().to_rfc3339(),
                }
            };

            // Update the document
            match games.update_one(
                doc! { 
                    "_id": game_id,
                    "status": "active"
                },
                update_doc.clone(),
                None
            ).await {
                Ok(update_result) => {
                    if update_result.modified_count > 0 {
                        if let Ok(Some(updated_game)) = games.find_one(
                            doc! { "_id": game_id }, 
                            None
                        ).await {
                            if let Ok(conns) = connections.try_lock() {
                                for conn in conns.values() {
                                    if conn.game_id == game_id {
                                        println!("📨 Sending game completion to player: {}", conn.username);
                                        send_completed_game(&updated_game, &conn.sender).await;
                                    }
                                }
                            }
                        }
                    }
                },
                Err(e) => println!("❌ Error updating game: {}", e),
            }
        },
        Ok(None) => println!("❌ No active game found with ID: {}", game_id),
        Err(e) => println!("❌ Error finding game: {}", e),
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
    println!("👋 Player {} attempting to join game {}", username, game_id);
    
    // First check if this is a reconnection
    {
        let mut disconnected = DISCONNECTED_PLAYERS.lock().await;
        if let Some(info) = disconnected.get(username) {
            if info.game_id == game_id {
                println!("🔄 Player {} reconnecting to game {}", username, game_id);
                disconnected.remove(username);  // Remove from disconnected list
            }
        }
    }

    let games = db.collection::<Game>("games");
   
    // First check if game exists
    match games.find_one(doc! { "_id": game_id }, None).await {
        Ok(Some(game)) => {
            println!("🎮 Found game: {:?}", game);  // Debug log
            
            match game.status.as_str() {
                "completed" => {
                    println!("❌ Game {} is already completed", game_id);
                    send_completed_game(&game, sender).await;
                    return;
                },
                "active" | "waiting" => {
                    // Check if player is already in the game
                    let is_white = game.white_player.as_deref() == Some(username);
                    let is_black = game.black_player.as_deref() == Some(username);
                    
                    if game.status == "active" && !is_white && !is_black {
                        println!("🚫 Rejecting join attempt: Game {} is active and player {} is not a participant", game_id, username);
                        let msg = ServerMessage::GameFull {
                            message: "This game is already in progress".to_string()
                        };
                        sender.send(WarpMessage::text(serde_json::to_string(&msg).unwrap())).ok();
                        return;
                    }

                    // Handle joining for waiting game
                    if game.status == "waiting" {
                        let (color, opponent) = if game.white_player.is_none() {
                            ("white", game.black_player.clone())
                        } else if game.black_player.is_none() {
                            ("black", game.white_player.clone())
                        } else {
                            println!("🚫 Game is full");
                            let msg = ServerMessage::GameFull {
                                message: "Game is already full".to_string()
                            };
                            sender.send(WarpMessage::text(serde_json::to_string(&msg).unwrap())).ok();
                            return;
                        };

                        // Update game with new player
                        let update = match color {
                            "white" => doc! { 
                                "$set": {
                                    "white_player": username,
                                    "status": if game.black_player.is_some() { "active" } else { "waiting" },
                                    "updated_at": chrono::Utc::now().to_rfc3339()
                                }
                            },
                            "black" => doc! { 
                                "$set": {
                                    "black_player": username,
                                    "status": if game.white_player.is_some() { "active" } else { "waiting" },
                                    "updated_at": chrono::Utc::now().to_rfc3339()
                                }
                            },
                            _ => unreachable!()
                        };

                        if let Ok(_) = games.update_one(doc! { "_id": game_id }, update, None).await {
                            // Store the connection
                            let player_conn = PlayerConnection {
                                id: connection_id.to_string(),
                                game_id: game_id.to_string(),
                                username: username.to_string(),
                                color: color.to_string(),
                                sender: sender.clone(),
                            };

                            if let Ok(mut conns) = connections.try_lock() {
                                conns.insert(username.to_string(), player_conn);
                            }

                            // Get updated game state
                            if let Ok(Some(updated_game)) = games.find_one(doc! { "_id": game_id }, None).await {
                                send_game_state(color, &updated_game, username, sender);
                                notify_opponent(&updated_game, username, connections).await;

                                // If this was the second player joining (game becomes active)
                                if updated_game.status == "active" {
                                    // Start the game timer after a delay
                                    let game_id = game_id.to_string();
                                    let db = db.clone();
                                    let connections = connections.clone();
                                    tokio::spawn(async move {
                                        start_game_timer(&game_id, &db, &connections).await;
                                    });
                                }
                            }
                        }
                    } else {
                        // Reconnection to active game
                        let color = if is_white { "white" } else { "black" };
                        
                        // Store the connection
                        let player_conn = PlayerConnection {
                            id: connection_id.to_string(),
                            game_id: game_id.to_string(),
                            username: username.to_string(),
                            color: color.to_string(),
                            sender: sender.clone(),
                        };

                        if let Ok(mut conns) = connections.try_lock() {
                            conns.insert(username.to_string(), player_conn);
                        }

                        send_game_state(color, &game, username, sender);
                    }
                },
                _ => {
                    println!("❌ Invalid game status: {}", game.status);
                    let msg = ServerMessage::GameNotFound {
                        message: "Invalid game status".to_string()
                    };
                    sender.send(WarpMessage::text(serde_json::to_string(&msg).unwrap())).ok();
                }
            }
        },
        Ok(None) => {
            println!("❌ Game {} not found", game_id);
            let msg = ServerMessage::GameNotFound {
                message: format!("Game {} not found", game_id)
            };
            sender.send(WarpMessage::text(serde_json::to_string(&msg).unwrap())).ok();
        },
        Err(e) => {
            println!("❌ Database error: {}", e);
            let msg = ServerMessage::Error("Internal server error".to_string());
            sender.send(WarpMessage::text(serde_json::to_string(&msg).unwrap())).ok();
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
    from: &str,
    to: &str,
    promotion: Option<String>,
    pgn: &str,
    fen: &str,
    timestamp: i64,
    db: &Database,
    connections: &Connections
) {
    let games = db.collection::<Game>("games");
    
    if let Ok(Some(mut game)) = games.find_one(doc! { "_id": game_id }, None).await {
        // Return early if game is not active
        if game.status != "active" {
            return;
        }

        // Check if it's the player's turn
        if (game.turn == "white" && game.white_player.as_deref() != Some(username)) ||
           (game.turn == "black" && game.black_player.as_deref() != Some(username)) {
            return;
        }

        // Parse FEN and check game ending conditions
        if let Ok(fen_obj) = Fen::from_str(&fen) {
            let setup = fen_obj.into_setup();
            if let Ok(position) = Chess::from_setup(setup, CastlingMode::Standard)
                .or_else(PositionError::ignore_too_much_material)
                .or_else(PositionError::ignore_impossible_check) 
            {
                // Update game state first
                game.moves.push(format!("{}{}", from, to));
                game.fen = fen.to_string();
                game.pgn = pgn.to_string();
                game.turn = if game.turn == "white" { "black".to_string() } else { "white".to_string() };
                
                // Calculate and update times
                let now = current_timestamp_ms();
                let elapsed_ms = now - game.last_move_timestamp;
                
                if game.turn == "black" { // White just moved
                    game.white_time_ms = (game.white_time_ms - elapsed_ms).max(0);
                    if game.moves.len() > 1 {
                        game.white_time_ms += game.increment_ms;
                    }
                } else { // Black just moved
                    game.black_time_ms = (game.black_time_ms - elapsed_ms).max(0);
                    if game.moves.len() > 1 {
                        game.black_time_ms += game.increment_ms;
                    }
                }
                game.last_move_timestamp = now;

                // Update game in database first
                games.update_one(
                    doc! { "_id": game_id },
                    doc! {
                        "$set": {
                            "moves": &game.moves,
                            "fen": &game.fen,
                            "pgn": &game.pgn,
                            "turn": &game.turn,
                            "white_time_ms": game.white_time_ms,
                            "black_time_ms": game.black_time_ms,
                            "last_move_timestamp": game.last_move_timestamp,
                            "updated_at": chrono::Utc::now().to_rfc3339()
                        }
                    },
                    None
                ).await.ok();

                // Notify players of the move
                let move_msg = ServerMessage::MoveMade {
                    from: from.to_string(),
                    to: to.to_string(),
                    fen: game.fen.clone(),
                    pgn: game.pgn.clone(),
                    by_username: username.to_string(),
                    turn: game.turn.clone(),
                    white_time_ms: game.white_time_ms,
                    black_time_ms: game.black_time_ms
                };

                if let Ok(conns) = connections.try_lock() {
                    for conn in conns.values() {
                        if conn.game_id == game_id {
                            conn.sender.send(WarpMessage::text(
                                serde_json::to_string(&move_msg).unwrap()
                            )).ok();
                        }
                    }
                }

                // Now check for game ending conditions
                let game_result = if position.is_checkmate() {
                    Some(format!("{} wins by checkmate", 
                        if game.turn == "white" { "Black" } else { "White" }))
                } else if position.is_stalemate() {
                    Some("Draw by stalemate".to_string())
                } else if position.is_insufficient_material() {
                    Some("Draw by insufficient material".to_string())
                } else {
                    None
                };

                // If game is over, update status and notify players
                if let Some(result) = game_result {
                    handle_game_over(game_id, result, db, connections).await;
                    return;
                }

                // Check for timeout after move
                check_time_out(&game, db, connections).await;
            }
        }
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
    
    let move_msg = ServerMessage::MoveMade {
        from: from.to_string(),
        to: to.to_string(),
        fen: fen.to_string(),
        pgn: pgn.to_string(),
        by_username: by_username.to_string(),
        turn: "white".to_string(),
        white_time_ms: *white_time,
        black_time_ms: *black_time
    };
    
    let time_msg = ServerMessage::TimeUpdate {
        white_time_ms: *white_time,
        black_time_ms: *black_time
    };

    let move_str = serde_json::to_string(&move_msg).unwrap();
    let time_str = serde_json::to_string(&time_msg).unwrap();

    // Try to get the lock with timeout
    let mut retry_count = 0;
    let max_retries = 5;
    
    while retry_count < max_retries {
        
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
        // Only update times if game is active and has both players
        if game.status != "active" || game.white_player.is_none() || game.black_player.is_none() {
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
    println!("⏰ Checking timeout for game: {}", game._id);
    println!("Current game status: {}", game.status);
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
        println!("Time out detected for game {}", game._id);
        let result = if white_time_remaining <= 0 {
            "Black wins on time"
        } else {
            "White wins on time"
        };

        let (standardized_result, winner) = get_game_result_info(
            result,
            &game.white_player,
            &game.black_player
        );

     

        let games = db.collection::<Game>("games");
        
        // First update the game status
        match games.find_one_and_update(
            doc! { 
                "_id": &game._id,
                "status": "active"  // Only update if still active
            },
            doc! { 
                "$set": {
                    "status": "completed",
                    "result": result,
                    "pgn": game.pgn.clone(),
                    "updated_at": chrono::Utc::now().to_rfc3339(),
                    "white_time_ms": white_time_remaining.max(0),
                    "black_time_ms": black_time_remaining.max(0)
                }
            },
            None
        ).await {
            Ok(Some(updated_game)) => {
                // Game was successfully updated, now notify clients
                if let Ok(conns) = connections.try_lock() {
                    for conn in conns.values() {
                        if conn.game_id == game._id {
                            // Send final time update
                            let time_msg = ServerMessage::TimeUpdate {
                                white_time_ms: white_time_remaining.max(0),
                                black_time_ms: black_time_remaining.max(0)
                            };
                            conn.sender.send(WarpMessage::text(
                                serde_json::to_string(&time_msg).unwrap()
                            )).ok();
                            
                            // Send game completion
                            send_completed_game(&updated_game, &conn.sender).await;
                        }
                    }
                }
            },
            Ok(None) => {
                // Game was not found or was not active
                println!("Game {} was not updated (not found or not active)", game._id);
            },
            Err(e) => {
                eprintln!("Error updating game {}: {}", game._id, e);
            }
        }
    }
}

// Helper function to determine winner and standardize result format
fn get_game_result_info(result_str: &str, white_player: &Option<String>, black_player: &Option<String>) 
    -> (String, Option<String>) { // Returns (standardized_result, winner)
    
    if result_str.contains("abandoned") {
        // Check who abandoned
        if let Some(username) = result_str.split_whitespace().next() {
            let is_white_abandoned = white_player.as_deref() == Some(username);
            if is_white_abandoned {
                return ("0-1".to_string(), black_player.clone())
            } else {
                return ("1-0".to_string(), white_player.clone())
            }
        }
    } else if result_str.contains("resigned") {
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
    } else if result_str == "1-0" {
        return ("1-0".to_string(), white_player.clone())
    } else if result_str == "0-1" {
        return ("0-1".to_string(), black_player.clone())
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
    println!("🎮 Preparing game completion message");
    println!("📊 Game state before processing:");
    println!("   Status: {}", game.status);
    println!("   Result: {}", game.result);
    println!("   Reason: {:?}", game.reason);

    let (standardized_result, winner) = get_game_result_info(
        &game.result, 
        &game.white_player, 
        &game.black_player
    );
    
    println!("🎯 After get_game_result_info:");
    println!("   Standardized Result: {}", standardized_result);
    println!("   Winner: {:?}", winner);

    // Convert to lowercase for case-insensitive comparison
    let result_lower = game.result.to_lowercase();
    let reason = if result_lower.contains("time") || game.reason.as_deref() == Some("timeout") {
        "timeout"
    } else if result_lower.contains("resigned") {
        "resignation"
    } else if result_lower.contains("checkmate") {
        "checkmate"
    } else if result_lower.contains("stalemate") {
        "stalemate"
    } else if result_lower.contains("draw") || game.reason.as_deref() == Some("Draw") {
        "draw"
    } else if result_lower.contains("abandonment") || result_lower.contains("abandoned") {
        "abandonment"
    } else {
        println!("❓ Unknown game end reason");
        "unknown"
    };


    let new_pgn = construct_complete_pgn(
        &game.white_player,
        &game.black_player,
        &standardized_result,
        &game.pgn,
        game.white_time,
        game.increment
    );

    let msg = ServerMessage::GameCompleted {
        game_id: game._id.clone(),
        white_player: game.white_player.clone(),
        black_player: game.black_player.clone(),
        fen: game.fen.clone(),
        pgn: new_pgn,
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

    if let Ok(msg_str) = serde_json::to_string(&msg) {
        sender.send(WarpMessage::text(msg_str)).ok();
    }
}

async fn handle_draw_offer(
    game_id: &str,
    username: &str,
    db: &Database,
    connections: &Connections
) {
    let games = db.collection::<Game>("games");
    
    if let Ok(Some(mut game)) = games.find_one(doc! { "_id": game_id }, None).await {
        if game.status != "active" {
            return;
        }

        // Update draw offer in database
        games.update_one(
            doc! { "_id": game_id },
            doc! { 
                "$set": {
                    "draw_offered_by": username,
                    "updated_at": chrono::Utc::now().to_rfc3339()
                }
            },
            None
        ).await.ok();

        // Notify opponent about draw offer
        let draw_msg = ServerMessage::DrawOffered {
            by_username: username.to_string()
        };

        if let Ok(conns) = connections.try_lock() {
            for conn in conns.values() {
                if conn.game_id == game_id && conn.username != username {
                    conn.sender.send(WarpMessage::text(
                        serde_json::to_string(&draw_msg).unwrap()
                    )).ok();
                }
            }
        }
    }
}

async fn handle_draw_accept(
    game_id: &str,
    username: &str,
    db: &Database,
    connections: &Connections
) {
    let games = db.collection::<Game>("games");
    
    if let Ok(Some(game)) = games.find_one(doc! { "_id": game_id }, None).await {
        // Verify there's a pending draw offer and it's from the opponent
        if game.draw_offered_by.as_deref() != None && 
           game.draw_offered_by.as_deref() != Some(username) {
            
            // Update game as drawn
            games.update_one(
                doc! { "_id": game_id },
                doc! { 
                    "$set": {
                        "status": "completed",
                        "result": "Draw by agreement",
                        "draw_offered_by": None::<Option<String>>,  // Specify the type here
                        "updated_at": chrono::Utc::now().to_rfc3339()
                    }
                },
                None
            ).await.ok();

            // Send game completion to both players
            if let Ok(Some(updated_game)) = games.find_one(doc! { "_id": game_id }, None).await {
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
}

async fn handle_draw_decline(
    game_id: &str,
    username: &str,
    db: &Database,
    connections: &Connections
) {
    let games = db.collection::<Game>("games");
    
    // Clear draw offer
    games.update_one(
        doc! { "_id": game_id },
        doc! { 
            "$set": {
                "draw_offered_by": None::<Option<String>>,  // Specify the type here
                "updated_at": chrono::Utc::now().to_rfc3339()
            }
        },
        None
    ).await.ok();

    // Notify opponent about declined draw
    let decline_msg = ServerMessage::DrawDeclined {
        by_username: username.to_string()
    };

    if let Ok(conns) = connections.try_lock() {
        for conn in conns.values() {
            if conn.game_id == game_id && conn.username != username {
                conn.sender.send(WarpMessage::text(
                    serde_json::to_string(&decline_msg).unwrap()
                )).ok();
            }
        }
    }
}


#[derive(Debug, Clone)]
pub struct GameConfig {
    pub time_control: i32,
    pub increment: i32,
}

/// Generates a secure, unique game ID using a cryptographically secure RNG
pub fn generate_game_id() -> String {
    let mut rng = rand::thread_rng();
    
    // Generate 8 random bytes (64 bits) for better uniqueness
    let random_bytes: Vec<u8> = (0..8)
        .map(|_| rng.gen())
        .collect();
    
    // Encode with Crockford's base32 for human-friendly IDs
    let encoded = encode(Alphabet::Crockford, &random_bytes)
        .to_lowercase();
    
    // Ensure exactly 10 characters
    if encoded.len() >= 10 {
        encoded[..10].to_string()
    } else {
        // Pad with zeros if needed (shouldn't happen with 8 bytes)
        format!("{:0<10}", encoded)
    }
}

/// Validates a game ID format and checks for uniqueness
pub async fn is_valid_game_id(game_id: &str, db: &Database) -> bool {
    let games = db.collection::<Game>("games");
    
    // Check format (lowercase alphanumeric, specific length)
    if game_id.len() != 10 || !game_id.chars().all(|c| {
        c.is_ascii_lowercase() || c.is_ascii_digit()
    }) {
        return false;
    }

    // Ensure ID doesn't exist
    match games.find_one(doc! { "_id": game_id }, None).await {
        Ok(Some(_)) => false,  // ID exists
        Ok(None) => true,      // ID is available
        Err(e) => {
            eprintln!("Database error while validating game ID: {}", e);
            false  // Consider invalid on database errors
        }
    }
}

// Add a helper function for existing game validation
pub async fn is_existing_game_id(game_id: &str, db: &Database) -> bool {
    let games = db.collection::<Game>("games");
    
    // Check format first
    if game_id.len() != 10 || !game_id.chars().all(|c| {
        c.is_ascii_lowercase() || c.is_ascii_digit()
    }) {
        return false;
    }

    // Check if game exists
    match games.find_one(doc! { "_id": game_id }, None).await {
        Ok(Some(_)) => true,   // Game exists
        Ok(None) => false,     // Game doesn't exist
        Err(e) => {
            eprintln!("Database error while checking existing game: {}", e);
            false
        }
    }
}

// Add constants for game configuration
pub const MIN_TIME_CONTROL: i32 = 30;    // 30 seconds minimum
pub const MAX_TIME_CONTROL: i32 = 7200;  // 2 hours maximum
pub const MIN_INCREMENT: i32 = 0;        // No increment minimum
pub const MAX_INCREMENT: i32 = 60;       // 1 minute maximum increment

// Add a helper function for validating time controls
pub fn is_valid_time_control(time_control: i32, increment: i32) -> bool {
    time_control >= MIN_TIME_CONTROL 
        && time_control <= MAX_TIME_CONTROL
        && increment >= MIN_INCREMENT 
        && increment <= MAX_INCREMENT
}

// Add new Message struct for database storage
#[derive(Debug, Serialize, Deserialize)]
pub struct ChatMessage {
    pub _id: String,           // UUID for the message
    pub game_id: String,       // Reference to the game
    pub sender: String,        // Username of sender
    pub content: String,       // Message content
    #[serde(with = "mongodb::bson::serde_helpers::chrono_datetime_as_bson_datetime")]
    pub timestamp: DateTime<Utc>, // When the message was sent
    pub visible_to_all: bool,  // Public or private message
    pub recipient: Option<String>, // For private messages
}

// Add the message handling function
async fn handle_chat_message(
    game_id: &str,
    username: &str,
    content: &str,
    recipient: &Option<String>,
    db: &Database,
    connections: &Connections,
   ) {
    let messages = db.collection::<ChatMessage>("chat_messages");
    let message_id = Uuid::new_v4().to_string();
    let timestamp = Utc::now();
    
    // Create new message document
    let message = ChatMessage {
        _id: message_id.clone(),
        game_id: game_id.to_string(),
        sender: username.to_string(),
        content: content.to_string(),
        timestamp,
        visible_to_all: recipient.is_none(),
        recipient: recipient.clone(),
    };

    // Store message in database
    if let Ok(_) = messages.insert_one(&message, None).await {
        let server_message = ServerMessage::ChatMessageReceived {
            id: message_id,
            game_id: game_id.to_string(),
            sender: username.to_string(),
            content: content.to_string(),
            timestamp,
            is_private: recipient.is_some(),
            recipient: recipient.clone(),
        };

        let msg_str = serde_json::to_string(&server_message).unwrap();

        // Send message to appropriate recipients
        if let Ok(conns) = connections.try_lock() {
            for conn in conns.values() {
                if conn.game_id == game_id {
                    // For private messages, send only to sender and recipient
                    if let Some(ref recipient) = recipient {
                        if conn.username == *recipient || conn.username == username {
                            conn.sender.send(WarpMessage::text(msg_str.clone())).ok();
                        }
                    } else {
                        // Public message, send to all players in the game
                        conn.sender.send(WarpMessage::text(msg_str.clone())).ok();
                    }
                }
            }
        }
    }
}

// Add function to fetch chat history
async fn fetch_chat_history(
    game_id: &str,
    username: &str,
    db: &Database,
    sender: &tokio::sync::mpsc::UnboundedSender<WarpMessage>,
) {
    let messages = db.collection::<ChatMessage>("chat_messages");
    
    // Query for messages visible to this user
    let filter = doc! {
        "$and": [
            { "game_id": game_id },
            { "$or": [
                { "visible_to_all": true },
                { "sender": username },
                { "recipient": username }
            ]}
        ]
    };
    
    if let Ok(mut cursor) = messages.find(filter, None).await {
        let mut chat_history = Vec::new();
        
        while let Ok(Some(message)) = cursor.try_next().await {
            chat_history.push(message);
        }
        
        // Sort messages by timestamp
        chat_history.sort_by(|a, b| a.timestamp.cmp(&b.timestamp));
        
        let history_msg = ServerMessage::ChatHistory {
            messages: chat_history,
        };
        
        sender.send(WarpMessage::text(
            serde_json::to_string(&history_msg).unwrap()
        )).ok();
    }
}

// Add this new struct to track active games
#[derive(Debug, Clone)]
struct ActiveGame {
    game_id: String,
    last_move_timestamp: i64,
    white_time_ms: i64,
    black_time_ms: i64,
    increment_ms: i64,
    turn: String,
}

// Add this function to start the time monitor when a game becomes active
async fn start_time_monitor(
    game_id: String,
    db: Database,
    connections: Connections
) {
    let games = db.collection::<Game>("games");
    
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(tokio::time::Duration::from_millis(100));
        
        loop {
            interval.tick().await;
            
            if let Ok(Some(game)) = games.find_one(doc! { "_id": &game_id }, None).await {
                if game.status != "active" {
                    break;
                }
                
                let now = current_timestamp_ms();
                let elapsed_ms = now - game.last_move_timestamp;
                
                let (white_time_remaining, black_time_remaining) = if game.turn == "white" {
                    (game.white_time_ms - elapsed_ms, game.black_time_ms)
                } else {
                    (game.white_time_ms, game.black_time_ms - elapsed_ms)
                };

                if white_time_remaining <= 0 || black_time_remaining <= 0 {
                    // Determine winner and result
                    let (result, winner) = if white_time_remaining <= 0 {
                        ("0-1", game.black_player.clone())
                    } else {
                        ("1-0", game.white_player.clone())
                    };

                    // Update game in database with complete information
                    if let Ok(Some(updated_game)) = games.find_one_and_update(
                        doc! { 
                            "_id": &game_id,
                            "status": "active"
                        },
                        doc! { 
                            "$set": {
                                "status": "completed",
                                "result": &result,
                                "winner": &winner,
                                "reason": "timeout",
                                "updated_at": chrono::Utc::now().to_rfc3339(),
                                "white_time_ms": white_time_remaining.max(0),
                                "black_time_ms": black_time_remaining.max(0)
                            }
                        },
                        None
                    ).await {
                        // Send GameCompleted to all connected clients
                        if let Ok(conns) = connections.try_lock() {
                            for conn in conns.values() {
                                if conn.game_id == game_id {
                                    send_completed_game(&updated_game, &conn.sender).await;
                                }
                            }
                        }
                    }
                    break;
                }
            } else {
                break;
            }
        }
    });
}

// First, let's add a new helper function to start the game timer
async fn start_game_timer(game_id: &str, db: &Database, connections: &Connections) {
    // Wait 2 seconds before starting the timer
    tokio::time::sleep(tokio::time::Duration::from_secs(2)).await;
    
    let games = db.collection::<Game>("games");
    
    // Set the initial timestamp when the game actually starts
    if let Ok(Some(mut game)) = games.find_one(doc! { "_id": game_id }, None).await {
        let now = chrono::Utc::now().timestamp_millis();
        
        // Update the game with fresh timestamps
        let update = doc! {
            "$set": {
                "last_move_timestamp": now,
                "updated_at": chrono::Utc::now().to_rfc3339()
            }
        };
        
        if let Ok(_) = games.update_one(doc! { "_id": game_id }, update, None).await {
            // Notify players that the game is starting
            let time_msg = ServerMessage::TimeUpdate {
                white_time_ms: game.white_time_ms,
                black_time_ms: game.black_time_ms
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
}