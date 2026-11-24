//! WebSocket gateway — real-time event push to connected clients.
//!
//! All events broadcast via `tokio::sync::broadcast`. Each connected client
//! gets its own receiver + a set of subscribed channels. Slow clients skip
//! (lag) rather than block producers.
//!
//! ## Channels
//!
//! - `trades`        — public: every matched Trade
//! - `orderbook`     — public: orderbook snapshot after each trade
//! - `ticker`        — public: mark/index price updates
//! - `liquidations`  — public: every Liquidation
//! - `user:rXXX`     — private: per-user Fill, OrderUpdate, PositionChanged,
//!   and Liquidation events where `user_id == rXXX`
//!
//! ## Client protocol
//!
//! On connect, the server subscribes the client to the default public set
//! `{trades, orderbook, ticker, liquidations}` for backwards compatibility.
//!
//! At any time the client may send a JSON control frame:
//!
//! ```json
//! {"action":"subscribe","channels":["trades","user:rAlice..."]}
//! {"action":"unsubscribe","channels":["ticker"]}
//! {"action":"set","channels":["ticker","user:rBob..."]}
//! ```
//!
//! `set` replaces the entire subscription set; `subscribe` adds; `unsubscribe`
//! removes. Unknown channels are ignored silently.
//!
//! The data we broadcast is not secret (market data is public, user events
//! reference xrpl_addresses that are also public), so there is no
//! authentication on /ws — any client may subscribe to any user channel.
//! Authentication can be added later by requiring a signed X-XRPL-Signature
//! on upgrade.

use axum::{
    extract::{
        ws::{Message, WebSocket, WebSocketUpgrade},
        State,
    },
    http::StatusCode,
    response::IntoResponse,
};
use std::collections::HashSet;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use tokio::sync::broadcast;
use tracing::{debug, info, warn};

use crate::api::AppState;

/// Events pushed over WebSocket. JSON with `"type"` discriminator.
///
/// Events carrying a `user_id` are routed to the matching `user:<id>`
/// channel in addition to any relevant public channel.
#[derive(Clone, Debug, serde::Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum WsEvent {
    // ── Public market data ─────────────────────────────────────────
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

    // ── User-specific ──────────────────────────────────────────────
    /// A user's order was matched (one Fill per side per trade).
    Fill {
        user_id: String,
        order_id: u64,
        trade_id: u64,
        side: String, // "long" | "short" — the user's side on this fill
        role: String, // "maker" | "taker"
        price: String,
        size: String,
        timestamp_ms: u64,
    },
    /// Lifecycle update for a user's order.
    OrderUpdate {
        user_id: String,
        order_id: u64,
        status: String, // "open" | "partially_filled" | "filled" | "cancelled"
        filled: String,
        remaining: String,
        client_order_id: Option<String>,
    },
    /// Marker that a user's position state changed. Orchestrator does NOT
    /// carry position state (lives in the SGX enclave); clients should
    /// re-fetch `/v1/account/positions` on receipt.
    PositionChanged {
        user_id: String,
        reason: String, // "fill" | "liquidation"
    },
}

impl WsEvent {
    /// Default channel for the event (used when no explicit routing is
    /// configured). Public events go to their public channel; private
    /// events go to `user:<id>`.
    fn public_channel(&self) -> Option<&'static str> {
        match self {
            WsEvent::Trade { .. } => Some("trades"),
            WsEvent::Orderbook { .. } => Some("orderbook"),
            WsEvent::Ticker { .. } => Some("ticker"),
            WsEvent::Liquidation { .. } => Some("liquidations"),
            WsEvent::Fill { .. }
            | WsEvent::OrderUpdate { .. }
            | WsEvent::PositionChanged { .. } => None,
        }
    }

    /// User channel (`user:<id>`) for this event, if any. Liquidation also
    /// fans out to its owner's user channel.
    fn user_id(&self) -> Option<&str> {
        match self {
            WsEvent::Fill { user_id, .. }
            | WsEvent::OrderUpdate { user_id, .. }
            | WsEvent::PositionChanged { user_id, .. }
            | WsEvent::Liquidation { user_id, .. } => Some(user_id.as_str()),
            _ => None,
        }
    }

    /// Returns true if this event should be delivered to a client with the
    /// given subscription set.
    fn matches(&self, channels: &HashSet<String>) -> bool {
        if let Some(ch) = self.public_channel() {
            if channels.contains(ch) {
                return true;
            }
        }
        if let Some(uid) = self.user_id() {
            let user_ch = format!("user:{uid}");
            if channels.contains(&user_ch) {
                return true;
            }
        }
        false
    }
}

// ── Subscribe / unsubscribe control frames ────────────────────────

#[derive(Debug, serde::Deserialize)]
#[serde(tag = "action", rename_all = "lowercase")]
enum ControlFrame {
    Subscribe { channels: Vec<String> },
    Unsubscribe { channels: Vec<String> },
    Set { channels: Vec<String> },
    Ping,
}

fn default_channels() -> HashSet<String> {
    ["trades", "orderbook", "ticker", "liquidations"]
        .iter()
        .map(|s| s.to_string())
        .collect()
}

// ── Handler + client loop ─────────────────────────────────────────

/// Axum handler: upgrade to WebSocket, then forward broadcast events.
pub async fn ws_handler(
    ws: WebSocketUpgrade,
    State(state): State<Arc<AppState>>,
) -> impl IntoResponse {
    // Market-data stream: only the sequencer holds the authoritative book.
    // Reject the upgrade on validators so nginx routes the WS to the
    // sequencer (cluster read-consistency, task #113 — mirrors the read
    // guard in api::require_sequencer_read).
    if !state.is_sequencer.load(Ordering::Relaxed) {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            "this node is not the sequencer",
        )
            .into_response();
    }
    upgrade(ws, state.log_health.clone(), state.ws_tx.subscribe())
}

/// The stream carries trading data (book, trades, fills, order updates), so it is gated like the trading
/// routes: no new stream while this node's replicated state is not the cluster's (diverged or fenced),
/// and an open stream is closed (1013, try again later) instead of sending its next event.
fn upgrade(
    ws: WebSocketUpgrade,
    health: Arc<crate::log_node::LogHealth>,
    rx: broadcast::Receiver<WsEvent>,
) -> axum::response::Response {
    if health.refuses_trading() {
        return (StatusCode::SERVICE_UNAVAILABLE, STATE_NOT_THE_CLUSTERS).into_response();
    }
    ws.on_upgrade(move |socket| client_loop(socket, rx, health))
        .into_response()
}

const STATE_NOT_THE_CLUSTERS: &str =
    "this node's replicated state is not the cluster's (diverged or fenced); use another node";

/// Per-client loop: read from broadcast, filter by subscribed channels,
/// write JSON to socket.
async fn client_loop(
    mut socket: WebSocket,
    mut rx: broadcast::Receiver<WsEvent>,
    health: Arc<crate::log_node::LogHealth>,
) {
    info!("WebSocket client connected");
    let mut channels: HashSet<String> = default_channels();

    loop {
        tokio::select! {
            result = rx.recv() => {
                match result {
                    Ok(event) => {
                        if !event.matches(&channels) {
                            continue;
                        }
                        if health.refuses_trading() {
                            let _ = socket
                                .send(Message::Close(Some(axum::extract::ws::CloseFrame {
                                    code: 1013,
                                    reason: STATE_NOT_THE_CLUSTERS.into(),
                                })))
                                .await;
                            break;
                        }
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
                    Some(Ok(Message::Text(text))) => {
                        handle_control(&text, &mut channels, &mut socket).await;
                    }
                    Some(Ok(Message::Ping(p))) => {
                        if socket.send(Message::Pong(p)).await.is_err() {
                            break;
                        }
                    }
                    _ => {}
                }
            }
        }
    }
    info!("WebSocket client disconnected");
}

async fn handle_control(text: &str, channels: &mut HashSet<String>, socket: &mut WebSocket) {
    let frame: ControlFrame = match serde_json::from_str(text) {
        Ok(f) => f,
        Err(e) => {
            let msg = serde_json::json!({
                "type": "error",
                "message": format!("invalid control frame: {}", e),
            });
            let _ = socket.send(Message::Text(msg.to_string().into())).await;
            return;
        }
    };

    match frame {
        ControlFrame::Subscribe { channels: add } => {
            for c in add {
                channels.insert(c);
            }
        }
        ControlFrame::Unsubscribe { channels: rm } => {
            for c in rm {
                channels.remove(&c);
            }
        }
        ControlFrame::Set { channels: new } => {
            channels.clear();
            for c in new {
                channels.insert(c);
            }
        }
        ControlFrame::Ping => {
            let msg = serde_json::json!({"type": "pong"});
            let _ = socket.send(Message::Text(msg.to_string().into())).await;
            return;
        }
    }

    // Acknowledge the new subscription set
    let ack = serde_json::json!({
        "type": "subscribed",
        "channels": channels.iter().collect::<Vec<_>>(),
    });
    debug!(subscribed = ?channels, "ws subscription updated");
    let _ = socket.send(Message::Text(ack.to_string().into())).await;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn set(channels: &[&str]) -> HashSet<String> {
        channels.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn trade_event_serialization() {
        let event = WsEvent::Trade {
            trade_id: 1,
            price: "0.55000000".into(),
            size: "100.00000000".into(),
            taker_side: "long".into(),
            maker_user_id: "rAlice".into(),
            taker_user_id: "rBob".into(),
            timestamp_ms: 1743500000000,
        };
        let json = serde_json::to_string(&event).unwrap();
        assert!(json.contains("\"type\":\"trade\""));
        assert!(json.contains("\"trade_id\":1"));
    }

    #[test]
    fn fill_event_serialization() {
        let event = WsEvent::Fill {
            user_id: "rAlice".into(),
            order_id: 42,
            trade_id: 7,
            side: "long".into(),
            role: "taker".into(),
            price: "0.55".into(),
            size: "100".into(),
            timestamp_ms: 1,
        };
        let json = serde_json::to_string(&event).unwrap();
        assert!(json.contains("\"type\":\"fill\""));
        assert!(json.contains("\"user_id\":\"rAlice\""));
        assert!(json.contains("\"role\":\"taker\""));
    }

    #[test]
    fn order_update_serialization() {
        let event = WsEvent::OrderUpdate {
            user_id: "rAlice".into(),
            order_id: 99,
            status: "partially_filled".into(),
            filled: "50".into(),
            remaining: "50".into(),
            client_order_id: Some("my-42".into()),
        };
        let json = serde_json::to_string(&event).unwrap();
        assert!(json.contains("\"type\":\"order_update\""));
        assert!(json.contains("\"status\":\"partially_filled\""));
    }

    #[test]
    fn position_changed_serialization() {
        let event = WsEvent::PositionChanged {
            user_id: "rAlice".into(),
            reason: "fill".into(),
        };
        let json = serde_json::to_string(&event).unwrap();
        assert!(json.contains("\"type\":\"position_changed\""));
    }

    #[test]
    fn matches_public_channel() {
        let ticker = WsEvent::Ticker {
            mark_price: "0.5".into(),
            index_price: "0.5".into(),
            timestamp: 0,
        };
        assert!(ticker.matches(&set(&["ticker"])));
        assert!(!ticker.matches(&set(&["trades"])));
    }

    #[test]
    fn matches_user_channel() {
        let fill = WsEvent::Fill {
            user_id: "rAlice".into(),
            order_id: 1,
            trade_id: 1,
            side: "long".into(),
            role: "taker".into(),
            price: "1".into(),
            size: "1".into(),
            timestamp_ms: 0,
        };
        // Fill has no public channel — must subscribe by user
        assert!(!fill.matches(&set(&["trades"])));
        assert!(fill.matches(&set(&["user:rAlice"])));
        assert!(!fill.matches(&set(&["user:rBob"])));
    }

    #[test]
    fn liquidation_fans_out_to_user_and_public() {
        let liq = WsEvent::Liquidation {
            position_id: 1,
            user_id: "rVictim".into(),
            price: "0.1".into(),
        };
        assert!(liq.matches(&set(&["liquidations"])));
        assert!(liq.matches(&set(&["user:rVictim"])));
        assert!(!liq.matches(&set(&["user:rAlice"])));
    }

    #[test]
    fn default_channels_include_public_set() {
        let ch = default_channels();
        for c in ["trades", "orderbook", "ticker", "liquidations"] {
            assert!(ch.contains(c), "missing default channel: {c}");
        }
        assert!(!ch.iter().any(|c| c.starts_with("user:")));
    }

    #[test]
    fn control_frame_parse_subscribe() {
        let txt = r#"{"action":"subscribe","channels":["trades","user:rAlice"]}"#;
        let frame: ControlFrame = serde_json::from_str(txt).unwrap();
        match frame {
            ControlFrame::Subscribe { channels } => {
                assert_eq!(
                    channels,
                    vec!["trades".to_string(), "user:rAlice".to_string()]
                );
            }
            _ => panic!("expected Subscribe"),
        }
    }

    #[test]
    fn control_frame_parse_set_and_unsubscribe() {
        let txt = r#"{"action":"set","channels":["ticker"]}"#;
        matches!(
            serde_json::from_str::<ControlFrame>(txt).unwrap(),
            ControlFrame::Set { .. }
        );
        let txt = r#"{"action":"unsubscribe","channels":["trades"]}"#;
        matches!(
            serde_json::from_str::<ControlFrame>(txt).unwrap(),
            ControlFrame::Unsubscribe { .. }
        );
    }

    #[test]
    fn control_frame_parse_ping() {
        let txt = r#"{"action":"ping"}"#;
        matches!(
            serde_json::from_str::<ControlFrame>(txt).unwrap(),
            ControlFrame::Ping
        );
    }

    mod state_gate {
        use super::*;
        use crate::log_node::LogHealth;
        use futures_util::StreamExt;
        use tokio_tungstenite::tungstenite::Message as C;

        async fn serve(health: Arc<LogHealth>, tx: broadcast::Sender<WsEvent>) -> String {
            let app = axum::Router::new().route(
                "/ws",
                axum::routing::get(move |ws: WebSocketUpgrade| {
                    let (h, rx) = (health.clone(), tx.subscribe());
                    async move { upgrade(ws, h, rx) }
                }),
            );
            let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let a = l.local_addr().unwrap();
            tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
            format!("ws://{a}/ws")
        }

        fn trade(id: u64) -> WsEvent {
            WsEvent::Trade {
                trade_id: id,
                price: "1".into(),
                size: "1".into(),
                taker_side: "long".into(),
                maker_user_id: "rA".into(),
                taker_user_id: "rB".into(),
                timestamp_ms: 1,
            }
        }

        #[tokio::test]
        async fn an_open_stream_closes_when_the_node_diverges_and_a_new_one_is_refused() {
            let health = Arc::new(LogHealth::new("follow"));
            let (tx, _keep) = broadcast::channel(16);
            let url = serve(health.clone(), tx.clone()).await;
            let (mut c, _) = tokio_tungstenite::connect_async(&url).await.unwrap();
            // Wait until the server side has subscribed, then a healthy node streams.
            while tx.receiver_count() < 2 {
                tokio::task::yield_now().await;
            }
            tx.send(trade(1)).unwrap();
            let m = c.next().await.unwrap().unwrap();
            assert!(m.to_text().unwrap().contains("\"trade_id\":1"), "{m:?}");

            health.update(|s| {
                s.halted = Some(crate::log_client::rc::DIVERGED);
                s.diverged = true;
            });
            tx.send(trade(2)).unwrap();
            match c.next().await.unwrap().unwrap() {
                C::Close(Some(f)) => assert_eq!(u16::from(f.code), 1013),
                other => panic!("the diverged node sent {other:?} instead of closing"),
            }

            let e = tokio_tungstenite::connect_async(&url).await.unwrap_err();
            assert!(e.to_string().contains("503"), "{e}");
        }
    }
}
