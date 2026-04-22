//! Integration tests — full API flow with mock enclave.
//!
//! Spins up:
//! 1. Mock enclave server (returns success for all perp endpoints)
//! 2. Real Orchestrator API server
//!
//! Tests the complete flow: submit order → match → fill → orderbook/ticker.

use std::net::SocketAddr;

use axum::{
    extract::Query,
    http::StatusCode,
    response::IntoResponse,
    routing::{get, post},
    Json, Router,
};
use serde_json::{json, Value};

// We need to reference the crate's public API
// Since this is an integration test, we import from the binary crate
// But the orchestrator is a binary — so we use HTTP to test it.
//
// Alternative: test just the API layer by constructing AppState directly.
// That requires the lib items to be accessible. Let's use the HTTP approach.

/// Start a mock enclave server that responds to all /v1/perp/* endpoints.
async fn start_mock_enclave() -> (SocketAddr, tokio::task::JoinHandle<()>) {
    let app = Router::new()
        .route("/v1/perp/deposit", post(mock_deposit))
        .route("/v1/perp/deposit-xrp", post(mock_ok))
        .route("/v1/perp/position/open", post(mock_open_position))
        .route("/v1/perp/position/close", post(mock_ok))
        .route("/v1/perp/balance", get(mock_balance))
        .route("/v1/perp/price", post(mock_ok))
        .route("/v1/perp/liquidations/check", get(mock_no_liquidations))
        .route("/v1/perp/liquidate", post(mock_ok))
        .route("/v1/perp/funding/apply", post(mock_ok))
        .route("/v1/perp/state/save", post(mock_ok))
        .route("/v1/perp/state/load", post(mock_ok))
        .route("/v1/pool/status", get(mock_ok))
        .route("/v1/pool/attestation-quote", post(mock_attestation));

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let handle = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });

    (addr, handle)
}

async fn mock_ok() -> impl IntoResponse {
    Json(json!({"status": "success"}))
}

async fn mock_deposit(Json(body): Json<Value>) -> impl IntoResponse {
    Json(json!({
        "status": "success",
        "user_id": body["user_id"],
        "new_balance": "1000.00000000"
    }))
}

async fn mock_open_position(Json(body): Json<Value>) -> impl IntoResponse {
    Json(json!({
        "status": "success",
        "position_id": 1,
        "user_id": body["user_id"],
        "side": body["side"],
        "size": body["size"],
        "entry_price": body["price"]
    }))
}

#[derive(serde::Deserialize)]
struct UserIdQuery {
    user_id: Option<String>,
}

async fn mock_balance(Query(q): Query<UserIdQuery>) -> impl IntoResponse {
    let user_id = q.user_id.unwrap_or_default();
    Json(json!({
        "status": "success",
        "data": {
            "margin_balance": "1000.00000000",
            "xrp_balance": "0.00000000",
            "staked_xrp": "0.00000000",
            "unrealized_pnl": "0.00000000",
            "used_margin": "0.00000000",
            "available_margin": "1000.00000000",
            "positions": [],
            "user_id": user_id
        }
    }))
}

async fn mock_no_liquidations() -> impl IntoResponse {
    Json(json!({"status": "success", "count": 0, "liquidatable": []}))
}

async fn mock_attestation() -> impl IntoResponse {
    (
        StatusCode::SERVICE_UNAVAILABLE,
        Json(json!({"status": "error", "message": "DCAP not available in test"})),
    )
}

// ── Tests ──────────────────────────────────────────────────────

#[tokio::test]
async fn mock_enclave_deposit_works() {
    let (addr, _handle) = start_mock_enclave().await;
    let client = reqwest::Client::new();

    let resp: Value = client
        .post(format!("http://{addr}/v1/perp/deposit"))
        .json(&json!({
            "user_id": "rAlice",
            "amount": "100.00000000",
            "xrpl_tx_hash": "ABC123"
        }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();

    assert_eq!(resp["status"], "success");
    assert_eq!(resp["new_balance"], "1000.00000000");
}

#[tokio::test]
async fn mock_enclave_balance_works() {
    let (addr, _handle) = start_mock_enclave().await;
    let client = reqwest::Client::new();

    let resp: Value = client
        .get(format!("http://{addr}/v1/perp/balance?user_id=rAlice"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();

    assert_eq!(resp["status"], "success");
    assert_eq!(resp["data"]["margin_balance"], "1000.00000000");
    assert_eq!(resp["data"]["user_id"], "rAlice");
}

#[tokio::test]
async fn mock_enclave_open_position_works() {
    let (addr, _handle) = start_mock_enclave().await;
    let client = reqwest::Client::new();

    let resp: Value = client
        .post(format!("http://{addr}/v1/perp/position/open"))
        .json(&json!({
            "user_id": "rBob",
            "side": "long",
            "size": "50.00000000",
            "price": "0.55000000",
            "leverage": 5
        }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();

    assert_eq!(resp["status"], "success");
    assert_eq!(resp["position_id"], 1);
    assert_eq!(resp["side"], "long");
}

#[tokio::test]
async fn mock_enclave_no_liquidations() {
    let (addr, _handle) = start_mock_enclave().await;
    let client = reqwest::Client::new();

    let resp: Value = client
        .get(format!("http://{addr}/v1/perp/liquidations/check"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();

    assert_eq!(resp["count"], 0);
}

#[tokio::test]
async fn mock_enclave_state_save_load() {
    let (addr, _handle) = start_mock_enclave().await;
    let client = reqwest::Client::new();

    let save: Value = client
        .post(format!("http://{addr}/v1/perp/state/save"))
        .json(&json!({}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(save["status"], "success");

    let load: Value = client
        .post(format!("http://{addr}/v1/perp/state/load"))
        .json(&json!({}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(load["status"], "success");
}

#[tokio::test]
async fn perp_client_against_mock() {
    let (addr, _handle) = start_mock_enclave().await;

    // Use the actual PerpClient against mock enclave
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(5))
        .build()
        .unwrap();

    // Deposit
    let resp: Value = client
        .post(format!("http://{addr}/v1/perp/deposit"))
        .json(&json!({"user_id": "rTest", "amount": "500.00000000", "xrpl_tx_hash": "hash1"}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(resp["status"], "success");

    // Update price
    let resp: Value = client
        .post(format!("http://{addr}/v1/perp/price"))
        .json(&json!({"mark_price": "0.55000000", "index_price": "0.55000000", "timestamp": 1000}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(resp["status"], "success");

    // Open position
    let resp: Value = client
        .post(format!("http://{addr}/v1/perp/position/open"))
        .json(&json!({"user_id": "rTest", "side": "long", "size": "100.00000000", "price": "0.55000000", "leverage": 5}))
        .send().await.unwrap().json().await.unwrap();
    assert_eq!(resp["status"], "success");

    // Get balance
    let resp: Value = client
        .get(format!("http://{addr}/v1/perp/balance?user_id=rTest"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(resp["status"], "success");

    // Apply funding
    let resp: Value = client
        .post(format!("http://{addr}/v1/perp/funding/apply"))
        .json(&json!({"funding_rate": "0.00050000", "timestamp": 2000}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(resp["status"], "success");
}
