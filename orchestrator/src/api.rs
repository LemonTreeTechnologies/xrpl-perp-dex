//! HTTP API server for the trading engine.
//!
//! Public endpoints (user-facing):
//!   POST /v1/orders          — submit order
//!   DELETE /v1/orders/:id    — cancel order
//!   DELETE /v1/orders        — cancel all (requires user_id query param)
//!   GET /v1/orders           — user's open orders (requires user_id query param)
//!   GET /v1/positions        — user's positions (proxied to enclave)
//!   GET /v1/account/balance  — user's balance (proxied to enclave)
//!   GET /v1/markets/:market/orderbook  — order book depth
//!   GET /v1/markets/:market/ticker     — best bid/ask/mid
//!   GET /v1/markets/:market/trades     — recent trades

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use axum::{
    extract::{Path, Query, State},
    http::{HeaderValue, Method, StatusCode},
    response::IntoResponse,
    routing::{delete, get, post},
    Json, Router,
};
use serde::{Deserialize, Serialize};
use tower_http::cors::CorsLayer;
use tracing::error;

use tokio::sync::broadcast;

use crate::auth;
use crate::orderbook::{OrderType, TimeInForce};
use crate::perp_client::PerpClient;
use crate::trading::TradingEngine;
use crate::types::{FP8, Side};
use crate::ws::{self, WsEvent};

// ── App state ───────────────────────────────────────────────────

pub struct AppState {
    pub engine: TradingEngine,
    pub perp: PerpClient,
    pub ws_tx: broadcast::Sender<WsEvent>,
    pub is_sequencer: Arc<AtomicBool>,
    /// Current mark price (updated by background price feed)
    pub mark_price: Arc<std::sync::atomic::AtomicI64>,
    /// Current funding rate (FP8 raw value)
    pub funding_rate: Arc<std::sync::atomic::AtomicI64>,
    /// Last funding application timestamp
    pub last_funding_time: Arc<std::sync::atomic::AtomicU64>,
    /// XRPL config for withdrawals
    pub xrpl_url: String,
    pub escrow_address: String,
}

// ── Request/Response types ──────────────────────────────────────

#[derive(Deserialize)]
pub struct SubmitOrderRequest {
    pub user_id: String,
    #[allow(dead_code)]
    pub market: Option<String>,
    pub side: String,
    #[serde(rename = "type", default = "default_limit")]
    pub order_type: String,
    pub price: Option<String>,
    pub size: String,
    #[serde(default = "default_leverage")]
    pub leverage: u32,
    #[serde(default = "default_gtc")]
    pub time_in_force: String,
    #[serde(default)]
    pub reduce_only: bool,
    pub client_order_id: Option<String>,
}

fn default_limit() -> String { "limit".into() }
fn default_leverage() -> u32 { 1 }
fn default_gtc() -> String { "gtc".into() }

#[derive(Serialize)]
struct ApiResponse<T: Serialize> {
    status: String,
    #[serde(flatten)]
    data: T,
}

#[derive(Serialize)]
struct ErrorResponse {
    status: String,
    message: String,
}

#[derive(Deserialize)]
pub struct UserIdQuery {
    pub user_id: Option<String>,
}

#[derive(Deserialize)]
pub struct DepthQuery {
    pub levels: Option<usize>,
}

// ── Helpers ─────────────────────────────────────────────────────

fn ok<T: Serialize>(data: T) -> (StatusCode, Json<ApiResponse<T>>) {
    (StatusCode::OK, Json(ApiResponse { status: "success".into(), data }))
}

fn err(code: StatusCode, msg: &str) -> impl IntoResponse {
    (code, Json(ErrorResponse { status: "error".into(), message: msg.into() }))
}

fn parse_side(s: &str) -> Result<Side, String> {
    match s.to_lowercase().as_str() {
        "buy" | "long" => Ok(Side::Long),
        "sell" | "short" => Ok(Side::Short),
        _ => Err(format!("invalid side: {}", s)),
    }
}

fn parse_order_type(s: &str) -> Result<OrderType, String> {
    match s.to_lowercase().as_str() {
        "limit" => Ok(OrderType::Limit),
        "market" => Ok(OrderType::Market),
        _ => Err(format!("invalid order type: {}", s)),
    }
}

fn parse_tif(s: &str) -> Result<TimeInForce, String> {
    match s.to_lowercase().as_str() {
        "gtc" => Ok(TimeInForce::Gtc),
        "ioc" => Ok(TimeInForce::Ioc),
        "fok" => Ok(TimeInForce::Fok),
        _ => Err(format!("invalid time_in_force: {}", s)),
    }
}

// ── Router ──────────────────────────────────────────────────────

pub fn router(state: Arc<AppState>) -> Router {
    let cors = CorsLayer::new()
        .allow_origin("*".parse::<HeaderValue>().unwrap())
        .allow_methods([Method::GET, Method::POST, Method::DELETE, Method::OPTIONS])
        .allow_headers(tower_http::cors::Any);

    Router::new()
        .route("/v1/orders", post(submit_order))
        .route("/v1/orders", get(get_orders))
        .route("/v1/orders", delete(cancel_all_orders))
        .route("/v1/orders/{order_id}", delete(cancel_order))
        .route("/v1/account/balance", get(get_balance))
        .route("/v1/withdraw", post(withdraw))
        .route("/v1/markets/{market}/orderbook", get(get_orderbook))
        .route("/v1/markets/{market}/ticker", get(get_ticker))
        .route("/v1/markets/{market}/trades", get(get_trades))
        .route("/v1/markets/{market}/funding", get(get_funding))
        .route("/v1/markets", get(get_markets))
        .route("/v1/openapi.json", get(openapi_spec))
        .route("/v1/attestation/quote", post(attestation_quote))
        .route("/v1/attestation/commitment", get(attestation_commitment))
        .layer(axum::middleware::from_fn(auth::auth_middleware))
        .route("/ws", get(ws::ws_handler))
        .layer(cors)
        .with_state(state)
}

/// Serve OpenAPI spec
/// DCAP Remote Attestation — proxy to enclave.
/// Public endpoint (no auth) — anyone can verify the enclave.
/// Returns SGX Quote v3 with Intel ECDSA signature chain.
/// On hardware without DCAP support: returns 503.
async fn attestation_quote(
    State(state): State<Arc<AppState>>,
    body: axum::body::Bytes,
) -> impl IntoResponse {
    // Parse optional user_data from body
    let user_data = if body.is_empty() {
        "0x00".to_string()
    } else {
        match serde_json::from_slice::<serde_json::Value>(&body) {
            Ok(v) => v.get("user_data")
                .and_then(|u| u.as_str())
                .unwrap_or("0x00")
                .to_string(),
            Err(_) => "0x00".to_string(),
        }
    };

    // Proxy to enclave attestation-quote endpoint
    let _enclave_url = format!(
        "{}/pool/attestation-quote",
        state.perp.base_url().replace("/v1", "").replace("/perp", "")
    );

    let client = reqwest::Client::builder()
        .danger_accept_invalid_certs(true)
        .timeout(std::time::Duration::from_secs(60))
        .build()
        .unwrap();

    match client.post(&format!("{}/v1/pool/attestation-quote",
            state.perp.base_url().trim_end_matches("/v1").trim_end_matches("/")))
        .json(&serde_json::json!({"user_data": user_data}))
        .send()
        .await
    {
        Ok(resp) => {
            match resp.json::<serde_json::Value>().await {
                Ok(data) => {
                    if data.get("status").and_then(|s| s.as_str()) == Some("success") {
                        (StatusCode::OK, Json(data)).into_response()
                    } else {
                        (StatusCode::SERVICE_UNAVAILABLE, Json(serde_json::json!({
                            "status": "error",
                            "message": "DCAP attestation not available on this platform. Use Azure DCsv3 for hardware attestation.",
                            "enclave_response": data
                        }))).into_response()
                    }
                }
                Err(e) => (StatusCode::BAD_GATEWAY, Json(serde_json::json!({
                    "status": "error",
                    "message": format!("Failed to parse enclave response: {}", e)
                }))).into_response()
            }
        }
        Err(e) => (StatusCode::BAD_GATEWAY, Json(serde_json::json!({
            "status": "error",
            "message": format!("Failed to reach enclave: {}", e)
        }))).into_response()
    }
}

/// Get latest state commitment info (for on-chain verification).
/// Public endpoint — no auth needed.
async fn attestation_commitment() -> impl IntoResponse {
    ok(serde_json::json!({
        "registry": crate::commitment::REGISTRY_ADDRESS,
        "network": "sepolia",
        "description": "CommitmentRegistryV4 — TEE-signed state proofs",
        "how_to_verify": {
            "1": "Query this endpoint or the contract directly on Sepolia",
            "2": "Verify ECDSA signature matches attested enclave address",
            "3": "Verify Merkle root against published state",
            "4": "Use /v1/attestation/quote to verify enclave identity (DCAP)"
        },
        "contract_abi": "commit(bytes32 marketId, bytes32 root, bytes32 snapshotHash, uint8 v, bytes32 r, bytes32 s)",
        "etherscan": format!("https://sepolia.etherscan.io/address/{}", crate::commitment::REGISTRY_ADDRESS),
    })).into_response()
}

async fn openapi_spec() -> impl IntoResponse {
    let spec = serde_json::json!({
        "openapi": "3.0.3",
        "info": {
            "title": "Perp DEX Trading API",
            "description": "Perpetual futures DEX on XRPL with TEE (Intel SGX). Settlement in RLUSD.",
            "version": "0.1.0"
        },
        "servers": [
            {"url": "/", "description": "Current server (relative URL)"}
        ],
        "paths": {
            "/v1/orders": {
                "post": {
                    "summary": "Submit order",
                    "requestBody": {
                        "required": true,
                        "content": {
                            "application/json": {
                                "schema": {"$ref": "#/components/schemas/SubmitOrder"},
                                "example": {
                                    "user_id": "rAlice123",
                                    "side": "buy",
                                    "type": "limit",
                                    "price": "0.55000000",
                                    "size": "100.00000000",
                                    "leverage": 5,
                                    "time_in_force": "gtc"
                                }
                            }
                        }
                    },
                    "responses": {"200": {"description": "Order result with fills"}}
                },
                "get": {
                    "summary": "Get user's open orders",
                    "parameters": [{"name": "user_id", "in": "query", "required": true, "schema": {"type": "string"}}],
                    "responses": {"200": {"description": "List of open orders"}}
                },
                "delete": {
                    "summary": "Cancel all user's orders",
                    "parameters": [{"name": "user_id", "in": "query", "required": true, "schema": {"type": "string"}}],
                    "responses": {"200": {"description": "Number cancelled"}}
                }
            },
            "/v1/orders/{order_id}": {
                "delete": {
                    "summary": "Cancel order by ID",
                    "parameters": [{"name": "order_id", "in": "path", "required": true, "schema": {"type": "integer"}}],
                    "responses": {"200": {"description": "Cancelled order"}}
                }
            },
            "/v1/account/balance": {
                "get": {
                    "summary": "Get user balance and positions",
                    "parameters": [{"name": "user_id", "in": "query", "required": true, "schema": {"type": "string"}}],
                    "responses": {"200": {"description": "Balance, margin, positions, unrealized PnL"}}
                }
            },
            "/v1/markets/{market}/orderbook": {
                "get": {
                    "summary": "Order book depth",
                    "parameters": [
                        {"name": "market", "in": "path", "required": true, "schema": {"type": "string"}, "example": "XRP-RLUSD-PERP"},
                        {"name": "levels", "in": "query", "schema": {"type": "integer", "default": 20}}
                    ],
                    "responses": {"200": {"description": "Bids and asks arrays"}}
                }
            },
            "/v1/markets/{market}/ticker": {
                "get": {
                    "summary": "Best bid/ask/mid price",
                    "parameters": [{"name": "market", "in": "path", "required": true, "schema": {"type": "string"}}],
                    "responses": {"200": {"description": "Ticker data"}}
                }
            },
            "/v1/markets/{market}/trades": {
                "get": {
                    "summary": "Recent trades",
                    "parameters": [{"name": "market", "in": "path", "required": true, "schema": {"type": "string"}}],
                    "responses": {"200": {"description": "Last 100 trades"}}
                }
            },
            "/v1/markets/{market}/funding": {
                "get": {
                    "summary": "Current funding rate",
                    "parameters": [{"name": "market", "in": "path", "required": true, "schema": {"type": "string"}}],
                    "responses": {"200": {"description": "Funding rate, mark price, next funding time"}}
                }
            },
            "/v1/markets": {
                "get": {
                    "summary": "List available markets",
                    "responses": {"200": {"description": "Market details: name, mark price, fees, leverage"}}
                }
            },
            "/v1/attestation/quote": {
                "post": {
                    "summary": "DCAP remote attestation (SGX Quote v3)",
                    "description": "Returns Intel-signed SGX Quote v3. Azure DCsv3 only. Send user_data as challenge nonce.",
                    "requestBody": {"content": {"application/json": {"schema": {"type": "object", "properties": {"user_data": {"type": "string"}}}}}},
                    "responses": {"200": {"description": "SGX Quote hex"}, "503": {"description": "DCAP not available"}}
                }
            },
            "/v1/attestation/commitment": {
                "get": {
                    "summary": "On-chain state commitment info (Sepolia)",
                    "responses": {"200": {"description": "CommitmentRegistryV4 contract details"}}
                }
            },
            "/ws": {
                "get": {
                    "summary": "WebSocket — real-time market data",
                    "description": "Upgrade to WebSocket. Receives JSON events: trade, orderbook, ticker, liquidation. No auth required."
                }
            }
        },
        "components": {
            "schemas": {
                "SubmitOrder": {
                    "type": "object",
                    "required": ["user_id", "side", "size"],
                    "properties": {
                        "user_id": {"type": "string", "description": "XRPL r-address or any unique ID"},
                        "side": {"type": "string", "enum": ["buy", "sell", "long", "short"]},
                        "type": {"type": "string", "enum": ["limit", "market"], "default": "limit"},
                        "price": {"type": "string", "description": "FP8 price (required for limit)", "example": "0.55000000"},
                        "size": {"type": "string", "description": "FP8 size in XRP", "example": "100.00000000"},
                        "leverage": {"type": "integer", "default": 1, "minimum": 1, "maximum": 20},
                        "time_in_force": {"type": "string", "enum": ["gtc", "ioc", "fok"], "default": "gtc"},
                        "reduce_only": {"type": "boolean", "default": false},
                        "client_order_id": {"type": "string", "description": "Optional user-defined ID"}
                    }
                }
            }
        }
    });
    (StatusCode::OK, Json(spec))
}

// ── Handlers ────────────────────────────────────────────────────

async fn submit_order(
    State(state): State<Arc<AppState>>,
    Json(req): Json<SubmitOrderRequest>,
) -> impl IntoResponse {
    if !state.is_sequencer.load(Ordering::Relaxed) {
        return err(StatusCode::SERVICE_UNAVAILABLE, "this node is not the sequencer").into_response();
    }

    let side = match parse_side(&req.side) {
        Ok(s) => s,
        Err(e) => return err(StatusCode::BAD_REQUEST, &e).into_response(),
    };
    let order_type = match parse_order_type(&req.order_type) {
        Ok(t) => t,
        Err(e) => return err(StatusCode::BAD_REQUEST, &e).into_response(),
    };
    let tif = match parse_tif(&req.time_in_force) {
        Ok(t) => t,
        Err(e) => return err(StatusCode::BAD_REQUEST, &e).into_response(),
    };

    let price = match order_type {
        OrderType::Market => FP8::ZERO,
        OrderType::Limit => match req.price.as_deref() {
            Some(p) => match p.parse::<FP8>() {
                Ok(fp) => fp,
                Err(_) => return err(StatusCode::BAD_REQUEST, "invalid price").into_response(),
            },
            None => return err(StatusCode::BAD_REQUEST, "limit order requires price").into_response(),
        },
    };

    let size = match req.size.parse::<FP8>() {
        Ok(fp) if fp.raw() > 0 => fp,
        _ => return err(StatusCode::BAD_REQUEST, "invalid or non-positive size").into_response(),
    };

    // Validate leverage (1-20)
    if req.leverage < 1 || req.leverage > 20 {
        return err(StatusCode::BAD_REQUEST, "leverage must be 1-20").into_response();
    }

    match state.engine.submit_order(
        req.user_id,
        side,
        order_type,
        price,
        size,
        req.leverage,
        tif,
        req.reduce_only,
        req.client_order_id,
    ).await {
        Ok(result) => {
            // Broadcast trade events via WebSocket
            for t in &result.trades {
                let _ = state.ws_tx.send(WsEvent::Trade {
                    trade_id: t.trade_id,
                    price: t.price.to_string(),
                    size: t.size.to_string(),
                    taker_side: format!("{}", t.taker_side),
                    maker_user_id: t.maker_user_id.clone(),
                    taker_user_id: t.taker_user_id.clone(),
                    timestamp_ms: t.timestamp_ms,
                });
            }
            // Broadcast orderbook snapshot after trade
            if !result.trades.is_empty() {
                let (bids, asks) = state.engine.depth(20).await;
                let _ = state.ws_tx.send(WsEvent::Orderbook {
                    bids: bids.iter().map(|(p, s)| [p.to_string(), s.to_string()]).collect(),
                    asks: asks.iter().map(|(p, s)| [p.to_string(), s.to_string()]).collect(),
                });
            }

            let trades_json: Vec<serde_json::Value> = result.trades.iter().map(|t| {
                serde_json::json!({
                    "trade_id": t.trade_id,
                    "price": t.price.to_string(),
                    "size": t.size.to_string(),
                    "maker_user_id": t.maker_user_id,
                    "taker_user_id": t.taker_user_id,
                    "taker_side": format!("{}", t.taker_side),
                })
            }).collect();

            ok(serde_json::json!({
                "order_id": result.order.id,
                "order_status": format!("{:?}", result.order.status),
                "filled": result.order.filled.to_string(),
                "remaining": result.order.remaining().to_string(),
                "trades": trades_json,
                "failed_fills": result.failed_fills.len(),
            })).into_response()
        }
        Err(e) => {
            error!("submit_order error: {}", e);
            err(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()).into_response()
        }
    }
}

async fn cancel_order(
    State(state): State<Arc<AppState>>,
    Path(order_id): Path<u64>,
) -> impl IntoResponse {
    match state.engine.cancel_order(order_id).await {
        Ok(order) => ok(serde_json::json!({
            "order_id": order.id,
            "status": format!("{:?}", order.status),
        })).into_response(),
        Err(e) => err(StatusCode::NOT_FOUND, &e.to_string()).into_response(),
    }
}

async fn cancel_all_orders(
    State(state): State<Arc<AppState>>,
    Query(params): Query<UserIdQuery>,
) -> impl IntoResponse {
    let user_id = match params.user_id {
        Some(id) => id,
        None => return err(StatusCode::BAD_REQUEST, "user_id query param required").into_response(),
    };
    let cancelled = state.engine.cancel_all(&user_id).await;
    ok(serde_json::json!({
        "cancelled": cancelled.len(),
    })).into_response()
}

async fn get_orders(
    State(state): State<Arc<AppState>>,
    Query(params): Query<UserIdQuery>,
) -> impl IntoResponse {
    let user_id = match params.user_id {
        Some(id) => id,
        None => return err(StatusCode::BAD_REQUEST, "user_id query param required").into_response(),
    };
    let orders = state.engine.user_orders(&user_id).await;
    let orders_json: Vec<serde_json::Value> = orders.iter().map(|o| {
        serde_json::json!({
            "order_id": o.id,
            "side": format!("{}", o.side),
            "type": format!("{:?}", o.order_type),
            "price": o.price.to_string(),
            "size": o.size.to_string(),
            "filled": o.filled.to_string(),
            "remaining": o.remaining().to_string(),
            "status": format!("{:?}", o.status),
        })
    }).collect();
    ok(serde_json::json!({ "orders": orders_json })).into_response()
}

async fn get_balance(
    State(state): State<Arc<AppState>>,
    Query(params): Query<UserIdQuery>,
) -> impl IntoResponse {
    let user_id = match params.user_id {
        Some(id) => id,
        None => return err(StatusCode::BAD_REQUEST, "user_id query param required").into_response(),
    };
    match state.perp.get_balance(&user_id).await {
        Ok(val) => (StatusCode::OK, Json(val)).into_response(),
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()).into_response(),
    }
}

async fn get_orderbook(
    State(state): State<Arc<AppState>>,
    Path(_market): Path<String>,
    Query(params): Query<DepthQuery>,
) -> impl IntoResponse {
    let levels = params.levels.unwrap_or(20).min(100); // cap at 100
    let (bids, asks) = state.engine.depth(levels).await;

    let bids_json: Vec<[String; 2]> = bids.iter()
        .map(|(p, s)| [p.to_string(), s.to_string()])
        .collect();
    let asks_json: Vec<[String; 2]> = asks.iter()
        .map(|(p, s)| [p.to_string(), s.to_string()])
        .collect();

    ok(serde_json::json!({
        "bids": bids_json,
        "asks": asks_json,
    })).into_response()
}

async fn get_ticker(
    State(state): State<Arc<AppState>>,
    Path(_market): Path<String>,
) -> impl IntoResponse {
    let (bid, ask, mid) = state.engine.ticker().await;
    ok(serde_json::json!({
        "best_bid": bid.map(|p| p.to_string()),
        "best_ask": ask.map(|p| p.to_string()),
        "mid_price": mid.map(|p| p.to_string()),
    })).into_response()
}

async fn get_trades(
    State(state): State<Arc<AppState>>,
    Path(_market): Path<String>,
) -> impl IntoResponse {
    let trades = state.engine.recent_trades().await;
    let trades_json: Vec<serde_json::Value> = trades.iter().rev().take(100).map(|t| {
        serde_json::json!({
            "trade_id": t.trade_id,
            "price": t.price.to_string(),
            "size": t.size.to_string(),
            "taker_side": format!("{}", t.taker_side),
            "timestamp_ms": t.timestamp_ms,
        })
    }).collect();
    ok(serde_json::json!({ "trades": trades_json })).into_response()
}

async fn get_funding(
    State(state): State<Arc<AppState>>,
    Path(_market): Path<String>,
) -> impl IntoResponse {
    let rate_raw = state.funding_rate.load(Ordering::Relaxed);
    let mark_raw = state.mark_price.load(Ordering::Relaxed);
    let last_ts = state.last_funding_time.load(Ordering::Relaxed);

    ok(serde_json::json!({
        "funding_rate": FP8(rate_raw).to_string(),
        "mark_price": FP8(mark_raw).to_string(),
        "next_funding_time": last_ts + 8 * 3600,
        "interval_hours": 8,
    })).into_response()
}

async fn withdraw(
    State(state): State<Arc<AppState>>,
    Json(req): Json<crate::withdrawal::WithdrawRequest>,
) -> impl IntoResponse {
    // TODO: escrow_account_id and session_key should come from config
    let escrow_account_id = &state.escrow_address;
    let session_key = "00".repeat(32); // placeholder — real value from enclave account

    match crate::withdrawal::process_withdrawal(
        &state.perp,
        &state.xrpl_url,
        &state.escrow_address,
        escrow_account_id,
        &session_key,
        &req,
    )
    .await
    {
        Ok(result) => {
            let code = if result.status == "success" {
                StatusCode::OK
            } else {
                StatusCode::BAD_REQUEST
            };
            (code, Json(serde_json::to_value(result).unwrap())).into_response()
        }
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()).into_response(),
    }
}

async fn get_markets(
    State(state): State<Arc<AppState>>,
) -> impl IntoResponse {
    let mark_raw = state.mark_price.load(Ordering::Relaxed);
    let (bid, ask, _) = state.engine.ticker().await;

    ok(serde_json::json!({
        "markets": [{
            "market": "XRP-RLUSD-PERP",
            "base": "XRP",
            "quote": "RLUSD",
            "mark_price": FP8(mark_raw).to_string(),
            "best_bid": bid.map(|p| p.to_string()),
            "best_ask": ask.map(|p| p.to_string()),
            "max_leverage": 20,
            "maintenance_margin": "0.00500000",
            "taker_fee": "0.00050000",
            "funding_interval_hours": 8,
            "status": "active",
        }]
    })).into_response()
}
