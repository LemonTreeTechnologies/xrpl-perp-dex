//! Virtual AMM curve, ladder builder, and posture/risk gating.
//!
//! Per Tom-spec (`docs/post-hackathon-specs.md` Issue #3 + PR #9 response):
//! - Constant-product curve; virtual reserves derived from collateral.
//! - The curve's implied mid is the **sole** source of truth for vault
//!   quoting. We never read an external mark to choose where to post —
//!   Tom: "this is REALLY IMPORTANT as it is the source of the arb flow."
//! - Ladder of N levels per side, step_bps apart, sized as a fraction of
//!   free collateral per level.
//! - Posture gating: hot when |net_delta_frac| ≥ delta_cap or util ≥
//!   util_cap; exit hot with a hysteresis gap.
//!
//! All math is in f64 XRP units; FP8 conversion is at the boundary.

use crate::types::{Side, FP8};

// ─── Curve ──────────────────────────────────────────────────────────────

/// Constant-product virtual AMM curve.
///
/// Reserve invariant `k = x_0² · mark_init` is anchored at vault genesis,
/// when the vault has zero position and the implied mid equals the
/// market mark. As the vault's position drifts from the target position,
/// the curve's `x_eff = x_0 + (position - target_position)` evolves and
/// the implied mid moves to push the position back toward target.
///
/// `target_delta_frac` is the fraction of collateral the vault aims to
/// hold as a **short** perp position (Tom-Q3.4: positive value = short
/// bias; default 0.5 per the spec example). So `target_position` in XRP
/// units is `-target_delta_frac · collateral_xrp`.
#[derive(Debug, Clone, Copy)]
pub struct Curve {
    pub x_0: f64,
    pub mark_init: f64,
    pub k: f64,
    pub target_position: f64,
}

impl Curve {
    /// Build a curve from collateral + depth + initial mark + target delta.
    ///
    /// `depth_mult`: how deep the virtual pool is relative to collateral
    /// (x_0 = depth_mult · collateral_xrp). Larger = less price impact per
    /// fill = wider curve. Tom-spec does not pin a default; we use 10.0
    /// as a moderate starting point (a fill of 10% of collateral moves
    /// the mid by ~2%).
    pub fn new(
        collateral_xrp: f64,
        depth_mult: f64,
        mark_init: f64,
        target_delta_frac: f64,
    ) -> Self {
        let x_0 = depth_mult * collateral_xrp;
        let k = x_0 * x_0 * mark_init;
        let target_position = -target_delta_frac * collateral_xrp;
        Self {
            x_0,
            mark_init,
            k,
            target_position,
        }
    }

    /// Implied mid price at the given vault position (XRP, signed: positive = long).
    /// p(offset) = mark_init · (x_0 / (x_0 + offset))², where offset = position − target_position.
    pub fn implied_mid(&self, position: f64) -> f64 {
        let offset = position - self.target_position;
        let x_eff = self.x_0 + offset;
        if x_eff <= 0.0 {
            // Pathological: vault is so short that the curve diverges.
            // Return a very large price; caller will throttle quoting.
            return self.mark_init * 1e6;
        }
        let ratio = self.x_0 / x_eff;
        self.mark_init * ratio * ratio
    }
}

// ─── Ladder ─────────────────────────────────────────────────────────────

/// One level of the quote ladder.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LadderLevel {
    pub side: Side,
    pub price: FP8,
    pub size: FP8,
}

/// Build a ladder of `steps` levels per side, `step_bps` apart, each sized
/// at `level_size_pct` of `free_collateral_xrp`. Mid is `curve.implied_mid(position)`.
///
/// Posture filters which sides are emitted:
/// - `Healthy` → both
/// - `DeltaHotLong` → asks only (sell to reduce length)
/// - `DeltaHotShort` → bids only (buy to reduce shortness)
/// - `UtilHot` → empty (caller cancels everything and reduces positions)
pub fn build_ladder(
    curve: &Curve,
    position: f64,
    free_collateral_xrp: f64,
    steps: usize,
    step_bps: u32,
    level_size_pct: f64,
    posture: Posture,
) -> Vec<LadderLevel> {
    if matches!(posture, Posture::UtilHot) {
        return Vec::new();
    }
    let mid = curve.implied_mid(position);
    if mid <= 0.0 || !mid.is_finite() {
        return Vec::new();
    }
    let level_size_xrp = free_collateral_xrp * level_size_pct;
    if level_size_xrp <= 0.0 {
        return Vec::new();
    }
    let want_asks = !matches!(posture, Posture::DeltaHotShort);
    let want_bids = !matches!(posture, Posture::DeltaHotLong);

    let mut out = Vec::with_capacity(2 * steps);
    let size_fp = FP8::from_f64(level_size_xrp);
    if size_fp.raw() <= 0 {
        return Vec::new();
    }
    for i in 1..=steps {
        let bps = (i as f64) * (step_bps as f64) / 10_000.0;
        if want_asks {
            let p = FP8::from_f64(mid * (1.0 + bps));
            if p.raw() > 0 {
                out.push(LadderLevel {
                    side: Side::Short,
                    price: p,
                    size: size_fp,
                });
            }
        }
        if want_bids {
            let p = FP8::from_f64(mid * (1.0 - bps));
            if p.raw() > 0 {
                out.push(LadderLevel {
                    side: Side::Long,
                    price: p,
                    size: size_fp,
                });
            }
        }
    }
    out
}

// ─── Posture ────────────────────────────────────────────────────────────

/// Vault risk posture. Enters hot at the cap, exits via hysteresis.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Posture {
    /// Both sides quoting.
    Healthy,
    /// Too long (delta ≥ +cap); asks-only until delta returns below cap·(1−hyst).
    DeltaHotLong,
    /// Too short (delta ≤ −cap); bids-only until delta returns above −cap·(1−hyst).
    DeltaHotShort,
    /// Collateral utilization over cap; cancel everything, reduce positions.
    UtilHot,
}

/// Evaluate posture given current state + previous posture (for hysteresis).
///
/// `net_delta_frac` is `vault_net_position_xrp / collateral_xrp` (signed; positive = long).
/// `util` is fraction-of-collateral in use (0.0..1.0).
/// `hysteresis` is the gap below the cap at which a hot state exits, e.g. 0.25.
pub fn evaluate_posture(
    net_delta_frac: f64,
    util: f64,
    prev: Posture,
    hysteresis: f64,
    delta_cap: f64,
    util_cap: f64,
) -> Posture {
    if util >= util_cap {
        return Posture::UtilHot;
    }
    let resume_thr = delta_cap * (1.0 - hysteresis);

    match prev {
        Posture::DeltaHotLong => {
            if net_delta_frac >= resume_thr {
                Posture::DeltaHotLong
            } else if net_delta_frac <= -delta_cap {
                Posture::DeltaHotShort
            } else {
                Posture::Healthy
            }
        }
        Posture::DeltaHotShort => {
            if net_delta_frac <= -resume_thr {
                Posture::DeltaHotShort
            } else if net_delta_frac >= delta_cap {
                Posture::DeltaHotLong
            } else {
                Posture::Healthy
            }
        }
        _ => {
            if net_delta_frac >= delta_cap {
                Posture::DeltaHotLong
            } else if net_delta_frac <= -delta_cap {
                Posture::DeltaHotShort
            } else {
                Posture::Healthy
            }
        }
    }
}

// ─── Tests ──────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn approx_eq(a: f64, b: f64, tol: f64) -> bool {
        (a - b).abs() <= tol
    }

    // ── Curve ──

    #[test]
    fn curve_at_target_quotes_mark() {
        // Vault collateral 1000 XRP, target_delta_frac 0.5, mark 2.0 USD/XRP.
        // target_position = -500 XRP. At position = target_position, mid == mark.
        let c = Curve::new(1000.0, 10.0, 2.0, 0.5);
        let mid = c.implied_mid(-500.0);
        assert!(approx_eq(mid, 2.0, 1e-9), "expected 2.0, got {mid}");
    }

    #[test]
    fn curve_mid_drops_when_position_above_target() {
        // Too long → curve drops to discourage buying.
        let c = Curve::new(1000.0, 10.0, 2.0, 0.5);
        let at_target = c.implied_mid(-500.0);
        let above = c.implied_mid(0.0); // 500 XRP above target
        assert!(above < at_target, "above-target mid {above} not < {at_target}");
    }

    #[test]
    fn curve_mid_rises_when_position_below_target() {
        // Too short → curve rises to encourage longing.
        let c = Curve::new(1000.0, 10.0, 2.0, 0.5);
        let at_target = c.implied_mid(-500.0);
        let below = c.implied_mid(-1000.0); // 500 XRP below target
        assert!(below > at_target, "below-target mid {below} not > {at_target}");
    }

    #[test]
    fn curve_invariant_constant_product() {
        // k = x_0² · mark_init = (10·1000)² · 2 = 200_000_000.
        let c = Curve::new(1000.0, 10.0, 2.0, 0.0);
        assert!(approx_eq(c.k, 200_000_000.0, 1e-3));
        assert!(approx_eq(c.x_0, 10_000.0, 1e-9));
    }

    #[test]
    fn curve_target_zero_quotes_mark_at_zero_position() {
        let c = Curve::new(1000.0, 10.0, 2.0, 0.0);
        assert!(approx_eq(c.implied_mid(0.0), 2.0, 1e-9));
    }

    // ── Ladder ──

    #[test]
    fn ladder_healthy_produces_both_sides() {
        let c = Curve::new(1000.0, 10.0, 2.0, 0.5);
        let levels = build_ladder(&c, -500.0, 800.0, 5, 10, 0.1, Posture::Healthy);
        assert_eq!(levels.len(), 10, "5 levels × 2 sides = 10");
        let n_ask = levels.iter().filter(|l| l.side == Side::Short).count();
        let n_bid = levels.iter().filter(|l| l.side == Side::Long).count();
        assert_eq!(n_ask, 5);
        assert_eq!(n_bid, 5);
    }

    #[test]
    fn ladder_bid_lt_mid_lt_ask() {
        let c = Curve::new(1000.0, 10.0, 2.0, 0.5);
        let levels = build_ladder(&c, -500.0, 800.0, 3, 10, 0.1, Posture::Healthy);
        let mid_f = c.implied_mid(-500.0);
        for lv in &levels {
            let p = lv.price.to_f64();
            match lv.side {
                Side::Short => assert!(p > mid_f, "ask {p} not > mid {mid_f}"),
                Side::Long => assert!(p < mid_f, "bid {p} not < mid {mid_f}"),
            }
        }
    }

    #[test]
    fn ladder_steps_widen_outward() {
        let c = Curve::new(1000.0, 10.0, 2.0, 0.5);
        let levels = build_ladder(&c, -500.0, 800.0, 3, 10, 0.1, Posture::Healthy);
        let asks: Vec<f64> = levels
            .iter()
            .filter(|l| l.side == Side::Short)
            .map(|l| l.price.to_f64())
            .collect();
        let bids: Vec<f64> = levels
            .iter()
            .filter(|l| l.side == Side::Long)
            .map(|l| l.price.to_f64())
            .collect();
        for w in asks.windows(2) {
            assert!(w[1] > w[0], "asks not strictly increasing: {asks:?}");
        }
        for w in bids.windows(2) {
            assert!(w[1] < w[0], "bids not strictly decreasing: {bids:?}");
        }
    }

    #[test]
    fn ladder_level_size_is_free_collateral_fraction() {
        let c = Curve::new(1000.0, 10.0, 2.0, 0.5);
        let levels = build_ladder(&c, -500.0, 800.0, 3, 10, 0.1, Posture::Healthy);
        // 10% of 800 = 80 XRP per level.
        for lv in &levels {
            assert!(approx_eq(lv.size.to_f64(), 80.0, 0.01), "size {} != 80", lv.size.to_f64());
        }
    }

    #[test]
    fn ladder_delta_hot_long_emits_asks_only() {
        let c = Curve::new(1000.0, 10.0, 2.0, 0.5);
        let levels = build_ladder(&c, -500.0, 800.0, 3, 10, 0.1, Posture::DeltaHotLong);
        assert_eq!(levels.len(), 3);
        assert!(levels.iter().all(|l| l.side == Side::Short));
    }

    #[test]
    fn ladder_delta_hot_short_emits_bids_only() {
        let c = Curve::new(1000.0, 10.0, 2.0, 0.5);
        let levels = build_ladder(&c, -500.0, 800.0, 3, 10, 0.1, Posture::DeltaHotShort);
        assert_eq!(levels.len(), 3);
        assert!(levels.iter().all(|l| l.side == Side::Long));
    }

    #[test]
    fn ladder_util_hot_is_empty() {
        let c = Curve::new(1000.0, 10.0, 2.0, 0.5);
        let levels = build_ladder(&c, -500.0, 800.0, 3, 10, 0.1, Posture::UtilHot);
        assert!(levels.is_empty());
    }

    #[test]
    fn ladder_empty_when_no_free_collateral() {
        let c = Curve::new(1000.0, 10.0, 2.0, 0.5);
        let levels = build_ladder(&c, -500.0, 0.0, 3, 10, 0.1, Posture::Healthy);
        assert!(levels.is_empty());
    }

    // ── Posture ──

    #[test]
    fn posture_healthy_when_within_caps() {
        let p = evaluate_posture(0.0, 0.5, Posture::Healthy, 0.25, 2.0, 0.8);
        assert_eq!(p, Posture::Healthy);
    }

    #[test]
    fn posture_enters_hot_long_at_cap() {
        let p = evaluate_posture(2.0, 0.5, Posture::Healthy, 0.25, 2.0, 0.8);
        assert_eq!(p, Posture::DeltaHotLong);
    }

    #[test]
    fn posture_stays_hot_long_above_resume_threshold() {
        // hyst 0.25 → exit threshold = 2·(1−0.25) = 1.5. At 1.6, still hot.
        let p = evaluate_posture(1.6, 0.5, Posture::DeltaHotLong, 0.25, 2.0, 0.8);
        assert_eq!(p, Posture::DeltaHotLong);
    }

    #[test]
    fn posture_exits_hot_long_below_resume_threshold() {
        // Exit at 1.4 (< 1.5).
        let p = evaluate_posture(1.4, 0.5, Posture::DeltaHotLong, 0.25, 2.0, 0.8);
        assert_eq!(p, Posture::Healthy);
    }

    #[test]
    fn posture_enters_hot_short_at_negative_cap() {
        let p = evaluate_posture(-2.0, 0.5, Posture::Healthy, 0.25, 2.0, 0.8);
        assert_eq!(p, Posture::DeltaHotShort);
    }

    #[test]
    fn posture_stays_hot_short_above_negative_resume() {
        let p = evaluate_posture(-1.6, 0.5, Posture::DeltaHotShort, 0.25, 2.0, 0.8);
        assert_eq!(p, Posture::DeltaHotShort);
    }

    #[test]
    fn posture_exits_hot_short_within_negative_resume() {
        let p = evaluate_posture(-1.4, 0.5, Posture::DeltaHotShort, 0.25, 2.0, 0.8);
        assert_eq!(p, Posture::Healthy);
    }

    #[test]
    fn posture_util_hot_dominates() {
        let p = evaluate_posture(0.0, 0.9, Posture::Healthy, 0.25, 2.0, 0.8);
        assert_eq!(p, Posture::UtilHot);
    }

    #[test]
    fn posture_util_hot_overrides_delta_hot() {
        // Even when delta would say healthy, util cap pins to UtilHot.
        let p = evaluate_posture(0.5, 0.85, Posture::DeltaHotLong, 0.25, 2.0, 0.8);
        assert_eq!(p, Posture::UtilHot);
    }

    #[test]
    fn posture_no_oscillation_in_hysteresis_band() {
        // delta = 1.7 (in the band 1.5..2.0): if previously Healthy, stays Healthy
        // (didn't reach the cap yet). If previously DeltaHotLong, stays hot (still
        // above resume threshold). Demonstrates hysteresis works in both directions.
        assert_eq!(
            evaluate_posture(1.7, 0.5, Posture::Healthy, 0.25, 2.0, 0.8),
            Posture::Healthy
        );
        assert_eq!(
            evaluate_posture(1.7, 0.5, Posture::DeltaHotLong, 0.25, 2.0, 0.8),
            Posture::DeltaHotLong
        );
    }
}
