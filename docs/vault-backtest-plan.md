# Vault performance simulation plan — XRP, trailing 12 months

**Status:** planning only — no code yet.
**Author:** drafted 2026-05-01.
**Scope:** propose a simulation harness that estimates trailing-year PnL / APY
for the three vaults described in `vault-design-spec.md` and the
`perp-dex-ui` cards:
1. Market Making (low risk)
2. Delta Neutral (medium risk)
3. Delta One (high risk)

**Headline window:** 2025-05-01 → 2026-05-01 (rolling 12 months ending today).
**Reference window:** calendar 2025 (2025-01-01 → 2025-12-31), reported as a
secondary table for the round-number comparison. Both halves of each window
are reported separately so the early-2025 low-vol / low-funding regime
doesn't get averaged into the post-rally late-2025/early-2026 regime.

**Vault unit-of-account:** **RLUSD** for all three vaults. Depositors bring
RLUSD, share NAV is RLUSD-denominated, all PnL columns and APY headlines are
in RLUSD/USD. Note this contradicts the live code (`vault_mm.rs` is
XRP-denominated today) and the canonical `docs/vault-design-spec.md:24-28`
("Accepted Liquidity: XRP"). The simulation reflects the **production
target** described in `BACKEND-API.md` and `perp-dex-faq-ru.md` — the spec
needs a follow-up edit to match.

The aim is *order-of-magnitude* expected returns under stated assumptions, not
a production backtest. We have the strategy code in-tree; we lack the venue
(no historical fills against our own book yet), so most of the work is in
modelling the counterparty side.

---

## 1. What we already have

Code we can reuse with minimal changes:

| Component | Path | Notes |
|---|---|---|
| MM + DN strategy logic | `orchestrator/src/vault_mm.rs` | Pyramid sizes `[1.9, 3.8, 7.6]`, half-spread `0.0025` widening 1.5×/level, `max_inventory` cap, `max_delta` flip-to-one-side rule |
| CLOB matcher | `orchestrator/src/orderbook.rs` | Price-time priority, partial fills, `Trade` records |
| Funding model | `orchestrator/src/main.rs:399` | `compute_funding_rate(mark, index)` = `(mark-index)/index`, clamped ±5 bps, applied every 8h (`FUNDING_INTERVAL`) |
| Index price source | `orchestrator/src/price_feed.rs` | Binance `XRPUSDT` spot — same series we'd backfill for 2025 |

Code we **don't** have today and will need to model in the harness:
- **Fee leg in matching.** `trading.rs` settles fills via `open_position` / `close_position` with no fee debit/credit. The committed fee schedule (`docs/perp-dex-faq-ru.md:323-328`) is `taker = 0.05%`, `maker = 0%`, funding cap `±0.05% per 8h`. The simulation has to apply these explicitly — the code doesn't yet.
- **Maker rebate.** `vault-design-spec.md` and `BUSINESS-PLAN.md` both list "fee rebate" as a vault revenue stream, but the FAQ schedule has `maker = 0%` flat — no rebate is currently committed at the venue. The simulation uses **1 bp as the headline default** — that matches the dYdX v4 base maker rebate and the typical Tier-1 VIP rebate at Binance / Bybit / OKX, and splits cleanly as `4 bp protocol / 1 bp vault` from our 5 bp taker fee. Sensitivity sweep `[0, 1, 2]` covers "no rebate program ever ships" and "generous Tier-1 VIP-equivalent" as the bounds.
- **Spot RLUSD/XRP market** and **lending integration** — explicitly flagged missing in `vault-design-spec.md` for the Delta One vault. Delta One is "planned post-launch" in `PITCH.md` with no committed POL allocation.
- **Counterparty order flow** — our book has no organic taker traffic yet; the backtest has to synthesise it (see §4).

---

## 2. Goal & non-goals

Goal: produce a per-vault **RLUSD-denominated** PnL curve over the window
with a decomposition into {spread, fee/rebate, funding, borrow cost,
inventory MtM}, plus the sensitivity to the top 2–3 parameters. APY is
reported as `(end_NAV − start_NAV) / start_NAV × 365/window_days` in
RLUSD — directly comparable to the UI's 12–18 / 15–25 / 20–35 % bands,
which are implicitly USD numbers.

Non-goals:
- Slippage realism beyond a simple impact model.
- Latency / failure-mode simulation (covered by `tests/scenarios_runner.py`
  in a different lens — that one is correctness, this one is economics).
- Capacity sizing under deep TVL — start with single-vault, fixed deposit.

---

## 3. Data inputs (XRP, 2025-05-01 → 2026-05-01; also calendar 2025)

| Series | Resolution | Source | Used for |
|---|---|---|---|
| XRP/USDT spot mid + 1m OHLCV | 1 minute | Binance public REST `klines` | Mark/index price; drives funding |
| XRP perp funding rate | 8h | Binance / Bybit / OKX historical funding endpoints | Sanity-check our clamp; **proxy** for what funding XRP perps actually paid over the window — drives DN and Delta One yield |
| XRP perp open interest, taker flow | 1m | Binance perp `aggTrades` | Synthetic counterparty flow for vault fills |
| USD borrow rate proxy | daily | **FRED SOFR + 100 bps** — public CSV, no API key. Originally planned to use Aave V3 USDC variable borrow APR via DefiLlama, but `chartLendBorrow` is now paywalled (402); SOFR + spread is the documented fallback and Aave USDC has historically tracked SOFR + a modest premium | Delta One leverage cost. Fixed 5% / 8% / 12% APR are the round-number sensitivity points; SOFR + 100 bps daily is the headline source |
| XRPL fee schedule | static | n/a | Withdraw / deposit cost on the user side (negligible but include) |

Storage: a small Parquet bundle under `research/backtest-data/` (gitignored;
fetch script committed) keyed by `{symbol}_{from}_{to}` so multiple windows
share cache. ~15 MB per year at 1m resolution.

Open data question: do we trust a single CEX's 2025 funding history, or blend
the median across Binance/Bybit/OKX? Recommendation: median of the three —
Binance can be an outlier during squeezes, and our index is already Binance
spot, so picking Binance funding too would double-count.

---

## 4. Simulation harness — three fidelity tiers

Pick the cheapest tier that answers the question; only escalate if results
are decision-relevant.

### Tier A — closed-form per-bucket (fastest, ~1 day to build)

All revenues are in **RLUSD** (mark price × XRP-quoted size). Treat each
8-hour funding bucket as a unit:
- **MM revenue** = `fill_volume × mark_usd × (half_spread + rebate_bps/10_000)`
- `fill_volume` ≈ `min(taker_buy_vol, taker_sell_vol) × fill_share`, where
  `fill_share` is the fraction of taker flow that crosses our quoted band
  (function of half-spread vs. realised vol in the bucket).
- **DN revenue** = MM revenue + funding accrual on the running perp inventory:
  `Σ (funding_rate × inventory_size × mark_usd)` summed across the bucket.
  Under the RLUSD-NAV interpretation, the perp position itself *is* the
  delta exposure relative to the USD-flat collateral; DN captures funding
  during inventory periods rather than running a separate hedge leg (see §7
  caveat — the live code doesn't actually hedge either).
- **Delta One revenue** = `funding_rate × notional × leverage − borrow_apr × notional × (leverage−1) × bucket_hours / (365 × 24)`,
  plus a small spot-vs-mark basis trickle.

Inventory MtM is RLUSD-quoted: residual position carried at end-of-bucket
× (mark_end − mark_avg_entry).

Output: 365 daily rows × 3 vaults. Good enough for "is the targeted APY band
plausible?" and to map parameter sensitivity.

### Tier B — event-driven, our CLOB in the loop (1–2 weeks)

Lift `vault_mm.rs` into a Rust binary that:
1. Spins up an in-process `OrderBook` from `orderbook.rs`.
2. Replays Binance 1m bars as the *index/mark* feed.
3. Synthesises taker orders against our book at each minute, sized so that
   aggregate taker volume matches the historical XRP-perp flow scaled to the
   vault's TVL share. Take direction = sign of next-bar return + Poisson
   noise (cheap proxy for adverse selection).
4. Lets `vault_mm.rs` quote, cancel, repost on its real `interval_secs`.
5. Applies funding on the 8h boundary using the median CEX rate, debits
   maker fee / credits rebate as configurable bps.

This gives us realistic inventory accumulation, level-pyramid behaviour,
the `max_inventory` kill-switch firing, and the DN flip-to-one-side logic.
PnL is the sum of `Trade` records plus running mark-to-market on the
position carried at end-of-period.

### Tier C — agent-based with adverse selection model (later, only if Tier B's
fill assumptions are clearly wrong)

Replace the noisy-sign taker model with a flow model fitted to actual
Binance taker imbalance per minute. Out of scope for the first pass.

---

## 5. Parameters to sweep

For MM and DN (`vault_mm.rs` defaults in parens):
- `half_spread` (0.0025) → sweep `[10, 15, 25, 40, 60]` bps
- `levels` (3) → `[1, 3, 5]`
- `interval_secs` (5) → `[5, 30, 120]` — at higher rebalance cost the cancel/replace toll matters
- `max_inventory` (50 XRP) → `[10, 50, 200, 500]`
- `max_delta` (500 XRP) → `[100, 500, 2000]` — only DN
- `taker_fee_bps` is **fixed at 5** (committed in the FAQ); `maker_rebate_bps` headline = **1**, sweep `[0, 1, 2]` for sensitivity (1 bp = market-standard default, matching dYdX v4 base / typical Tier-1 VIP)
- **Vault notional**: **$100k for all three vaults** (apples-to-apples). $100k matches the `BUSINESS-PLAN.md` POL allocation for the MM vault; DN and Delta One don't have committed POL but run at the same notional so the headline comparison is clean

For Delta One:
- `target_leverage` → `[1.5, 2.0, 3.0]`
- `borrow_apr`: headline = **FRED SOFR + 100 bps daily series** for the window (Aave V3 USDC paywalled at DefiLlama); sensitivity = fixed `[5%, 8%, 12%]` APR
- `funding_capture_efficiency` → `[0.7, 0.85, 1.0]` (slippage on entering/rolling the short)

Each sweep is one row in the output table; we report median APY and
max-drawdown over 2025.

---

## 6. Outputs

A single `research/vault-backtest/` deliverable (one report per window, keyed
by `from-to`; trailing-12mo headline + calendar-2025 reference):

1. `report.md` — exec summary, headline APY per vault, drawdown chart,
   parameter sensitivity table, list of assumptions explicitly numbered so
   we can argue with each one.
2. `equity_curves.csv` — daily NAV per vault per parameter combo (long-form).
3. `decomposition.csv` — daily PnL split into spread / rebate / funding /
   borrow / inventory MtM.
4. `assumptions.md` — every magic number with its source and confidence
   level (high / medium / low). The Delta One fee rebate, the
   counterparty-flow Poisson rate, and the borrow-rate proxy are the
   load-bearing ones; if any of them is off by 2× the conclusion changes.

Targeted APY bands from the UI cards (12–18 / 15–25 / 20–35 %) become
*hypotheses to falsify*, not targets to fit.

---

## 7. Risks & honest caveats

- **Fee rebate is aspirational at our venue today.** The committed schedule
  is `taker = 5 bps`, `maker = 0%` (not rebated). The simulation's
  headline `rebate = 1 bp` is the market-standard value other venues pay,
  not a number our protocol has committed. The `0` column shows the "no
  rebate program ever ships" downside; `2` shows the Tier-1 VIP-equivalent
  upside. Never bury the assumption.
- **No real flow.** Synthetic counterparty flow is the biggest fudge. A
  Tier B run with Poisson taker direction is plausible, not predictive — DN
  inventory PnL especially is sensitive to adverse selection that this model
  understates.
- **Funding clamp.** Our `compute_funding_rate` clamps at ±5 bps per 8h
  (≈54 % APR equivalent). Real 2025 XRP perp funding occasionally exceeds
  this in either direction. The DN/Delta One yield should be reported under
  *both* "what the venue actually paid" and "what our clamped rate would
  have paid" — the gap is informative.
- **Delta One has missing pieces.** Spot RLUSD/XRP and lending integration
  don't exist yet (per the spec). The Delta One simulation is therefore a
  *what-if-we-had-it* analysis, not a backtest of shipped infra. The pitch
  doesn't allocate POL to Delta One — running it at $100k matches the MM
  POL for apples-to-apples comparison and should be labelled as such. The
  Aave-V3-USDC borrow proxy is also a *reference rate*, not a quote we
  could actually borrow at against XRP collateral on XRPL today.

- **Spec / code / production-target mismatch on vault denomination.** The
  canonical `vault-design-spec.md` says "Accepted Liquidity: XRP", live
  `vault_mm.rs` deposits XRP, and `BACKEND-API.md` / `perp-dex-faq-ru.md`
  say the production market and collateral are RLUSD. The simulation
  follows the production target (RLUSD-deposit, RLUSD-NAV) — this is a
  forward-looking projection, not a backtest of what's deployed today. The
  spec text needs a follow-up edit to reconcile.

- **DN's "delta-neutral" requires interpretation under RLUSD-NAV.** With
  RLUSD collateral the depositor is already USD-flat — the only delta
  exposure is the *running perp inventory* from MM fills. The simulation
  treats DN's "neutrality" as `|net_perp_position| ≤ max_delta` capped by
  one-sided quoting (matches the live code), with funding accruing on the
  inventory between fills (matches the spec's revenue story). It does
  *not* simulate a separate active perp short hedge, because the live
  code doesn't open one. If the team wants the spec's "active hedge"
  reading, that's a code change, not a simulation change.
- **Survivorship.** XRP/USDT was liquid on Binance throughout 2025. We're
  not testing a path where the index feed fails — that's `failure-modes-test-report.md`'s
  job.

---

## 8. Concrete tasks (suggested order)

1. **Data fetch script** (Python or Rust) — pulls 2025 Binance spot 1m + perp
   funding + perp aggTrades; caches under `research/backtest-data/2025/`.
   Runs once, deterministic.
2. **Tier A notebook** — closed-form per-bucket, all three vaults. Goal: a
   first-cut headline number to compare against the UI's APY bands. Cheap;
   stop here if the answers are obviously sensible and the user doesn't
   want more.
3. **Tier B harness** — Rust binary in `tools/vault-backtest/` (new crate
   under the workspace) that imports `orderbook` and `vault_mm` and feeds
   them historical data. Output equity curves + decomposition.
4. **Parameter sweep runner** — wraps the Tier B binary in a small driver
   that emits the CSVs.
5. **Report write-up** — `research/vault-backtest-2025/report.md` with the
   honest caveats from §7 made loud.

Estimated effort: Tier A ≈ 1–2 days. Tier B ≈ 1–2 weeks if we lift the
existing Rust modules cleanly; longer if we end up needing to detangle
`AppState` to run them headless.

---

## 9. Resolved decisions

All assumptions are pinned. The harness can be built without further input.

| Decision | Value | Source / rationale |
|---|---|---|
| Vault unit-of-account | **RLUSD** | depositor brings RLUSD, NAV is RLUSD-denominated; APY headline is in USD. Contradicts current spec text + live code (XRP-in) — needs spec follow-up |
| Window (headline) | 2025-05-01 → 2026-05-01 | trailing 12 months ending today |
| Window (reference) | 2025-01-01 → 2025-12-31 | round-number calendar 2025 |
| Each window | reported in halves | exposes early-2025 low-vol vs. post-rally regime |
| Taker fee | 5 bps (fixed) | `docs/perp-dex-faq-ru.md:323` — committed |
| Maker rebate, headline | **1 bp** (paid to vault) | dYdX v4 base / typical Tier-1 VIP — market standard for "default rebate program"; clean 4 bp protocol / 1 bp vault split of our 5 bp taker fee |
| Maker rebate, sweep | `[0, 1, 2]` bps | downside / headline / Tier-1 VIP-equivalent upside |
| Funding cap | ±5 bps / 8h | `docs/perp-dex-faq-ru.md:328` — committed (also reported uncapped to show the gap) |
| Vault notional | **100,000 RLUSD for all three** | apples-to-apples; matches MM POL in `BUSINESS-PLAN.md:12` (RLUSD ≈ USD 1:1) |
| Borrow rate (Delta One), headline | **FRED SOFR + 100 bps daily series** | Public TradFi data (no API key); fallback after DefiLlama paywalled `chartLendBorrow`. Aave USDC has historically tracked SOFR + a modest DeFi premium so this runs slightly low — sensitivity bounds cover the realistic range |
| Borrow rate, sensitivity | fixed `[5%, 8%, 12%]` APR | round-number bounds |

The `assumptions.md` deliverable in §6 should reproduce this table verbatim
and tag confidence (committed / market-standard proxy / hypothetical).
