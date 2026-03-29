# TEE Perpetual Futures Mechanics: Detailed Design

**Date:** 2026-03-29
**Status:** Design
**Prerequisite:** 01-feasibility-analysis.md (Variant B selected)
**Scope:** SGX enclave computation + XRPL mainnet settlement for XRP/RLUSD perpetual futures

---

## Overview

This document specifies how perpetual futures mechanics -- margin, funding rate, liquidation, order matching, and settlement -- operate in an architecture where an Intel SGX enclave performs all computation and XRPL mainnet handles only monetary settlement in RLUSD.

The fundamental design tension: **all position state lives inside the TEE, but all money lives on XRPL**. Every mechanism described below must bridge that gap while maintaining safety guarantees if either side experiences delays or failures.

### Key Terminology

| Term | Meaning in this system |
|---|---|
| TEE | Intel SGX enclave running the perp engine |
| Enclave Operator | Entity running the TEE hardware (semi-trusted) |
| Escrow Account | XRPL account controlled by TEE-held keys for holding collateral |
| Sealed Storage | SGX feature: data encrypted to the specific enclave identity, persisted to disk |
| Attestation | SGX remote attestation proving what code runs inside the enclave |
| Mark Price | Fair price of the perpetual contract (calculated inside TEE) |
| Index Price | Spot price of underlying (XRP/USD), sourced from external feeds |

---

## 1. Margin System in TEE

### 1.1 Design Principles

- **Collateral custody on XRPL.** Real RLUSD sits in an escrow account whose signing keys exist only inside the enclave. The enclave operator cannot move funds without enclave cooperation.
- **Margin accounting in TEE.** The enclave maintains an in-memory ledger of every user's margin balance, unrealized P&L, and position sizes. This ledger is the authoritative source for all margin checks.
- **Isolated margin only (PoC).** Each position has its own margin allocation. A liquidation on one position does not affect others. Cross-margin can be added later but complicates state recovery.

### 1.2 Deposit Flow

```
1. User generates a deposit intent (amount, user_id) signed by their XRPL keypair.
2. User submits a Payment transaction on XRPL:
     Source:      user's XRPL account
     Destination: TEE escrow account
     Amount:      N RLUSD
     Memo:        user_id (so the TEE can attribute the deposit)
3. TEE monitors XRPL ledger via a trusted connection (WebSocket to rippled).
4. TEE waits for the transaction to appear in a **validated** ledger
   (not just proposed -- must be finalized, ~3-5 seconds).
5. TEE credits the user's internal margin balance by N RLUSD.
6. TEE writes the updated balance to sealed storage.
7. TEE returns a signed receipt (enclave signature + attestation quote)
   confirming the credit.
```

**Why wait for validated ledger?** XRPL has a clear validated/proposed distinction. Only validated transactions are final. The TEE must not credit deposits on proposed-only transactions to avoid double-spend attacks where a transaction is later dropped.

**Edge case: duplicate detection.** The TEE tracks processed XRPL transaction hashes in sealed storage. If the TEE restarts and replays the ledger, it skips already-processed transactions.

### 1.3 Withdrawal Flow

```
1. User sends a signed withdrawal request to the TEE:
     { user_id, amount, destination_xrpl_address, signature }
2. TEE verifies the user's XRPL signature (proving ownership).
3. TEE checks margin sufficiency:
     available_margin = margin_balance - used_margin - unrealized_loss
     REQUIRE: available_margin >= amount
4. TEE debits the user's internal balance by amount.
5. TEE constructs and signs an XRPL Payment transaction:
     Source:      TEE escrow account
     Destination: user's XRPL address
     Amount:      amount RLUSD
6. TEE submits the signed transaction to XRPL.
7. TEE waits for validation, then updates sealed storage.
8. TEE returns the XRPL transaction hash as proof of withdrawal.
```

**Safety constraint:** The TEE never signs a withdrawal that would leave a user under-margined. The margin check in step 3 accounts for unrealized losses on all open positions.

**Rate limiting:** Withdrawals are processed in order, one per user at a time, to prevent race conditions where two concurrent withdrawal requests each pass the margin check individually but together would overdraw.

### 1.4 Margin Calculation

For a single isolated-margin position:

```
initial_margin_required = position_size * entry_price / leverage
maintenance_margin      = position_size * mark_price * maintenance_rate

where:
  maintenance_rate = 0.5% for PoC (adjustable per market)
  max leverage     = 20x for PoC

margin_ratio = (margin_balance + unrealized_pnl) / position_notional
liquidation triggers when margin_ratio <= maintenance_rate
```

Unrealized P&L:
```
For LONG:  unrealized_pnl = position_size * (mark_price - entry_price)
For SHORT: unrealized_pnl = position_size * (entry_price - mark_price)
```

All calculations use fixed-point arithmetic (integer math with 8 decimal places) to avoid floating-point non-determinism across TEE restarts or multi-enclave setups.

### 1.5 TEE Failure and Margin State Recovery

See Section 5 for full state recovery design. The key property for margins:

- **No funds are lost.** RLUSD remains in the XRPL escrow account regardless of TEE state.
- **State is recoverable.** Sealed storage contains the latest margin ledger. On restart, the TEE unseals this data and resumes.
- **Consistency check.** After recovery, the TEE sums all internal balances + outstanding positions' unrealized P&L. This must be less than or equal to the XRPL escrow balance. Any discrepancy (escrow has more than internal accounting expects) is safe -- it means some deposits arrived but were not yet credited. The TEE replays recent XRPL ledgers to catch up.

---

## 2. Funding Rate in TEE

### 2.1 Purpose

Funding rates keep the perpetual contract price anchored to the spot price. When the perp trades above spot (premium), longs pay shorts. When below (discount), shorts pay longs. This is the core mechanism that makes perpetuals track the underlying.

### 2.2 Price Sources

**Index Price (spot reference):**

The TEE requires external spot price data. Options in order of preference:

1. **Multiple CEX API feeds via TLS-attested connections.** The TEE opens TLS connections to Binance, Coinbase, Kraken, and Bitstamp REST APIs. Using TLS termination inside the enclave (the TLS session key never leaves the TEE), the enclave can prove it received authentic data from these endpoints. The index price is the **median** of the four feeds (resistant to one compromised source).

2. **XRPL on-ledger oracle (future).** If XRPL adds native oracle support (currently absent), the TEE could read price data directly from validated ledgers. Not available today.

3. **Fallback: XRPL native DEX mid-price.** The XRPL has a built-in CLOB for spot XRP. The TEE can read the order book from validated ledgers and compute a mid-price. This is a secondary check, not a primary source, because XRPL spot liquidity is thin compared to CEXs.

**Mark Price (fair perp price):**

```
mark_price = index_price * (1 + 30-second EMA of basis)

where basis = (best_bid + best_ask) / 2 / index_price - 1
```

The mark price uses the index price as a foundation and adjusts by a smoothed (EMA) basis measured from the TEE's own order book. This prevents mark price manipulation via a single large order (the EMA smooths spikes) and keeps it grounded to the index.

### 2.3 Funding Rate Calculation

Following the industry-standard approach (adapted from Binance/dYdX):

```
premium_index = (mark_price - index_price) / index_price

funding_rate = clamp(premium_index, -0.05%, +0.05%)
             + clamp(interest_rate_component, ...)

where:
  interest_rate_component = 0.01% / 8 = 0.00125% per period
  (represents the RLUSD/XRP interest rate differential, fixed for PoC)

  clamp bounds: +/- 0.05% per period (safety rails)
```

### 2.4 Funding Application Schedule

- **Calculation frequency:** Every second internally (TEE updates a running TWAP of the premium index over the funding period).
- **Application frequency:** Every **8 hours** (00:00, 08:00, 16:00 UTC), matching CEX convention.
- **Application method:** At the funding timestamp, the TEE:
  1. Computes the final funding rate from the 8-hour TWAP of premium index.
  2. For each open position, calculates the funding payment:
     `funding_payment = position_size * mark_price * funding_rate`
  3. Debits/credits each user's margin balance accordingly.
  4. Writes the updated state to sealed storage.
  5. Publishes a signed funding rate report (rate, timestamp, all inputs) for transparency.

### 2.5 Funding Settlement on XRPL

Funding payments are **internal ledger adjustments** within the TEE. They do not trigger individual XRPL transactions. Instead:

- Funding credits increase a user's available margin (allowing larger withdrawals).
- Funding debits decrease a user's available margin (potentially triggering liquidation if margin becomes insufficient).
- The net effect settles on XRPL only when a user withdraws or is liquidated, or during periodic batch settlement (Section 7).

**Rationale:** Funding is a zero-sum transfer between longs and shorts. Moving RLUSD on XRPL for every funding payment would be expensive and unnecessary. The TEE escrow account balance represents the total system collateral; individual allocations are tracked internally.

---

## 3. Liquidation in TEE

### 3.1 Monitoring

The TEE continuously evaluates all open positions against the mark price. On every mark price update (approximately once per second):

```
for each position:
    margin_ratio = (position_margin + unrealized_pnl) / abs(position_notional)
    if margin_ratio <= maintenance_margin_rate:
        trigger_liquidation(position)
```

With isolated margin, each position is evaluated independently.

### 3.2 Liquidation Process

```
1. TEE identifies position P where margin_ratio <= maintenance_rate.
2. TEE cancels all open orders for this user in this market
   (to prevent the margin situation from worsening).
3. TEE attempts to close position P at the current mark price
   by placing a forced liquidation order into the order book.
   - This is a market order with no price limit.
   - It matches against resting limit orders from other traders.
4. Once filled:
   a. If remaining_margin > 0 after closing at fill price:
        - Return remaining_margin to user's free margin balance.
   b. If remaining_margin < 0 (position was underwater):
        - The loss exceeding margin is absorbed by the Insurance Fund.
5. TEE updates sealed storage with the liquidation result.
6. TEE publishes a signed liquidation receipt:
   { position_id, user_id, fill_price, margin_returned, insurance_used,
     enclave_signature, timestamp }
```

### 3.3 Trustlessness of Liquidation

**Can liquidation be trustless if only the TEE can trigger it?**

Not fully trustless, but **verifiably honest** under the SGX trust model:

- **Deterministic liquidation logic.** The liquidation rules are part of the attested enclave code. Remote attestation proves exactly which code is running. If the code says "liquidate at margin_ratio <= 0.5%", then users can verify (via attestation) that this is the rule that will be applied.
- **No operator discretion.** The enclave operator cannot selectively liquidate or delay liquidation. The code runs deterministically inside the enclave.
- **Verifiable after the fact.** Each liquidation receipt includes the mark price, index price inputs, position details, and margin calculation. Anyone can recheck the math.
- **Remaining risk: TEE compromise.** If SGX is broken (side-channel attack, compromised Intel key), the operator could theoretically manipulate liquidations. Mitigation: see Section 6 (Trust Model).

**Comparison to centralized exchanges:** This is strictly better than CEX liquidation, where users trust opaque systems. Here, attestation proves the liquidation logic, and receipts provide post-hoc verifiability.

### 3.4 Insurance Fund

The insurance fund covers negative-equity liquidations (socialized loss prevention).

**Funding sources:**
1. **Liquidation penalties.** When a position is liquidated, a small penalty fee (0.5% of position notional) is charged. If the margin remaining after closing the position exceeds zero, the penalty is deducted from the returned margin and added to the insurance fund.
2. **Trading fee allocation.** A portion of trading fees (e.g., 20%) is allocated to the insurance fund.
3. **Seed capital.** The protocol operator deposits initial RLUSD into the insurance fund.

**Mechanics:**
- The insurance fund is an internal balance within the TEE ledger, backed by RLUSD in the escrow account.
- When a liquidation results in negative equity, the insurance fund balance is reduced by the shortfall.
- If the insurance fund is depleted, **auto-deleveraging (ADL)** activates: the most profitable positions on the opposing side are forcibly reduced to cover the loss. This is a last resort.

**Insurance fund on XRPL:**
- The insurance fund balance is included in periodic state reports published by the TEE.
- In a full shutdown scenario, the insurance fund's RLUSD is distributed according to the final sealed state.

---

## 4. Order Matching in TEE

### 4.1 Encrypted Order Submission

```
1. User obtains the enclave's public key via remote attestation.
   (This key is generated inside the enclave and bound to the attestation quote.)
2. User constructs an order:
   { market, side, type, price, size, leverage, user_id, nonce, timestamp }
3. User encrypts the order with the enclave's public key (ECDH + AES-256-GCM).
4. User signs the encrypted payload with their XRPL private key
   (proves identity without revealing order contents).
5. User sends the encrypted+signed order to the TEE API endpoint.
6. TEE decrypts the order inside the enclave.
7. TEE verifies the user signature matches a registered XRPL account.
8. TEE checks margin sufficiency for the new order.
9. Order is placed into the internal order book.
```

**Anti-MEV guarantee:** The enclave operator sees only encrypted bytes. Order contents are revealed only inside the enclave after decryption. The operator cannot front-run, sandwich, or selectively delay orders based on content.

**Replay protection:** The nonce + timestamp in each order prevents replay. The TEE rejects orders with a previously-seen nonce or a timestamp more than 30 seconds old.

### 4.2 Order Book (CLOB) Design

The TEE maintains a Central Limit Order Book:

**Supported order types (PoC):**
- **Limit order:** rests on the book at a specified price.
- **Market order:** matches immediately at the best available price(s).
- **Limit IOC (Immediate-or-Cancel):** matches at specified price or better, cancels remaining.

**Matching engine:**
- Price-time priority (standard CLOB).
- Matching runs synchronously inside the enclave on each new order arrival.
- Deterministic: same sequence of orders always produces the same matches.

**Data structures:**
- Two sorted arrays (bids descending, asks ascending) per market.
- For PoC (single market, moderate volume), simple sorted vectors are sufficient. No need for more complex structures.

### 4.3 Trade Execution and Position Updates

When a match occurs:

```
1. TEE records the trade: { trade_id, maker_order, taker_order, price, size, timestamp }
2. TEE updates both users' positions:
   - If user has no existing position: create new position at fill price.
   - If same direction: increase position size, compute new average entry price.
   - If opposite direction: reduce or flip position, realize P&L.
3. TEE adjusts margin allocations:
   - For new/increased position: lock initial_margin from free margin.
   - For reduced position: release margin proportional to size reduction.
   - For realized P&L: add to (or subtract from) user's free margin.
4. TEE writes updated state to sealed storage (batched, not per-trade).
5. TEE sends execution reports to both users (encrypted to each user's key).
```

**Execution reports** include: trade_id, price, size, fee, new position details, new margin balance, enclave signature. Users can verify the enclave signature using the attested public key.

### 4.4 Trading Fees

```
maker_fee = 0.02% of notional (incentivizes liquidity provision)
taker_fee = 0.05% of notional

Fee is deducted from the user's free margin at trade time.
Fee allocation:
  - 60% to protocol revenue (withdrawable by operator)
  - 20% to insurance fund
  - 20% to maker rebate pool (optional, for incentivizing market makers)
```

---

## 5. State Management

### 5.1 State Architecture

All position state lives in TEE memory during operation. This is the single most critical design area, because TEE memory is volatile.

**State components:**

| Component | Size estimate (single market, 10K users) | Persistence |
|---|---|---|
| User margin balances | ~160 KB | Sealed storage |
| Open positions | ~400 KB | Sealed storage |
| Order book | ~200 KB (5K open orders) | Sealed storage |
| Trade history (recent) | ~2 MB (last 100K trades) | Sealed storage + external log |
| Funding rate state | ~1 KB | Sealed storage |
| Insurance fund balance | ~64 bytes | Sealed storage |
| Processed XRPL tx hashes | ~500 KB | Sealed storage |

Total sealed state: under 5 MB. Easily fits in enclave memory and seals quickly.

### 5.2 Sealed Storage and Snapshots

SGX sealed storage encrypts data with a key derived from the enclave's identity (MRENCLAVE) and the CPU's hardware key. Only the same enclave code on the same CPU can unseal it.

**Snapshot strategy:**

1. **Continuous journaling.** Every state mutation (trade, deposit, withdrawal, funding, liquidation) is appended to a write-ahead log (WAL) in sealed storage. This is fast (append-only) and provides point-in-time recovery.

2. **Periodic full snapshots.** Every 60 seconds, the TEE writes a complete state snapshot to sealed storage. This bounds recovery time (replay at most 60 seconds of WAL on restart).

3. **Snapshot format.** A deterministic serialization of all state components + a SHA-256 hash + the XRPL ledger sequence number at snapshot time. The ledger sequence anchors the snapshot to a specific point in XRPL history.

### 5.3 State Recovery After TEE Restart

```
1. Enclave starts, loads sealed snapshot + WAL from sealed storage.
2. Replays WAL entries after the snapshot to reconstruct latest state.
3. Connects to XRPL and fetches all validated ledgers from
   (snapshot_ledger_sequence + 1) to current.
4. Processes any deposits that arrived while the TEE was down
   (credits margin balances).
5. Re-evaluates all positions at current mark price
   (may trigger liquidations if price moved significantly).
6. Resumes accepting orders.
```

**Downtime window risk:** During the time the TEE is offline, no liquidations execute. If the market moves violently, positions that should have been liquidated may go deeply negative. The insurance fund absorbs this. To mitigate:

- Keep TEE restart time under 30 seconds (achievable with the small state size).
- Use a watchdog process that monitors TEE liveness and alerts the operator.
- In a prolonged outage (>5 minutes), the TEE on recovery enters a **liquidation-only mode**: it processes all pending liquidations before accepting new orders.

### 5.4 TEE Hardware Failure (Unrecoverable)

If the SGX CPU fails permanently, sealed storage cannot be unsealed on a different CPU (by SGX design). This is the worst-case scenario.

**Mitigations:**

1. **Encrypted state export.** Periodically (every 5 minutes), the TEE encrypts a full state snapshot with a key derived from a **sealing key backup protocol:**
   - At initial setup, the enclave generates a master key and splits it using Shamir's Secret Sharing (3-of-5 threshold).
   - The 5 shares are distributed to independent custodians (e.g., protocol multisig holders).
   - The encrypted snapshot is written to external durable storage (outside the enclave, encrypted and therefore safe).
   - To recover: 3 custodians provide their shares, the master key is reconstructed inside a new enclave (which is attested first), and state is restored.

2. **XRPL as ground truth for collateral.** Even if all internal state is lost, the XRPL escrow account balance is visible on-chain. A worst-case recovery could distribute escrow funds pro-rata to depositors based on their deposit transaction history (all visible on XRPL ledger). This is not ideal (open P&L is lost) but prevents total loss of funds.

3. **Redundant TEE (future).** Run two enclave instances on different hardware. Both process the same order stream. If one fails, the other continues. Requires a consensus protocol between enclaves (beyond PoC scope, but the architecture should not preclude it).

---

## 6. Trust Model

### 6.1 What Does the User Trust?

| Trusted Component | What it provides | Failure mode |
|---|---|---|
| **Intel SGX** | Code confidentiality and integrity inside the enclave | Side-channel attacks, Intel key compromise |
| **Enclave code** (attested) | Correct execution of perp mechanics | Bugs in the code (auditable via open-source + attestation) |
| **TEE operator** | Liveness (keeping the TEE running), network connectivity | Operator goes offline (positions frozen, no liquidations) |
| **XRPL validators** | Settlement finality, RLUSD transfer correctness | XRPL consensus failure (extremely unlikely, production since 2012) |
| **Price feed sources** | Accurate index price | Exchange API manipulation (mitigated by median of 4 sources) |

**What the user does NOT need to trust:**
- The TEE operator cannot see order contents (encrypted).
- The TEE operator cannot move funds without enclave cooperation (keys inside enclave).
- The TEE operator cannot manipulate matching, liquidation, or funding (attested code).

### 6.2 Remote Attestation Flow

```
1. User connects to the TEE API and requests an attestation report.
2. TEE generates an SGX quote:
   - MRENCLAVE: hash of the enclave binary (proves which code is running)
   - MRSIGNER: hash of the signing key (proves who built the code)
   - User data: enclave's public key for order encryption + nonce
3. The quote is signed by the CPU's attestation key (rooted in Intel's CA).
4. User (or user's client software) verifies:
   a. The quote signature chains to Intel's root CA.
   b. MRENCLAVE matches the published, audited enclave binary.
   c. The enclave is running on up-to-date hardware (no known vulnerabilities).
   d. The public key in the quote is the one they will use to encrypt orders.
5. If verification passes, the user can trust that:
   - The code running inside the enclave is exactly the audited code.
   - Only that code can decrypt their orders and access the signing keys.
```

**Practical implementation:** Client-side attestation verification should be handled by the trading client (web app or CLI). The open-source client code itself is auditable, closing the trust loop.

### 6.3 Attack Vectors and Mitigations

| Attack | Impact | Mitigation |
|---|---|---|
| **SGX side-channel (e.g., Spectre-class)** | Attacker extracts enclave secrets (order data, signing keys) | Keep SGX microcode updated; use SGX hardening best practices (constant-time code, ORAM for memory access patterns); monitor Intel security advisories |
| **Operator denial-of-service** | Operator shuts down TEE; no trading, no liquidations | Watchdog alerting; clear SLA commitments; user deposits remain safe on XRPL (can be recovered); future: redundant TEE |
| **Operator network manipulation** | Operator delays or drops specific user packets | TLS between user and enclave (terminated inside enclave); operator sees only encrypted traffic; timing attacks partially mitigated by batched processing |
| **Price feed manipulation** | Attacker compromises one exchange API, feeds false price | Median of 4 sources; outlier detection (reject any source >2% from median); staleness check (reject data older than 10 seconds) |
| **XRPL reorg / settlement failure** | Deposit credited but XRPL transaction later reverts | Only credit deposits from validated ledgers (XRPL has no reorgs once validated) |
| **Enclave code bug** | Incorrect margin calculation, bad liquidation | Open-source enclave code; third-party audit; deterministic builds (reproducible MRENCLAVE); bug bounty program |
| **Shamir key custodian collusion** | 3-of-5 custodians collude to reconstruct master key and steal state backup | Custodians are geographically and organizationally diverse; master key only useful inside an attested enclave (encrypted snapshot requires enclave to decrypt) |

### 6.4 Trust Comparison

```
Centralized Exchange:  Trust the operator completely (opaque systems)
Smart Contract DEX:    Trust the blockchain + smart contract code
This TEE DEX:          Trust Intel SGX + attested code + XRPL for settlement
```

The TEE model sits between CEX and on-chain DEX in terms of trust assumptions. It is weaker than a fully on-chain solution (SGX is not as battle-tested as Ethereum's security model), but far stronger than a centralized exchange (the operator cannot cheat without breaking SGX). For XRPL, where on-chain smart contracts are unavailable, this is the strongest achievable trust model.

---

## 7. Settlement Batching

### 7.1 Why Batch?

Individual XRPL transactions cost approximately 0.00001 XRP (negligible) but each takes 3-5 seconds for finality and each consumes ledger space. More importantly, the TEE escrow account would need to submit potentially thousands of transactions per minute if every trade settled individually. This is impractical and creates a bottleneck.

Instead, the TEE settles **net P&L** periodically.

### 7.2 Settlement Triggers

Settlement occurs under any of these conditions:

| Trigger | Description |
|---|---|
| **Time-based** | Every 1 hour, regardless of activity |
| **Threshold-based** | When any user's unsettled P&L exceeds $10,000 |
| **User-initiated** | User requests withdrawal (forces settlement of their account) |
| **Liquidation** | Liquidated positions settle immediately |
| **Risk-based** | When total unsettled P&L across all users exceeds $100,000 |

### 7.3 Batch Settlement Process

```
1. TEE computes net settlement for each user since last settlement:
     net_settlement[user] = realized_pnl + funding_payments - fees

2. TEE groups users into:
   - Receivers: net_settlement > 0 (they are owed RLUSD)
   - Payers: net_settlement < 0 (they owe RLUSD)

3. For payers: no XRPL transaction needed.
   Their margin balance in the TEE ledger is already reduced.
   The RLUSD stays in the escrow account.

4. For receivers: if the user wants to keep trading, no XRPL transaction
   needed either -- the credit stays as available margin in the TEE ledger.
   If the user has pending withdrawal requests, the TEE processes them now.

5. The TEE publishes a signed **settlement report**:
   { batch_id, timestamp, ledger_sequence,
     per_user_net_settlements, escrow_balance_before, escrow_balance_after,
     enclave_signature }

6. Anyone can verify:
   - Sum of all net settlements = 0 (zero-sum check)
   - Escrow balance on XRPL matches the TEE's reported escrow_balance_after
```

### 7.4 Actual XRPL Transactions

XRPL transactions only occur for:

| Event | XRPL Transaction |
|---|---|
| User deposit | Payment: user -> escrow (initiated by user) |
| User withdrawal | Payment: escrow -> user (signed by TEE) |
| Liquidation with margin return | Payment: escrow -> liquidated user (remaining margin) |
| Insurance fund top-up from external source | Payment: operator -> escrow |
| Protocol fee withdrawal | Payment: escrow -> operator (periodic) |

In steady state (no deposits or withdrawals), the escrow account balance does not change. All P&L, funding, and fee movements are internal ledger entries within the TEE.

### 7.5 Frequency vs. Cost vs. Risk Tradeoffs

| Settlement frequency | Cost | Risk |
|---|---|---|
| Per-trade | Very high (thousands of XRPL txns/min) | Lowest (instant settlement) |
| Every 1 minute | High | Low |
| **Every 1 hour** | Low (~60 txns/day for withdrawals) | **Moderate (acceptable)** |
| Every 24 hours | Minimal | High (large unsettled balances) |

**Recommended for PoC: 1-hour time-based + threshold overrides.**

The primary risk of infrequent settlement is that the TEE's internal ledger diverges significantly from on-chain reality. If the TEE fails, the gap between the escrow balance and what users are owed (based on latest sealed state) is bounded by the settlement interval. With 1-hour settlement + $100K threshold override, the maximum unsettled exposure is bounded and manageable.

---

## 8. System Lifecycle

### 8.1 Initial Setup

```
1. Deploy enclave binary (open source, deterministic build).
2. Enclave generates:
   - XRPL keypair for escrow account (private key never leaves enclave).
   - Encryption keypair for order encryption.
   - Shamir shares of backup master key (distributed to custodians).
3. Publish remote attestation report with:
   - MRENCLAVE hash
   - Escrow account XRPL address
   - Order encryption public key
4. Fund escrow account with small XRP reserve (for XRPL transaction fees).
5. Operator configures:
   - Market parameters (XRP/RLUSD perp, maintenance margin, max leverage)
   - Funding rate parameters
   - Settlement schedule
6. TEE enters operational mode and begins accepting deposits and orders.
```

### 8.2 Graceful Shutdown

```
1. TEE stops accepting new orders.
2. TEE cancels all open orders, returning margin.
3. TEE enters settlement-only mode.
4. Users withdraw all available margin.
5. For users with open positions: TEE force-closes at mark price.
6. TEE settles all remaining balances on XRPL.
7. TEE performs final sealed storage snapshot.
8. TEE shuts down.
```

### 8.3 Upgrade Path

Enclave upgrades change MRENCLAVE (the code hash). Users must re-attest. The upgrade process:

```
1. New enclave binary is published and audited.
2. Old enclave exports encrypted state (using Shamir backup key).
3. New enclave starts, attests, and imports state.
4. Users verify new attestation and resume trading.
5. Old enclave shuts down.
```

---

## 9. PoC Simplifications

For the April 15 PoC deadline, the following simplifications are acceptable:

| Full Design | PoC Simplification |
|---|---|
| 4 CEX price feeds with median | 1 price feed (Binance XRP/USDT) |
| Shamir backup key (3-of-5) | Single backup key held by operator |
| 1-hour batch settlement + thresholds | Settle on every trade (acceptable at low volume) |
| Multiple markets | Single market: XRP/RLUSD |
| Maker/taker fee tiers | Flat 0.05% fee |
| ADL (auto-deleveraging) | Not implemented; insurance fund only |
| Redundant TEE | Single TEE instance |
| Production attestation verification | Simulated attestation (SGX simulator mode acceptable for demo) |

---

## 10. Open Questions

1. **Payment channels.** XRPL payment channels allow off-chain micropayments with on-chain settlement. Could these be used to enable faster user withdrawals without waiting for ledger validation? Requires further investigation of payment channel mechanics with RLUSD (payment channels may only support XRP natively).

2. **Multi-collateral.** Could XRP itself be used as collateral alongside RLUSD? This adds complexity (variable collateral value, haircuts) but may improve capital efficiency for XRP holders.

3. **Enclave-to-enclave replication.** For production, a redundancy protocol is needed. Options: primary-backup with shared WAL, or active-active with deterministic replay. Both have tradeoffs in complexity and latency.

4. **Regulatory.** Perpetual futures are regulated derivatives in most jurisdictions. The TEE architecture does not change the regulatory classification. Legal analysis is needed before production launch.

5. **MEV on XRPL settlement.** While order matching is MEV-resistant (encrypted orders in TEE), the XRPL settlement transactions are visible. Could validators front-run settlement? Likely not material since settlements are net P&L transfers (not price-sensitive trades), but worth analyzing.
