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
    /// Create a new client. TLS verification is relaxed because the
    /// enclave serves a self-signed cert; `ensure_loopback_url` gates
    /// that relaxation on the URL actually being loopback (O-L4).
    pub fn new(base_url: &str) -> Result<Self> {
        crate::http_helpers::ensure_loopback_url(base_url)
            .context("PerpClient requires a loopback enclave URL (O-L4)")?;
        let client = crate::http_helpers::loopback_http_client(std::time::Duration::from_secs(30))
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

    /// Credit user margin after verified XRPL deposit (native XRP).
    ///
    /// REQ-20-impl R2: `sender_addr` is the XRPL `tx.Account` from the
    /// detected Payment (semantic rename from `user_id` — the enclave
    /// now routes the credit based on `destination_tag` + bindings).
    ///
    /// When `destination_tag` is `Some(t)`, the enclave consults the
    /// `deposit_bindings` map:
    ///   - binding for `(sender_addr, t)` found → credit to bound user_id
    ///   - no binding → credit to `__unclaimed__` (held until the user
    ///     calls `POST /v1/deposit-binding` per REQ-20 §2.5a)
    ///
    /// The HTTP response includes a `credited_user_id` field with the
    /// resolved target; callers may surface it to logs / DB mirror.
    pub async fn deposit(
        &self,
        sender_addr: &str,
        amount: &str,
        xrpl_tx_hash: &str,
        destination_tag: Option<u32>,
    ) -> Result<Value> {
        let mut body = serde_json::json!({
            "user_id": sender_addr,
            "amount": amount,
            "xrpl_tx_hash": xrpl_tx_hash,
        });
        if let Some(t) = destination_tag {
            body["dest_tag"] = serde_json::json!(t);
        }
        self.post("/perp/deposit", body).await
    }

    /// Credit user XRP collateral (valued at mark_price × 90% haircut).
    ///
    /// Same REQ-20-impl R2 plumbing as [`deposit`]; XRP-asset variant.
    /// Note: per R1 known limitation L-IMPL-1, XRP-asset bindings are
    /// not yet supported by the enclave's binding-move ecall (it
    /// operates on `margin_balance` only). XRP-asset probes will
    /// surface `DB_INVARIANT_VIOLATION` on binding registration until
    /// follow-up commit adds asset-class indicator to DepositLogEntry.
    #[allow(dead_code)]
    pub async fn deposit_xrp(
        &self,
        sender_addr: &str,
        xrp_amount: &str,
        xrpl_tx_hash: &str,
        destination_tag: Option<u32>,
    ) -> Result<Value> {
        let mut body = serde_json::json!({
            "user_id": sender_addr,
            "xrp_amount": xrp_amount,
            "xrpl_tx_hash": xrpl_tx_hash,
        });
        if let Some(t) = destination_tag {
            body["dest_tag"] = serde_json::json!(t);
        }
        self.post("/perp/deposit-xrp", body).await
    }

    /// REQ-20-impl R2 commit 2 — register a deposit binding via the
    /// enclave's `ecall_perp_register_deposit_binding`.
    ///
    /// All identity material flows through the existing
    /// `OrderSignatureBinding` auth surface; the orchestrator computes
    /// the 20-byte AccountID from the signer pubkey and passes it
    /// alongside the r-address — the enclave re-derives + constant-time
    /// compares per design choice (b) of issue #45.
    ///
    /// Returns the JSON response from the enclave server, which carries
    /// the structured DB_* result code via `result_code`/`code` field
    /// and an optional `bound_at_ms` on success.
    #[allow(clippy::too_many_arguments)]
    pub async fn register_deposit_binding(
        &self,
        user_id: &str,
        user_account_id_hex: &str,
        sender_addr: &str,
        dest_tag: u32,
        probe_tx_hash_hex: &str,
        signed_body_hex: &str,
        signature_hex: &str,
        signer_pubkey_hex: &str,
    ) -> Result<Value> {
        self.post(
            "/perp/deposit-binding",
            serde_json::json!({
                "user_id": user_id,
                "user_account_id_hex": user_account_id_hex,
                "sender_addr": sender_addr,
                "dest_tag": dest_tag,
                "probe_tx_hash_hex": probe_tx_hash_hex,
                "signed_body_hex": signed_body_hex,
                "signature_hex": signature_hex,
                "signer_pubkey_hex": signer_pubkey_hex,
            }),
        )
        .await
    }

    /// Atomic margin check + XRPL withdrawal tx signing.
    #[allow(dead_code)]
    /// β4 Thread A (AC-β4-A2): `tx_blob` is the for-signing serialization of the
    /// Payment — the enclave re-derives the signing hash itself and refuses any
    /// non-Payment. A bare hash is no longer accepted here.
    pub async fn withdraw(
        &self,
        user_id: &str,
        amount: &str,
        escrow_account_id: &str,
        session_key: &str,
        tx_blob: &str,
    ) -> Result<Value> {
        self.post(
            "/perp/withdraw",
            serde_json::json!({
                "user_id": user_id,
                "amount": amount,
                "escrow_account_id": escrow_account_id,
                "session_key": session_key,
                "tx_blob": tx_blob,
            }),
        )
        .await
    }

    /// #131 Tier-1 reserves commit — the sequencer's enclave computes the
    /// proof-of-liabilities root over its sealed state and signs the Gnosis-Safe
    /// EIP-712 tx-hash for `publishReserves`. Returns 500 if custody < liabilities.
    /// `account_id` keeps its `0x` (the enclave requires the 42-char form); the
    /// `session_key` and the two addresses are sent 0x-stripped (canonical, matching
    /// the enclave hex parser + the typed-sign session-key discipline).
    #[allow(dead_code)] // wired by the 3d publisher
    #[allow(clippy::too_many_arguments)]
    pub async fn reserves_commit(
        &self,
        account_id: &str,
        session_key: &str,
        epoch: u64,
        safe_address: &str,
        chain_id: u64,
        registry_address: &str,
        safe_nonce: u64,
    ) -> Result<Value> {
        self.post(
            "/perp/reserves-commit",
            serde_json::json!({
                "account_id": account_id,
                "session_key": session_key.trim_start_matches("0x"),
                "epoch": epoch,
                "safe_address": safe_address.trim_start_matches("0x"),
                "chain_id": chain_id,
                "registry_address": registry_address.trim_start_matches("0x"),
                "safe_nonce": safe_nonce,
            }),
        )
        .await
    }

    /// AC-BASE: this node attests the opening escrow figure (issuer/account-pinned
    /// message signed in-enclave). Returns {signature:{r,s,v}}.
    #[allow(clippy::too_many_arguments)]
    pub async fn reserves_baseline_sign(
        &self,
        account_id: &str,
        session_key: &str,
        ledger_index: u64,
        escrow_account: &str,
        rlusd_issuer: &str,
        escrow_rlusd: i64,
        escrow_xrp: i64,
    ) -> Result<Value> {
        self.post(
            "/perp/reserves-baseline/sign",
            serde_json::json!({
                "account_id": account_id,
                "session_key": session_key.trim_start_matches("0x"),
                "ledger_index": ledger_index,
                "escrow_account": escrow_account.trim_start_matches("0x"),
                "rlusd_issuer": rlusd_issuer.trim_start_matches("0x"),
                "escrow_rlusd": escrow_rlusd,
                "escrow_xrp": escrow_xrp,
            }),
        )
        .await
    }

    /// AC-BASE: apply the one-time baseline — verify the 2-of-3 quorum over the pinned
    /// message, seed custody := attested escrow, seal the one-shot marker.
    #[allow(clippy::too_many_arguments)]
    pub async fn reserves_baseline_apply(
        &self,
        ledger_index: u64,
        escrow_account: &str,
        rlusd_issuer: &str,
        escrow_rlusd: i64,
        escrow_xrp: i64,
        host_timestamp_ms: u64,
        quorum_bundle_hex: &str,
    ) -> Result<Value> {
        self.post(
            "/perp/reserves-baseline/apply",
            serde_json::json!({
                "ledger_index": ledger_index,
                "escrow_account": escrow_account.trim_start_matches("0x"),
                "rlusd_issuer": rlusd_issuer.trim_start_matches("0x"),
                "escrow_rlusd": escrow_rlusd,
                "escrow_xrp": escrow_xrp,
                "host_timestamp_ms": host_timestamp_ms,
                "quorum_bundle": quorum_bundle_hex.trim_start_matches("0x"),
            }),
        )
        .await
    }

    /// Query user margin, positions, unrealized PnL.
    pub async fn get_balance(&self, user_id: &str) -> Result<Value> {
        self.get(&format!("/perp/balance?user_id={user_id}")).await
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
    #[allow(dead_code)]
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

    // ── Shard identity ──────────────────────────────────────────

    /// Set shard_id on the enclave (called once at startup).
    pub async fn set_shard_id(&self, shard_id: u32) -> Result<Value> {
        self.post(
            "/perp/shard/set",
            serde_json::json!({ "shard_id": shard_id }),
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
