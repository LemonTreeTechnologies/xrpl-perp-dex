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

/// Vault strategy type.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum VaultStrategy {
    /// Quote both sides symmetrically around mark price.
    MarketMaking,
    /// Quote both sides but bias toward reducing net delta.
    /// If net long → heavier asks; if net short → heavier bids.
    /// Target: keep |net_delta| below max_delta.
    DeltaNeutral,
}

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
    /// Strategy: market_making (default) or delta_neutral.
    #[serde(default = "default_strategy")]
    pub strategy: VaultStrategy,
    /// Max acceptable net delta (in XRP, FP8). Beyond this the vault
    /// quotes one-sided to reduce exposure. Only used for delta_neutral.
    #[serde(default = "default_max_delta")]
    pub max_delta: f64,
}

fn default_vault_user_id() -> String { "vault:mm".into() }
fn default_half_spread() -> f64 { 0.0025 }
fn default_order_size() -> String { "100.00000000".into() }
fn default_interval() -> u64 { 5 }
fn default_initial_deposit() -> String { "10000.00000000".into() }
fn default_levels() -> usize { 3 }
fn default_strategy() -> VaultStrategy { VaultStrategy::MarketMaking }
fn default_max_delta() -> f64 { 500.0 }

impl Default for VaultMmConfig {
    fn default() -> Self {
        VaultMmConfig {
            user_id: default_vault_user_id(),
            half_spread: default_half_spread(),
            order_size: default_order_size(),
            interval_secs: default_interval(),
            initial_deposit: default_initial_deposit(),
            levels: default_levels(),
            strategy: default_strategy(),
            max_delta: default_max_delta(),
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

        // Delta Neutral: compute net position delta to decide quoting bias
        let (quote_bids, quote_asks) = if config.strategy == VaultStrategy::DeltaNeutral {
            let net_delta = compute_net_delta(&state.perp, &config.user_id).await;
            if net_delta > config.max_delta {
                // Too long → only sell (asks) to reduce
                debug!(net_delta, max = config.max_delta, "vault DN: over max delta, asks only");
                (false, true)
            } else if net_delta < -config.max_delta {
                // Too short → only buy (bids) to reduce
                debug!(net_delta, max = config.max_delta, "vault DN: under -max delta, bids only");
                (true, false)
            } else {
                (true, true)
            }
        } else {
            (true, true) // MM: always quote both sides
        };

        // Cancel all existing vault orders
        let cancelled = state.engine.cancel_all(&config.user_id).await;
        if !cancelled.is_empty() {
            debug!(cancelled = cancelled.len(), "vault: cancelled stale orders");
        }

        // Size multipliers per level — pyramid shape (small at top, large at bottom)
        // For 3 levels: weights [1, 4, 10] → sizes proportional to 25/100/250
        let level_weights: Vec<f64> = (0..config.levels)
            .map(|l| if l == 0 { 1.0 } else { (4.0_f64).powi(l as i32) })
            .collect();
        let total_weight: f64 = level_weights.iter().sum();

        // Place levels on each side
        for level in 0..config.levels {
            let spread_mult = config.half_spread * (1.0 + level as f64 * 0.5);
            let bid_price = FP8::from_f64(mark_f * (1.0 - spread_mult));
            let ask_price = FP8::from_f64(mark_f * (1.0 + spread_mult));

            if bid_price.raw() <= 0 || ask_price.raw() <= 0 {
                continue;
            }

            // Scale order size by level weight (pyramid: small tight, large wide)
            let level_size = FP8::from_f64(
                order_size.to_f64() * level_weights[level] / total_weight * config.levels as f64,
            );
            let level_size = if level_size.raw() <= 0 { order_size } else { level_size };

            // Place bid (skipped if delta neutral says "asks only")
            if quote_bids {
                if let Err(e) = state
                    .engine
                    .submit_order(
                        config.user_id.clone(),
                        Side::Long,
                        OrderType::Limit,
                        bid_price,
                        level_size,
                        1, // leverage
                        crate::orderbook::TimeInForce::Gtc,
                        false,
                        Some(format!("vault-bid-{}", level)),
                    )
                    .await
                {
                    warn!(level, price = %bid_price, "vault bid failed: {}", e);
                }
            }

            // Place ask (skipped if delta neutral says "bids only")
            if quote_asks {
                if let Err(e) = state
                    .engine
                    .submit_order(
                        config.user_id.clone(),
                        Side::Short,
                        OrderType::Limit,
                        ask_price,
                        level_size,
                        1,
                        crate::orderbook::TimeInForce::Gtc,
                        false,
                        Some(format!("vault-ask-{}", level)),
                    )
                    .await
                {
                    warn!(level, price = %ask_price, "vault ask failed: {}", e);
                }
            }
        }

        debug!(
            mark = %mark,
            levels = config.levels,
            quote_bids,
            quote_asks,
            "vault: placed fresh quotes"
        );
    }
}

/// Compute the vault's net delta (sum of long sizes - sum of short sizes).
/// Returns 0.0 if the query fails or the vault has no positions.
async fn compute_net_delta(
    perp: &crate::perp_client::PerpClient,
    user_id: &str,
) -> f64 {
    let bal = match perp.get_balance(user_id).await {
        Ok(b) => b,
        Err(_) => return 0.0,
    };
    let positions = match bal["data"]["positions"].as_array() {
        Some(arr) => arr,
        None => return 0.0,
    };
    let mut net: f64 = 0.0;
    for pos in positions {
        let size: f64 = pos["size"]
            .as_str()
            .and_then(|s| s.parse().ok())
            .unwrap_or(0.0);
        let side = pos["side"].as_str().unwrap_or("");
        match side {
            "long" | "1" => net += size,
            "short" | "2" => net -= size,
            _ => {}
        }
    }
    net
}
