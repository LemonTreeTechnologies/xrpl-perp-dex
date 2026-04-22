# Response to post-hackathon-specs.md — open questions

**Context:** response to 8Baller's `docs/post-hackathon-specs.md` (4 issues) and carry-forward of unresolved items from `docs/vault-design-followup.md` (A1–A7).
**Author:** AL + team
**Date:** 2026-04-19
**Status:** discussion — blocks implementation of issues #2 (cross-margin) and #3 (vAMM) until the items below are resolved.

This doc lists the questions we cannot answer without input from 8Baller. Items marked **BLOCKING** must be resolved before we write code; items marked **non-blocking** can be decided while we implement issue #1.

---

## Issue #1 — Contract specs in `get_markets`

**Summary of request:** return `minimum_order_size`, `amount_step`, `price_step` per market.

### Q1.1 (non-blocking) — Where do contract specs live?

Three options:

1. **Hardcoded constants** in the enclave (simplest, requires MRENCLAVE-bumping migration to change).
2. **Config file** read by the orchestrator at startup (easy to change, not authoritative — the enclave would need to enforce the same constants separately or it's an attack vector).
3. **Persisted state inside the enclave**, set via an admin ecall (requires a new ecall + admin auth).

For v1 we propose **(1)** — hardcode `minimum_order_size = 0.01 XRP`, `amount_step = 0.01 XRP`, `price_step = 0.0001 RLUSD`. Please confirm or override these values.

### Q1.2 (non-blocking) — Rounding semantics

When a submitted order doesn't match `amount_step`, do we:

- (a) **reject** with 400 (strictest, no silent corrections), or
- (b) **round down** to nearest step and accept?

We propose (a) for correctness. Please confirm.

---

## Issue #2 — Cross-margin portfolio system

**Summary of request:** replace isolated per-position margin with portfolio-level margin sharing across all positions. Funding applied in aggregate per-second.

### Q2.1 (BLOCKING) — Liquidation model under cross-margin

Under isolated margin, each position liquidates independently when its own maintenance margin is breached. Under cross-margin, the portfolio as a whole is liquidated when aggregate maintenance margin breaches. **What's the liquidation order?**

Options:

1. **Largest-loss-first** — close the position with the biggest unrealized loss to free the most margin.
2. **Largest-position-first** — close the largest position by notional.
3. **All-or-nothing** — liquidate the entire portfolio at once.
4. **Per-market tiered** — close only enough positions to bring the account back above maintenance margin.

We recommend (4) — partial liquidations. But this is a design decision with UX and MEV implications, not a technical one. Please pick.

### Q2.2 (BLOCKING) — Funding accrual granularity

The spec says "funding is calculated based on position size and the funding rate **each second**". Does that mean:

- (a) accrue funding every second into a running `funding_accrued` balance, apply to margin on any state-modifying ecall, or
- (b) apply funding to margin every second in a timer-driven ecall?

(a) is cheap and idempotent but requires all ecalls to first call "settle funding" before doing work. (b) is simpler to reason about but runs a global ecall every second across all users (potentially N×ecall calls per second where N = user count).

We recommend (a). Please confirm.

### Q2.3 (BLOCKING) — Margin check location

Under isolated margin, the enclave's `ecall_perp_open_position` checks that the **specific position's initial margin** is available. Under cross-margin, it must check **portfolio initial margin**:

```
required_initial_margin = sum(|position_notional_i|) × initial_margin_rate
+ |new_position_notional| × initial_margin_rate
maintenance_margin = sum(|position_notional_i|) × maintenance_margin_rate
```

This is a rewrite of the margin check, not an edit. We need confirmation that:

- Initial margin rate is uniform across markets (we only have XRP/RLUSD today, so irrelevant, but as soon as we add more markets it matters).
- Maintenance margin rate is uniform OR tiered by notional size.

What rate are we using? `vault-design-spec.md` implies MM vault uses 0.5% maintenance. Is that the same for retail users?

### Q2.4 (non-blocking) — Margin migration for existing positions

When we ship cross-margin, existing isolated positions held by users need to either:

- (a) be **grandfathered** as isolated until closed (dual-mode code forever), or
- (b) be **migrated** to cross-margin at deploy time (one-shot, simpler code, risk of breaking users who planned on isolation).

On dev/testnet this is moot. On mainnet, with real user positions, we need a plan. We recommend (b) with an on-chain announcement ≥1 week ahead.

---

## Issue #3 — Virtual AMM posting orders to CLOB

**Summary of request:** constant-product curve determines the prices at which the vault posts orders to the CLOB. The vault has no actual AMM reserves; it has an XRP collateral balance and a target delta.

### Q3.1 (BLOCKING) — Curve parametrization

"Constant product curve where the product of base and quote reserves is constant." Constant product is `x * y = k`, where `x`, `y` are the (virtual) reserves. But the vault doesn't hold base and quote — it holds XRP collateral that backs a perp position.

Two readings:

1. **Virtual reserves from collateral:** at init, `k = (0.5 × collateral_in_xrp)² × mark_price`. As the market moves, `x` and `y` are recomputed against the current mark, and orders are posted at the curve-implied price.
2. **Virtual reserves fixed at init:** `k = k_0` forever; `x` and `y` drift as fills happen. This is how Uniswap v2 works, but for a vAMM-to-CLOB hybrid it's unclear how to reconcile fills with reserve drift when the vault's physical collateral can't back arbitrary virtual imbalance.

We can implement either, but we need to know **which**. Please pick.

### Q3.2 (BLOCKING) — Order placement algorithm

"Post sell buy and sell orders on the CLOB based on the curve." Concretely:

- **How many price levels** per side? One tight quote, or a ladder of N orders?
- **What's the size of each order?** Fixed percentage of collateral? Derived from curve slippage at a fixed notional?
- **What's the refresh policy?** Cancel-replace every N seconds? Cancel-replace on mark price move of ≥X%? Event-driven on our own fills?

We propose a ladder of 5 levels per side, each 10 bps apart, sized at 10% of free collateral, refreshed on either a 2-second timer or a ≥5 bps mark price move. Please override.

### Q3.3 (BLOCKING) — Entry vs exit order labels

The spec introduces "entry" and "exit" order types. Clarify:

- **Are these just internal labels in the vault's bookkeeping**, or do they need to be a first-class order-type flag on the CLOB? (We recommend internal labels — the CLOB doesn't need to know.)
- **Exit order pricing** — priced from the same curve as entries, or a fixed offset from the filled entry price?
- **Exit order cancellation** — what happens to an outstanding exit order if the vault hits its delta cap and wants to stop reducing? Cancel it, or leave it and stop posting new ones?

### Q3.4 (BLOCKING) — Target delta semantics

"Delta 0.5 XRP exposure" — does "0.5" mean:

- (a) **50% of collateral notional**, i.e., if vault has 100k XRP collateral, target short is 50k XRP of perp, or
- (b) **0.5 XRP absolute**, i.e., net exposure should be within 0.5 XRP regardless of collateral size?

The rest of the doc mentions "delta -2 / +2" as risk bounds, which only makes sense as **XRP units**. But "delta 0.5 XRP exposure" as a target makes no sense in absolute units for a 100k vault. We assume (a) = fraction. Please confirm.

### Q3.5 (BLOCKING) — Relationship between vAMM and MM vault from `vault-design-spec.md`

`vault-design-spec.md` describes a **Market Making Vault** with spread/size/rebalance parameters. `post-hackathon-specs.md` describes a **vAMM vault** posting curve-based orders. Are these:

- (a) **the same vault** — vAMM replaces the MM vault's quoting logic?
- (b) **two different vault types** living alongside each other?
- (c) **vAMM replaces the MM vault entirely**; MM vault is deprecated?

We assume (a). Please confirm, because this drives whether `vault-design-spec.md`'s "Min Spread / Max Spread / Order Size %" parameters survive or are dropped in favor of curve parameters.

---

## Issue #4 — Risk limits (delta ±2, collateral utilization 80%)

**Summary of request:** enforce delta in [-2, +2], collateral utilization ≤ 80%. Stop posting / close positions when hit.

### Q4.1 (BLOCKING) — Which position to close when utilization > 80%

"Stop posting new orders and potentially start closing existing positions." Options:

1. **Close nothing, stop posting** — utilization drops only as funding accrues or the market moves.
2. **Reduce the largest position** by the amount needed to return to 80%.
3. **Cancel all resting orders** first; only close positions if that's not enough.

We recommend (3) — cancel first, close only if still over. Please confirm.

### Q4.2 (non-blocking) — Delta band hysteresis

If delta oscillates around ±2, we'll thrash between "open for new orders" and "only exits allowed". Do we want hysteresis (e.g. stop posting at |delta|=2, resume at |delta|=1.5)? We propose yes, with a default 25% gap. Please confirm.

### Q4.3 (non-blocking) — Per-operator vs global limits

The delta and utilization limits — are they **per-vault** (each vault tracks its own) or **global across all vaults** (aggregated)? We assume per-vault. Please confirm.

---

## Carry-forward from `vault-design-followup.md` (unanswered from 2026-04-09)

These were raised in the earlier review and not yet resolved. Re-listing with status:

| # | Topic | Status | Blocker? |
|---|-------|--------|----------|
| A1 | "Signs with session key" — is it 2-of-3 multisig or single-signer? | Unanswered | **BLOCKING** for vault withdraw implementation |
| A2 | Vault model — pure-vault / LP layer / vault-as-user? | Unanswered | **BLOCKING** for API routing |
| A3.1 | NAV formula — cash only / + unrealized PnL / + funding? | Unanswered | **BLOCKING** for PPS calculation |
| A3.2 | Share seeding / first deposit formula | Unanswered | **BLOCKING** for mint logic |
| A3.3 | Withdrawal queue / epoch vs immediate? NAV front-running mitigation | Unanswered | **BLOCKING** for security |
| A3.4 | Liquidation loss socialization vs insurance fund? | Unanswered | Non-blocking for v1 if we pick one |
| A4.1 | Vault trading strategy — naive MM / delta-neutral / passive LP? | Partially answered by issue #3 above, still needs confirmation that vAMM = the strategy | Blocking |
| A4.2 | Vault CLOB participation — same CLOB or separate? | Confirmed "same CLOB" by issue #3 | Resolved |
| A4.3 | When does orchestrator call `create-order`? | Partially answered by issue #3 Q3.2 refresh policy | Blocking |
| A5 | Fees (management / performance / HWM), capacity, pagination | Unanswered | Non-blocking for v1 if deferred |
| A6 | Admin auth | Unanswered | Non-blocking for v1 |
| A7 | DELETE → deprecate | Unanswered | Non-blocking |

---

## Additional questions on `vault-design-spec.md`

### V1 (non-blocking) — Vault type numbering

The spec lists vault types as **1. Market Making**, **2. Delta Neutral**, **4. Delta One**. **There is no Vault Type 3.** Was something intended there (Passive LP? Options-writing?) and dropped, or is it a typo?

### V2 (BLOCKING) — Maker rebate mechanics

The spec says:

> Protocol Vaults are special actors in the ecosystem such that they receive a rebate on orders that are executed against their orders. The rebate is a percentage of the fees that are paid by the taker.

Today our fee model is: taker pays 0.05%, maker pays nothing (all fees go to the protocol / insurance fund). Under the proposed rebate:

- **V2.1** — What's the rebate rate? Half the taker fee (0.025%) or a fixed bps number?
- **V2.2** — Does the remaining fee still go to the insurance fund, or does it split somewhere else?
- **V2.3** — Is the rebate paid in RLUSD (settlement asset) or XRP (collateral)? Accrued or paid per-trade?
- **V2.4** — Which vaults qualify? Only the official vault types from this spec, or any vault registered on the protocol? If the latter, how do we prevent a wash-trading vault (self-sell, self-buy) from farming rebates?
- **V2.5** — Does the rebate apply to **all maker trades** from a protocol vault, or only to trades where the maker is providing liquidity *within the curve's spread*? (Rationale: without a spread gate, a vault can post at mid-1bp/mid+1bp and collect rebates for near-zero real risk.)

We need answers to V2.1 and V2.5 before we touch the fee-settlement code.

### V3 (non-blocking) — Delta One Vault prerequisites

The spec explicitly notes that the Delta One Vault requires:

1. Spot RLUSD/XRP market on the exchange
2. Lending protocol integration (borrow USD against XRP collateral)

**Neither exists on XRPL today** (spot XRP/RLUSD is possible via AMM but the vault would need deep liquidity; no native USD-lending protocol against XRP collateral). Clarify:

- **V3.1** — Is Delta One Vault deprioritized until those primitives exist (could be Delta One is a Phase-2 feature), or are we expected to build the lending primitives ourselves?
- **V3.2** — If deprioritized, should we drop it from the v1 vault spec entirely, or keep it documented as "Phase 2"?

Our default: keep it documented as Phase 2, don't implement in v1.

### V4 (BLOCKING if V3 is not deprioritized) — MM Vault parameters vs vAMM parameters

The MM Vault in `vault-design-spec.md` specifies these tunables:

- Min Spread, Max Spread, Order Size as % of vault, Rebalance Frequency, Max Delta, Min Delta

The vAMM in `post-hackathon-specs.md` replaces all of those with curve parameters (`k`, target delta, collateral utilization cap). Q3.5 asks whether vAMM *is* the MM Vault's new implementation. If yes, **do any of the MM Vault's original parameters survive as operator-tunable knobs**, or are they entirely replaced?

Our default: replace entirely — `k`, target delta, utilization cap, refresh cadence are the full knob set. Min/Max Spread and Min/Max Delta become derived properties of the curve, not inputs.

---

## Issue #5 — DestinationTag-based user routing for exchange deposits

**Context:** XRPL exchanges (Binance, Kraken, etc.) operate from a shared hot wallet and disambiguate users by `DestinationTag`. Our deposit scanner currently keys credit by **sender address**, which means all Binance deposits collide on the Binance hot wallet's r-address. First deposit gets credited to "user=Binance"; every subsequent one is misattributed.

Partial work shipped in commit `16a678e` (2026-04-18):
- `xrpl_monitor.rs` parses `DestinationTag` off incoming Payment txs and carries it on `DepositEvent.destination_tag`.
- `withdrawal.rs` + CLI accept `--destination-tag` and include it in the signed XRPL Payment.

What's **still missing**:
- `DepositEvent.destination_tag` is declared but never read downstream. Deposit credit still routes by `sender`. (Compiler flags it: "field never read".)
- No API for a user to obtain their deposit tag, and no enclave-side mapping from tag → user_id.
- `/v1/system/status` (and the frontend guide) warn users to deposit "from personal wallets only" — acceptable for testnet, not acceptable for a production launch that wants exchange liquidity.

We rejected `asfRequireDest` on the escrow itself (`a017de0`) — personal-wallet users must still be able to deposit without a tag. So the model is **hybrid**: tag present → credit by tag, tag absent → credit by sender.

### Q5.1 (BLOCKING) — How does a user get their DestinationTag?

Three candidate models:

- **A. Deterministic derivation** — `tag = hash(xrpl_address) % 2^32` or similar. No registration step, frontend can compute it client-side. Collision risk ~1-in-4B; mitigated by the enclave rejecting deposits whose tag doesn't match any known user (funds would sit uncredited until operator intervention).
- **B. Enclave-issued, on-demand** — `POST /v1/account/deposit-tag` (auth required) → enclave allocates a sequential u32, persists the mapping in sealed state, returns it. Clean but adds an enclave write on first use.
- **C. Tag = first 4 bytes of user_id hash, checked on deposit** — hybrid: derived client-side, but enclave validates by iterating current users (feasible at N<10k users). Avoids persistent mapping but O(N) scan per deposit.

We lean toward **B** (enclave-issued): single source of truth, no collision space to audit, fits the existing user-registration flow. But want 8Baller's input since it shapes what the frontend has to show at onboarding.

### Q5.2 (non-blocking) — Withdrawal UX

Users withdrawing to an exchange **must** supply a DestinationTag (lose funds otherwise). Two options:

- **A.** Frontend always surfaces a DestinationTag input on the withdraw modal, labeled "required for exchange withdrawals".
- **B.** Frontend detects the destination is an exchange (via an address whitelist or a heuristic) and conditionally shows the field.

We lean toward **A** — simpler, no list to maintain, no silent mislabeling.

### Q5.3 (non-blocking) — Testnet readiness

The dev instance (`api-dev.xperp.fi`) will need the routing fix too, since Tom's frontend will be hitting it with real users once onboarding flows are in. We can ship Q5.1's implementation directly to testnet first.

---

## Team's proposed order of attack

1. **Ship issue #1** (contract specs) with the defaults above — lightweight, unblocks the `get_markets` consumers. Can land in parallel with answers to blocking questions.
2. **Ship issue #5** (DestinationTag routing) — required before Tom's frontend can onboard exchange users. Small scope once Q5.1 is answered; pre-mainnet blocker.
3. **Resolve Q2.1–Q2.3, Q3.1–Q3.5, Q4.1, Q5.1, A1, A2, A3.1–A3.3** — this is the design-decision bucket. Once these are answered we can implement issues #2 (cross-margin) and #3 (vAMM).
4. **Implement cross-margin (#2)** — bigger enclave change than #3, but #3 depends on cross-margin semantics for the vault's own portfolio accounting. So cross-margin first.
5. **Implement vAMM (#3) + risk limits (#4)** as a single vault module.
6. **Ship vault API (`vault-requirements.md`)** last, once all the above is proven.

Cross-margin (#2) is where we'll spend most of the effort. We'd like 8Baller's input on Q2.1–Q2.3 (and Q5.1) before we start drafting the enclave changes.

---

## How to respond

Inline edits to this file are fine — just commit on top. Or reply per-question in chat and we'll fold the answers back in. Once the BLOCKING items are resolved, we'll open a follow-up PR with the design for each issue before writing code.
