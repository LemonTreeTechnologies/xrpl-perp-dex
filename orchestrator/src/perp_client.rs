//! HTTP client for the Perp DEX enclave REST API.
//!
//! Rewrite of `perp_client.py`. All amounts are strings in FP8 format
//! (e.g., "100.50000000").

use anyhow::{Context, Result};
use serde_json::Value;

/// Client for the Perp DEX enclave REST API at `/v1/perp/*`.
pub struct PerpClient {
    base_url: String,
    client: reqwest::Client,
}

impl PerpClient {
    /// Create a new client. TLS verification is disabled (self-signed cert).
    pub fn new(base_url: &str) -> Result<Self> {
        let client = reqwest::Client::builder()
            .danger_accept_invalid_certs(true)
            .timeout(std::time::Duration::from_secs(30))
            .build()
            .context("failed to build reqwest client")?;

        Ok(Self {
            base_url: base_url.trim_end_matches('/').to_string(),
            client,
        })
    }

    /// Get base URL for proxying.
    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    // ── State management ────────────────────────────────────────

    /// Credit user RLUSD margin after verified XRPL deposit.
    pub async fn deposit(
        &self,
        user_id: &str,
        amount: &str,
        xrpl_tx_hash: &str,
    ) -> Result<Value> {
        self.post(
            "/perp/deposit",
            serde_json::json!({
                "user_id": user_id,
                "amount": amount,
                "xrpl_tx_hash": xrpl_tx_hash,
            }),
        )
        .await
    }

    /// Credit user XRP collateral (valued at mark_price × 90% haircut).
    pub async fn deposit_xrp(
        &self,
        user_id: &str,
        xrp_amount: &str,
        xrpl_tx_hash: &str,
    ) -> Result<Value> {
        self.post(
            "/perp/deposit-xrp",
            serde_json::json!({
                "user_id": user_id,
                "xrp_amount": xrp_amount,
                "xrpl_tx_hash": xrpl_tx_hash,
            }),
        )
        .await
    }

    /// Atomic margin check + XRPL withdrawal tx signing.
    pub async fn withdraw(
        &self,
        user_id: &str,
        amount: &str,
        escrow_account_id: &str,
        session_key: &str,
        tx_hash: &str,
    ) -> Result<Value> {
        self.post(
            "/perp/withdraw",
            serde_json::json!({
                "user_id": user_id,
                "amount": amount,
                "escrow_account_id": escrow_account_id,
                "session_key": session_key,
                "tx_hash": tx_hash,
            }),
        )
        .await
    }

    /// Query user margin, positions, unrealized PnL.
    pub async fn get_balance(&self, user_id: &str) -> Result<Value> {
        self.get(&format!("/perp/balance?user_id={}", user_id))
            .await
    }

    // ── Position management ─────────────────────────────────────

    /// Open long/short position with margin check.
    pub async fn open_position(
        &self,
        user_id: &str,
        side: &str,
        size: &str,
        price: &str,
        leverage: u32,
    ) -> Result<Value> {
        self.post(
            "/perp/position/open",
            serde_json::json!({
                "user_id": user_id,
                "side": side,
                "size": size,
                "price": price,
                "leverage": leverage,
            }),
        )
        .await
    }

    /// Close position, realize PnL.
    pub async fn close_position(
        &self,
        user_id: &str,
        position_id: u64,
        close_price: &str,
    ) -> Result<Value> {
        self.post(
            "/perp/position/close",
            serde_json::json!({
                "user_id": user_id,
                "position_id": position_id,
                "close_price": close_price,
            }),
        )
        .await
    }

    // ── Price & risk ────────────────────────────────────────────

    /// Update mark and index price.
    pub async fn update_price(
        &self,
        mark_price: &str,
        index_price: &str,
        timestamp: u64,
    ) -> Result<Value> {
        self.post(
            "/perp/price",
            serde_json::json!({
                "mark_price": mark_price,
                "index_price": index_price,
                "timestamp": timestamp,
            }),
        )
        .await
    }

    /// Scan for liquidatable positions.
    pub async fn check_liquidations(&self) -> Result<Value> {
        self.get("/perp/liquidations/check").await
    }

    /// Force-close undercollateralized position.
    pub async fn liquidate(&self, position_id: u64, close_price: &str) -> Result<Value> {
        self.post(
            "/perp/liquidate",
            serde_json::json!({
                "position_id": position_id,
                "close_price": close_price,
            }),
        )
        .await
    }

    // ── Funding ─────────────────────────────────────────────────

    /// Apply funding rate to all open positions.
    pub async fn apply_funding(&self, funding_rate: &str, timestamp: u64) -> Result<Value> {
        self.post(
            "/perp/funding/apply",
            serde_json::json!({
                "funding_rate": funding_rate,
                "timestamp": timestamp,
            }),
        )
        .await
    }

    // ── State persistence ───────────────────────────────────────

    /// Seal perp state to disk.
    pub async fn save_state(&self) -> Result<Value> {
        self.post("/perp/state/save", serde_json::json!({})).await
    }

    /// Unseal perp state from disk.
    pub async fn load_state(&self) -> Result<Value> {
        self.post("/perp/state/load", serde_json::json!({})).await
    }

    // ── Internal ────────────────────────────────────────────────

    async fn post(&self, path: &str, body: Value) -> Result<Value> {
        let url = format!("{}{}", self.base_url, path);
        let resp: Value = self
            .client
            .post(&url)
            .json(&body)
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        Ok(resp)
    }

    async fn get(&self, path: &str) -> Result<Value> {
        let url = format!("{}{}", self.base_url, path);
        let resp: Value = self
            .client
            .get(&url)
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        Ok(resp)
    }
}
