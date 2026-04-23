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
use tracing::{debug, info, warn};

use crate::api::AppState;
use crate::orderbook::OrderType;
use crate::types::{Side, FP8};

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
    /// O-M5: kill-switch cap on aggregate vault inventory (in XRP).
    /// A one-sided sweep would otherwise pyramid the vault's position
    /// without bound; this cap pauses placement on levels whose fill
    /// would push the inventory metric over the limit. For MarketMaking
    /// the metric is gross inventory (sum of |position sizes|); for
    /// DeltaNeutral it is |net delta|, since the hedge cancels.
    #[serde(default = "default_max_inventory")]
    pub max_inventory: f64,
}

fn default_vault_user_id() -> String {
    "vault:mm".into()
}
fn default_half_spread() -> f64 {
    0.0025
}
fn default_order_size() -> String {
    "100.00000000".into()
}
fn default_interval() -> u64 {
    5
}
fn default_initial_deposit() -> String {
    "10000.00000000".into()
}
fn default_levels() -> usize {
    3
}
fn default_strategy() -> VaultStrategy {
    VaultStrategy::MarketMaking
}
fn default_max_delta() -> f64 {
    500.0
}
fn default_max_inventory() -> f64 {
    50.0
}

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
            max_inventory: default_max_inventory(),
        }
    }
}

/// O-M5: return the index set of levels that can be placed without
/// exceeding `max_inventory`. Each entry is `true` if the level is
/// safe to place. If every entry is `false`, the caller should pause
/// quoting until inventory drains.
fn levels_to_place(inventory_metric: f64, max_inventory: f64, level_sizes: &[FP8]) -> Vec<bool> {
    level_sizes
        .iter()
        .map(|s| inventory_metric + s.to_f64() <= max_inventory)
        .collect()
}

/// Seed the vault user with initial margin in the enclave.
pub async fn seed_vault_deposit(perp: &crate::perp_client::PerpClient, config: &VaultMmConfig) {
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
                let avail_str = bal["data"]["available_margin"].as_str().unwrap_or("0");
                let avail: f64 = avail_str.parse().unwrap_or(0.0);
                if avail <= 0.0 {
                    debug!(user = %config.user_id, "vault MM: no available margin");
                    continue;
                }
                // 1% of available margin / number of levels = per-level size
                let per_level = avail * size_pct / config.levels as f64;
                let sized = FP8::from_f64(per_level);
                if sized.raw() <= 0 {
                    fallback_size
                } else {
                    sized
                }
            }
            Err(_) => fallback_size,
        };

        // Delta Neutral: compute net position delta to decide quoting bias
        let (quote_bids, quote_asks) = if config.strategy == VaultStrategy::DeltaNeutral {
            let net_delta = compute_net_delta(&state.perp, &config.user_id).await;
            if net_delta > config.max_delta {
                // Too long → only sell (asks) to reduce
                debug!(
                    net_delta,
                    max = config.max_delta,
                    "vault DN: over max delta, asks only"
                );
                (false, true)
            } else if net_delta < -config.max_delta {
                // Too short → only buy (bids) to reduce
                debug!(
                    net_delta,
                    max = config.max_delta,
                    "vault DN: under -max delta, bids only"
                );
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

        // Fixed pyramid sizes per level (split across both vaults: mm + dn)
        // Target visible book: 3.8 / 7.6 / 15.2 — each vault places half
        // Reduced 10x for mainnet safety (limits max position size)
        let fixed_sizes: [f64; 3] = [1.9, 3.8, 7.6];

        // O-M5: compute the inventory metric before quoting. A one-sided
        // sweep would otherwise pyramid without bound; the cap below
        // pauses placement on levels whose size would push us over.
        let inventory_metric = match config.strategy {
            VaultStrategy::MarketMaking => {
                compute_gross_inventory(&state.perp, &config.user_id).await
            }
            VaultStrategy::DeltaNeutral => {
                compute_net_delta(&state.perp, &config.user_id).await.abs()
            }
        };

        // Precompute level sizes so we can query the cap per level.
        let level_sizes: Vec<FP8> = (0..config.levels)
            .map(|level| {
                if level < fixed_sizes.len() {
                    FP8::from_f64(fixed_sizes[level])
                } else {
                    order_size
                }
            })
            .collect();
        let place_mask = levels_to_place(inventory_metric, config.max_inventory, &level_sizes);
        if place_mask.iter().all(|b| !*b) {
            warn!(
                user = %config.user_id,
                inventory = inventory_metric,
                cap = config.max_inventory,
                "vault: inventory cap reached, pausing quoting until it drains"
            );
            continue;
        }

        // Place levels on each side
        for level in 0..config.levels {
            if !place_mask[level] {
                debug!(
                    level,
                    inventory = inventory_metric,
                    cap = config.max_inventory,
                    "vault: level skipped to stay under inventory cap"
                );
                continue;
            }

            let spread_mult = config.half_spread * (1.0 + level as f64 * 0.5);
            let bid_price = FP8::from_f64(mark_f * (1.0 - spread_mult));
            let ask_price = FP8::from_f64(mark_f * (1.0 + spread_mult));

            if bid_price.raw() <= 0 || ask_price.raw() <= 0 {
                continue;
            }

            let level_size = level_sizes[level];

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
                        Some(format!("vault-bid-{level}")),
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
                        Some(format!("vault-ask-{level}")),
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
async fn compute_net_delta(perp: &crate::perp_client::PerpClient, user_id: &str) -> f64 {
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

/// Compute gross vault inventory: sum of |position sizes| across all
/// open positions. Used by the MM-mode inventory cap (O-M5) — a
/// one-sided sweep shows up as growing gross inventory regardless of
/// which side accumulated.
async fn compute_gross_inventory(perp: &crate::perp_client::PerpClient, user_id: &str) -> f64 {
    let bal = match perp.get_balance(user_id).await {
        Ok(b) => b,
        Err(_) => return 0.0,
    };
    let positions = match bal["data"]["positions"].as_array() {
        Some(arr) => arr,
        None => return 0.0,
    };
    let mut gross: f64 = 0.0;
    for pos in positions {
        let size: f64 = pos["size"]
            .as_str()
            .and_then(|s| s.parse().ok())
            .unwrap_or(0.0);
        gross += size.abs();
    }
    gross
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn levels_to_place_empty_inventory_allows_all() {
        let sizes = vec![FP8::from_f64(1.9), FP8::from_f64(3.8), FP8::from_f64(7.6)];
        let mask = levels_to_place(0.0, 50.0, &sizes);
        assert_eq!(mask, vec![true, true, true]);
    }

    #[test]
    fn levels_to_place_caps_largest_first() {
        let sizes = vec![FP8::from_f64(1.9), FP8::from_f64(3.8), FP8::from_f64(7.6)];
        // 45 + 1.9=46.9, 45+3.8=48.8, 45+7.6=52.6 → only the last is capped
        let mask = levels_to_place(45.0, 50.0, &sizes);
        assert_eq!(mask, vec![true, true, false]);
    }

    #[test]
    fn levels_to_place_one_sided_sweep_eventually_pauses_quoting() {
        // Simulate a one-sided sweep: inventory ratchets up by the
        // pyramid sum each rebalance. Assert that the cap bites before
        // it runs away (the "cap holds" property the audit asks for).
        let sizes = vec![FP8::from_f64(1.9), FP8::from_f64(3.8), FP8::from_f64(7.6)];
        let cap = 50.0;
        let mut inventory = 0.0;
        let mut rebalances = 0;
        loop {
            let mask = levels_to_place(inventory, cap, &sizes);
            if mask.iter().all(|b| !*b) {
                break;
            }
            for (i, placed) in mask.iter().enumerate() {
                if *placed {
                    inventory += sizes[i].to_f64();
                }
            }
            rebalances += 1;
            assert!(
                rebalances < 20,
                "cap should bite within a finite number of rebalances"
            );
        }
        // Once we pause, inventory must be bounded above by cap plus one
        // level size (the last placement can straddle the cap by less
        // than the largest level size).
        assert!(
            inventory <= cap + sizes.iter().map(|s| s.to_f64()).fold(0.0, f64::max),
            "inventory exceeded cap by more than one level size: {inventory}"
        );
    }

    #[test]
    fn levels_to_place_cap_exactly_equal_is_allowed() {
        let sizes = vec![FP8::from_f64(5.0)];
        let mask = levels_to_place(45.0, 50.0, &sizes); // 45 + 5 == 50
        assert_eq!(mask, vec![true]);
    }
}
