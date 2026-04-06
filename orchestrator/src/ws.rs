//! WebSocket gateway — real-time event push to connected clients.
//!
//! All events broadcast via `tokio::sync::broadcast`. Each connected
//! client gets its own receiver. Slow clients skip (lag) rather than
//! block producers.

use axum::{
    extract::{
        ws::{Message, WebSocket, WebSocketUpgrade},
        State,
    },
    response::IntoResponse,
};
use std::sync::Arc;
use tokio::sync::broadcast;
use tracing::{info, warn};

use crate::api::AppState;

/// Events pushed over WebSocket. JSON with `"type"` discriminator.
#[derive(Clone, Debug, serde::Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum WsEvent {
    Trade {
        trade_id: u64,
        price: String,
        size: String,
        taker_side: String,
        maker_user_id: String,
        taker_user_id: String,
        timestamp_ms: u64,
    },
    Orderbook {
        bids: Vec<[String; 2]>,
        asks: Vec<[String; 2]>,
    },
    Ticker {
        mark_price: String,
        index_price: String,
        timestamp: u64,
    },
    Liquidation {
        position_id: u64,
        user_id: String,
        price: String,
    },
}

/// Axum handler: upgrade to WebSocket, then forward broadcast events.
pub async fn ws_handler(
    ws: WebSocketUpgrade,
    State(state): State<Arc<AppState>>,
) -> impl IntoResponse {
    let rx = state.ws_tx.subscribe();
    ws.on_upgrade(move |socket| client_loop(socket, rx))
}

/// Per-client loop: read from broadcast, write JSON to socket.
async fn client_loop(mut socket: WebSocket, mut rx: broadcast::Receiver<WsEvent>) {
    info!("WebSocket client connected");
    loop {
        tokio::select! {
            result = rx.recv() => {
                match result {
                    Ok(event) => {
                        let json = match serde_json::to_string(&event) {
                            Ok(j) => j,
                            Err(_) => continue,
                        };
                        if socket.send(Message::Text(json.into())).await.is_err() {
                            break;
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(n)) => {
                        warn!("WebSocket client lagged, skipped {} events", n);
                    }
                    Err(broadcast::error::RecvError::Closed) => break,
                }
            }
            msg = socket.recv() => {
                match msg {
                    Some(Ok(Message::Close(_))) | None => break,
                    _ => {}
                }
            }
        }
    }
    info!("WebSocket client disconnected");
}
