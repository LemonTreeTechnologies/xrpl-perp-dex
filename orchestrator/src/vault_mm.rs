//! Market Making Vault — automated liquidity provision on the CLOB.
//!
//! Runs as a background tokio task inside the orchestrator. Every
//! `rebalance_interval` seconds it cancels stale orders and places fresh
//! limit buy + sell around the current mark price with a configurable
//! spread.
//!
//! The vault is a regular user from the trading engine's perspective — it
//! has its own margin balance in the enclave and submits orders through
//! `TradingEngine::submit_order`. No special treatment in the matching
//! engine.
//!
//! Designed per Tom's vault-design-spec.md (PR #4), type 1 "Market Making
//! Vault — low risk".

use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tracing::{debug, error, info, warn};

use crate::api::AppState;
use crate::orderbook::OrderType;
use crate::types::{FP8, Side};

/// Configuration for the Market Making Vault.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct VaultMmConfig {
    /// The vault's user_id in the trading engine / enclave.
    #[serde(default = "default_vault_user_id")]
    pub user_id: String,
    /// Half-spread as a fraction (e.g. 0.0025 = 0.25% each side, 0.5% total).
    #[serde(default = "default_half_spread")]
    pub half_spread: f64,
    /// Order size in XRP (FP8 string).
    #[serde(default = "default_order_size")]
    pub order_size: String,
    /// Seconds between rebalances.
    #[serde(default = "default_interval")]
    pub interval_secs: u64,
    /// Initial margin to deposit for the vault on startup (FP8 string).
    #[serde(default = "default_initial_deposit")]
    pub initial_deposit: String,
    /// Max number of open order levels per side.
    #[serde(default = "default_levels")]
    pub levels: usize,
}

fn default_vault_user_id() -> String { "vault:mm".into() }
fn default_half_spread() -> f64 { 0.0025 }
fn default_order_size() -> String { "100.00000000".into() }
fn default_interval() -> u64 { 5 }
fn default_initial_deposit() -> String { "10000.00000000".into() }
fn default_levels() -> usize { 3 }

impl Default for VaultMmConfig {
    fn default() -> Self {
        VaultMmConfig {
            user_id: default_vault_user_id(),
            half_spread: default_half_spread(),
            order_size: default_order_size(),
            interval_secs: default_interval(),
            initial_deposit: default_initial_deposit(),
            levels: default_levels(),
        }
    }
}

/// Seed the vault user with initial margin in the enclave.
pub async fn seed_vault_deposit(
    perp: &crate::perp_client::PerpClient,
    config: &VaultMmConfig,
) {
    let tx_hash = format!(
        "{:064x}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
    );
    match perp
        .deposit(&config.user_id, &config.initial_deposit, &tx_hash)
        .await
    {
        Ok(_) => info!(
            user = %config.user_id,
            amount = %config.initial_deposit,
            "vault MM: seeded initial deposit"
        ),
        Err(e) => warn!(
            user = %config.user_id,
            "vault MM: seed deposit failed (may already exist): {}",
            e
        ),
    }
}

/// Run the market-making loop. Call via `tokio::spawn`.
pub async fn run_vault_mm(state: Arc<AppState>, config: VaultMmConfig) {
    let mut interval = tokio::time::interval(Duration::from_secs(config.interval_secs));
    // Fallback order size if balance query fails
    let fallback_size: FP8 = config.order_size.parse().unwrap_or(FP8(100_00000000));
    // Max fraction of available margin to allocate across ALL levels per side
    let size_pct: f64 = 0.01; // 1% of balance total, split across levels

    info!(
        user = %config.user_id,
        half_spread = config.half_spread,
        size_pct = size_pct,
        fallback_size = %fallback_size,
        interval = config.interval_secs,
        levels = config.levels,
        "vault MM started"
    );

    loop {
        interval.tick().await;

        // Only run if this node is the sequencer (validators don't submit orders)
        if !state.is_sequencer.load(Ordering::Relaxed) {
            continue;
        }

        let mark_raw = state.mark_price.load(Ordering::Relaxed);
        if mark_raw <= 0 {
            debug!("vault MM: no mark price yet, skipping");
            continue;
        }
        let mark = FP8(mark_raw);
        let mark_f = mark.to_f64();

        // Query vault's available margin from enclave to size orders as %
        let order_size = match state.perp.get_balance(&config.user_id).await {
            Ok(bal) => {
                let avail_str = bal["data"]["available_margin"]
                    .as_str()
                    .unwrap_or("0");
                let avail: f64 = avail_str.parse().unwrap_or(0.0);
                if avail <= 0.0 {
                    debug!(user = %config.user_id, "vault MM: no available margin");
                    continue;
                }
                // 1% of available margin / number of levels = per-level size
                let per_level = avail * size_pct / config.levels as f64;
                let sized = FP8::from_f64(per_level);
                if sized.raw() <= 0 { fallback_size } else { sized }
            }
            Err(_) => fallback_size,
        };

        // Cancel all existing vault orders
        let cancelled = state.engine.cancel_all(&config.user_id).await;
        if !cancelled.is_empty() {
            debug!(cancelled = cancelled.len(), "vault MM: cancelled stale orders");
        }

        // Place levels on each side
        for level in 0..config.levels {
            let spread_mult = config.half_spread * (1.0 + level as f64 * 0.5);
            let bid_price = FP8::from_f64(mark_f * (1.0 - spread_mult));
            let ask_price = FP8::from_f64(mark_f * (1.0 + spread_mult));

            if bid_price.raw() <= 0 || ask_price.raw() <= 0 {
                continue;
            }

            // Place bid
            if let Err(e) = state
                .engine
                .submit_order(
                    config.user_id.clone(),
                    Side::Long,
                    OrderType::Limit,
                    bid_price,
                    order_size,
                    1, // leverage
                    crate::orderbook::TimeInForce::Gtc,
                    false,
                    Some(format!("vault-mm-bid-{}", level)),
                )
                .await
            {
                warn!(level, price = %bid_price, "vault MM bid failed: {}", e);
            }

            // Place ask
            if let Err(e) = state
                .engine
                .submit_order(
                    config.user_id.clone(),
                    Side::Short,
                    OrderType::Limit,
                    ask_price,
                    order_size,
                    1,
                    crate::orderbook::TimeInForce::Gtc,
                    false,
                    Some(format!("vault-mm-ask-{}", level)),
                )
                .await
            {
                warn!(level, price = %ask_price, "vault MM ask failed: {}", e);
            }
        }

        debug!(
            mark = %mark,
            levels = config.levels,
            "vault MM: placed fresh quotes"
        );
    }
}
