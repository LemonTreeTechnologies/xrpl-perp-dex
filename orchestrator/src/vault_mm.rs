//! V1 vault: virtual-AMM-on-CLOB market making (Tom-spec, post-hackathon-specs.md Issue #3).
//!
//! Runs as a background tokio task inside the orchestrator. The vault is a
//! regular user from the enclave's perspective — it deposits collateral,
//! holds positions, and submits maker orders through the engine. No special
//! treatment in the matching engine.
//!
//! Pricing source: the vAMM curve. Tom (Q3.2): "refreshes should be based
//! on curve-implied price moves, not external mark moves. NOTE, THIS IS
//! REALLY IMPORTANT as it is the source of the arb flow."
//!
//! Curve math, ladder construction, and posture/hysteresis live in
//! `crate::vamm` — fully unit-tested there. This file is the long-running
//! loop: poll → trigger → cancel → place.

use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tracing::{debug, info, warn};

use crate::api::AppState;
use crate::orderbook::{OrderType, TimeInForce};
use crate::types::{Side, FP8};
use crate::vamm::{build_ladder, evaluate_posture, Curve, Posture};

/// V1 vault (vAMM) configuration.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct VaultMmConfig {
    /// Vault user_id in the enclave / CLOB.
    #[serde(default = "default_vault_user_id")]
    pub user_id: String,
    /// Seed deposit on startup (FP8 XRP string).
    #[serde(default = "default_initial_deposit")]
    pub initial_deposit: String,
    /// Curve depth multiplier: x_0 = depth_mult × collateral. Larger = less slippage per fill.
    #[serde(default = "default_depth_mult")]
    pub depth_mult: f64,
    /// Target delta as a fraction of collateral notional (positive = short bias). Tom-Q3.4. Default 0.5.
    #[serde(default = "default_target_delta_frac")]
    pub target_delta_frac: f64,
    /// Ladder levels per side. Tom-Q3.2 default 5.
    #[serde(default = "default_steps")]
    pub steps: usize,
    /// Spacing between ladder levels in basis points. Tom-Q3.2 default 10.
    #[serde(default = "default_step_bps")]
    pub step_bps: u32,
    /// Per-level size as a fraction of free collateral. Tom-Q3.2 default 0.1 (10%).
    #[serde(default = "default_level_size_pct")]
    pub level_size_pct: f64,
    /// Hard delta cap (fraction of collateral). Tom-Q3.4 / Q4.1. Default 2.0.
    #[serde(default = "default_delta_cap")]
    pub delta_cap: f64,
    /// Collateral utilization cap. Tom-Q4.1. Default 0.8.
    #[serde(default = "default_util_cap")]
    pub util_cap: f64,
    /// Hysteresis gap. Tom-Q4.2. Default 0.25.
    #[serde(default = "default_hysteresis")]
    pub hysteresis: f64,
    /// Curve-implied mid move (bps) that triggers a refresh. Default 5.
    #[serde(default = "default_refresh_bps")]
    pub refresh_bps: u32,
    /// Poll cadence (sec). Default 1.
    #[serde(default = "default_poll_interval")]
    pub poll_interval_secs: u64,
}

fn default_vault_user_id() -> String {
    "vault:vamm".into()
}
fn default_initial_deposit() -> String {
    "10000.00000000".into()
}
fn default_depth_mult() -> f64 {
    10.0
}
fn default_target_delta_frac() -> f64 {
    0.5
}
fn default_steps() -> usize {
    5
}
fn default_step_bps() -> u32 {
    10
}
fn default_level_size_pct() -> f64 {
    0.1
}
fn default_delta_cap() -> f64 {
    2.0
}
fn default_util_cap() -> f64 {
    0.8
}
fn default_hysteresis() -> f64 {
    0.25
}
fn default_refresh_bps() -> u32 {
    5
}
fn default_poll_interval() -> u64 {
    1
}

impl Default for VaultMmConfig {
    fn default() -> Self {
        VaultMmConfig {
            user_id: default_vault_user_id(),
            initial_deposit: default_initial_deposit(),
            depth_mult: default_depth_mult(),
            target_delta_frac: default_target_delta_frac(),
            steps: default_steps(),
            step_bps: default_step_bps(),
            level_size_pct: default_level_size_pct(),
            delta_cap: default_delta_cap(),
            util_cap: default_util_cap(),
            hysteresis: default_hysteresis(),
            refresh_bps: default_refresh_bps(),
            poll_interval_secs: default_poll_interval(),
        }
    }
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
            "vault vAMM: seeded initial deposit"
        ),
        Err(e) => warn!(
            user = %config.user_id,
            "vault vAMM: seed deposit failed (may already exist): {}",
            e
        ),
    }
}

/// Sum net XRP position from enclave balance JSON.
/// Positive = net long; negative = net short.
fn positions_to_net_xrp(bal: &serde_json::Value) -> f64 {
    let mut net: f64 = 0.0;
    if let Some(arr) = bal["data"]["positions"].as_array() {
        for p in arr {
            let size: f64 = p["size"]
                .as_str()
                .and_then(|s| s.parse().ok())
                .unwrap_or(0.0);
            let side = p["side"].as_str().unwrap_or("long");
            if size > 0.0 {
                if side == "long" || side == "buy" {
                    net += size;
                } else {
                    net -= size;
                }
            }
        }
    }
    net
}

/// Pick the largest absolute position to close when in UtilHot.
/// Returns (position_id, side, size_to_close). For V1 we close the entire
/// largest position; partial-close sizing under Tom-Q4.1 ("close only if
/// still over") is achieved by re-evaluating posture on the next tick.
fn pick_position_to_reduce(bal: &serde_json::Value) -> Option<(u32, Side, FP8)> {
    let arr = bal["data"]["positions"].as_array()?;
    let mut best: Option<(u32, Side, FP8, f64)> = None;
    for p in arr {
        let pid: u32 = p["id"].as_u64().unwrap_or(0) as u32;
        if pid == 0 {
            continue;
        }
        let size_f: f64 = p["size"]
            .as_str()
            .and_then(|s| s.parse().ok())
            .unwrap_or(0.0);
        if size_f <= 0.0 {
            continue;
        }
        let size_fp = FP8::from_f64(size_f);
        let side_str = p["side"].as_str().unwrap_or("long");
        let side = if side_str == "long" || side_str == "buy" {
            Side::Long
        } else {
            Side::Short
        };
        let abs = size_f.abs();
        if best.as_ref().map(|b| abs > b.3).unwrap_or(true) {
            best = Some((pid, side, size_fp, abs));
        }
    }
    best.map(|(pid, side, sz, _)| (pid, side, sz))
}

/// Run the vAMM market-making loop. Spawn via `tokio::spawn`.
pub async fn run_vault_mm(state: Arc<AppState>, config: VaultMmConfig) {
    // 1. Parse collateral.
    let collateral_xrp: f64 = config
        .initial_deposit
        .parse::<FP8>()
        .map(|f| f.to_f64())
        .unwrap_or(10_000.0);
    if collateral_xrp <= 0.0 {
        warn!("vault vAMM: zero collateral, exiting");
        return;
    }

    // 2. Wait for an initial mark price to anchor the curve.
    //    After init the curve is self-referential — external mark is never read.
    let mark_init: f64 = loop {
        let raw = state.mark_price.load(Ordering::Relaxed);
        if raw > 0 {
            break FP8(raw).to_f64();
        }
        debug!("vault vAMM: waiting for initial mark price");
        tokio::time::sleep(Duration::from_millis(500)).await;
    };

    let curve = Curve::new(
        collateral_xrp,
        config.depth_mult,
        mark_init,
        config.target_delta_frac,
    );

    info!(
        user = %config.user_id,
        collateral = collateral_xrp,
        mark_init,
        target_delta_frac = config.target_delta_frac,
        depth_mult = config.depth_mult,
        steps = config.steps,
        step_bps = config.step_bps,
        delta_cap = config.delta_cap,
        util_cap = config.util_cap,
        refresh_bps = config.refresh_bps,
        "vault vAMM initialized"
    );

    let mut last_position: f64 = 0.0;
    let mut last_mid: f64 = curve.implied_mid(0.0);
    let mut posture = Posture::Healthy;
    // Tom-Q4.1 cancel-first / close-second: we close a position only when
    // UtilHot persists into a second tick. Track that across iterations.
    let mut util_hot_streak: u32 = 0;

    let mut interval = tokio::time::interval(Duration::from_secs(config.poll_interval_secs.max(1)));

    loop {
        interval.tick().await;

        if !state.is_sequencer.load(Ordering::Relaxed) {
            continue;
        }

        let bal = match state.perp.get_balance(&config.user_id).await {
            Ok(b) => b,
            Err(e) => {
                debug!("vault vAMM: get_balance failed: {}", e);
                continue;
            }
        };

        let free_xrp: f64 = bal["data"]["available_margin"]
            .as_str()
            .and_then(|s| s.parse().ok())
            .unwrap_or(0.0);
        let net_position = positions_to_net_xrp(&bal);
        let net_delta_frac = net_position / collateral_xrp;
        let used = (collateral_xrp - free_xrp).max(0.0);
        let util = (used / collateral_xrp).clamp(0.0, 1.0);

        let new_posture = evaluate_posture(
            net_delta_frac,
            util,
            posture,
            config.hysteresis,
            config.delta_cap,
            config.util_cap,
        );

        let curve_mid = curve.implied_mid(net_position);
        let posture_changed = new_posture != posture;
        let fill_detected = (net_position - last_position).abs() > 1e-9;
        let mid_moved_bps = if last_mid > 0.0 {
            ((curve_mid - last_mid).abs() / last_mid) * 10_000.0
        } else {
            f64::INFINITY
        };
        let curve_moved = mid_moved_bps >= config.refresh_bps as f64;
        let trigger = posture_changed || fill_detected || curve_moved;

        if !trigger && !matches!(new_posture, Posture::UtilHot) {
            // No work this tick. Reset util_hot_streak when we go back to a non-hot posture.
            if !matches!(posture, Posture::UtilHot) {
                util_hot_streak = 0;
            }
            continue;
        }

        debug!(
            posture = ?new_posture,
            net_position,
            net_delta_frac,
            util,
            curve_mid,
            posture_changed,
            fill_detected,
            mid_moved_bps,
            "vault vAMM: refresh trigger"
        );

        // Cancel-first (Tom-Q4.1): always cancel resting orders on trigger.
        let cancelled = state.engine.cancel_all(&config.user_id).await;
        if !cancelled.is_empty() {
            debug!(
                cancelled = cancelled.len(),
                "vault vAMM: cancelled resting orders"
            );
        }

        // UtilHot: cancel first, close on a second consecutive UtilHot tick.
        if matches!(new_posture, Posture::UtilHot) {
            util_hot_streak += 1;
            if util_hot_streak >= 2 {
                if let Some((pid, side, size)) = pick_position_to_reduce(&bal) {
                    // Close via reduce_only IOC market on the opposing side.
                    let close_side = match side {
                        Side::Long => Side::Short,
                        Side::Short => Side::Long,
                    };
                    info!(
                        position_id = pid,
                        size = %size,
                        close_side = ?close_side,
                        "vault vAMM: UtilHot persistent — reducing largest position"
                    );
                    if let Err(e) = state
                        .engine
                        .submit_order(
                            config.user_id.clone(),
                            close_side,
                            OrderType::Market,
                            FP8::ZERO,
                            size,
                            1,
                            TimeInForce::Ioc,
                            true,
                            Some(format!("vamm-reduce-{pid}")),
                            Some("vAMM".into()),
                        )
                        .await
                    {
                        warn!("vault vAMM: reduce-position failed: {}", e);
                    }
                } else {
                    warn!("vault vAMM: UtilHot but no position to reduce");
                }
            } else {
                info!(
                    util,
                    cap = config.util_cap,
                    "vault vAMM: UtilHot — cancelled resting, observing one more tick before reducing"
                );
            }
            posture = new_posture;
            last_position = net_position;
            last_mid = curve_mid;
            continue;
        }

        util_hot_streak = 0;

        // Build + place ladder.
        let levels = build_ladder(
            &curve,
            net_position,
            free_xrp,
            config.steps,
            config.step_bps,
            config.level_size_pct,
            new_posture,
        );

        let mut placed = 0usize;
        for lv in &levels {
            let cid = match lv.side {
                Side::Short => format!("vamm-ask-{}", lv.price.raw()),
                Side::Long => format!("vamm-bid-{}", lv.price.raw()),
            };
            match state
                .engine
                .submit_order(
                    config.user_id.clone(),
                    lv.side,
                    OrderType::Limit,
                    lv.price,
                    lv.size,
                    1,
                    TimeInForce::Gtc,
                    false,
                    Some(cid),
                    Some("vAMM".into()),
                )
                .await
            {
                Ok(_) => placed += 1,
                Err(e) => {
                    warn!(side = ?lv.side, price = %lv.price, "vault vAMM order failed: {}", e)
                }
            }
        }

        debug!(
            placed,
            total = levels.len(),
            curve_mid,
            net_position,
            "vault vAMM: ladder placed"
        );

        posture = new_posture;
        last_position = net_position;
        last_mid = curve_mid;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn positions_to_net_xrp_sums_long_minus_short() {
        let bal = json!({
            "data": {
                "positions": [
                    {"side": "long", "size": "100.0"},
                    {"side": "short", "size": "40.0"},
                    {"side": "long", "size": "20.0"},
                ]
            }
        });
        let net = positions_to_net_xrp(&bal);
        assert!((net - 80.0).abs() < 1e-9, "expected 80, got {net}");
    }

    #[test]
    fn positions_to_net_xrp_empty_returns_zero() {
        let bal = json!({ "data": { "positions": [] } });
        assert_eq!(positions_to_net_xrp(&bal), 0.0);
    }

    #[test]
    fn positions_to_net_xrp_missing_field_returns_zero() {
        let bal = json!({ "data": {} });
        assert_eq!(positions_to_net_xrp(&bal), 0.0);
    }

    #[test]
    fn pick_position_to_reduce_chooses_largest_absolute() {
        let bal = json!({
            "data": {
                "positions": [
                    {"id": 1, "side": "long",  "size": "10.0"},
                    {"id": 2, "side": "short", "size": "50.0"},
                    {"id": 3, "side": "long",  "size": "30.0"},
                ]
            }
        });
        let (pid, side, _sz) = pick_position_to_reduce(&bal).expect("some");
        assert_eq!(pid, 2);
        assert_eq!(side, Side::Short);
    }

    #[test]
    fn pick_position_to_reduce_returns_none_when_empty() {
        let bal = json!({ "data": { "positions": [] } });
        assert!(pick_position_to_reduce(&bal).is_none());
    }
}
