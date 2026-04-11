# Security Audit Report: XRPL Perpetual DEX

**Date**: 2026-04-07
**Auditor**: Claude Code Security Audit
**Scope**: Orchestrator (Rust, 5.3K LOC) + Enclave (C/C++, custom code)
**Repos**: `xrpl-perp-dex` (orchestrator) + `xrpl-perp-dex-enclave` (SGX enclave)

---

## Summary

| Severity | Count | Description |
|----------|:-----:|-------------|
| Critical | 4 | Unsigned withdrawals, unauthenticated price manipulation, replay attacks, unverified deposits |
| High | 9 | Double-spend, unauthorized cancellation, hardcoded keys, deposit replay, broken funding, FOK, fill rollback, negative margin, tx hash overflow |
| Medium | 15 | Arithmetic issues, amount corruption, missing validation, state consistency |
| Low/Info | 17 | Debug mode, TLS disabled, timing attacks, performance |
| **Total** | **45** | |

---

## CRITICAL

### C-01: Withdrawal Signature Discarded (Orchestrator)
**File:** `orchestrator/src/withdrawal.rs:159-192`

The `submit_xrpl_payment` function receives `_signature_hex` (note underscore — unused parameter) but never includes it in the XRPL transaction. The enclave-signed signature is obtained and thrown away. The transaction is submitted via `sign-and-submit` which requires the XRPL node to hold the private key — defeating the TEE security model.

**Impact:** Withdrawal flow is either broken (rejected by XRPL) or relies on the node holding the escrow key, negating enclave security.

**Fix:** Include `TxnSignature` and `SigningPubKey` fields in the submitted `tx_blob`, using the enclave-provided signature.

### C-02: Price Update / Liquidation / Funding Without Authentication (Enclave)
**File:** `Enclave/Enclave.cpp:4380-4385` (price), `Enclave.cpp:4446-4471` (funding), `perp_handler.cpp:507-554` (API)

The `ecall_perp_update_price`, `ecall_perp_apply_funding`, and `ecall_perp_liquidate` ecalls require NO session key or authentication. Anyone with API access can:
1. Set arbitrary mark/index prices → manipulate all PnL and margin calculations
2. Apply arbitrary funding rates → drain margin from all longs or shorts
3. Liquidate any position at manipulated prices

**Impact:** Total takeover. Attacker sets extreme prices, triggers mass liquidations, drains insurance fund.

**Fix:** Require admin session key for price/funding/liquidation ecalls. Verify price source (oracle signature).

### C-03: No Replay Protection on Authentication (Orchestrator)
**File:** `orchestrator/src/auth.rs:33-123`

The `verify_request` function verifies ECDSA signatures but includes no nonce, timestamp, or sequence number. Any valid signed request can be captured and replayed indefinitely.

**Impact:** Intercepted requests can drain funds, duplicate orders, or cancel all orders.

**Fix:** Include monotonic nonce or timestamp in signed payload. Reject requests older than N seconds or with reused nonces.

### C-04: Deposit Credit Without On-Chain Verification (Enclave)
**File:** `Enclave/Enclave.cpp:4208-4230`

The enclave trusts the orchestrator to report deposits correctly. `ecall_perp_deposit_credit` accepts any user_id, amount, and tx_hash without verifying the transaction on XRPL. A compromised orchestrator can fabricate deposits with made-up hashes.

**Impact:** Unlimited fake deposits. Complete fund theft.

**Fix:** Verify XRPL transaction proof inside the enclave (SPV proof or attested oracle), or implement multi-party deposit confirmation.

---

## HIGH

### H-01: Balance Deducted Before XRPL Confirmation (Orchestrator)
**File:** `orchestrator/src/withdrawal.rs:131-143`

When enclave signs a withdrawal but XRPL submission fails, balance is already deducted. Status returns `"signed_but_not_submitted"`. If the original tx actually went through but response was lost, retry creates double-spend.

**Fix:** Use two-phase commit: deduct on enclave sign, confirm on XRPL receipt, rollback on timeout.

### H-02: Cancel Order Without Authorization (Orchestrator)
**File:** `orchestrator/src/api.rs:495-506`

`cancel_order` takes only `order_id` with no user_id verification. Any authenticated user can cancel any other user's orders.

**Fix:** Verify `order.user_id == authenticated_user_id` before cancellation.

### H-03: Hardcoded Session Key for Withdrawals (Orchestrator)
**File:** `orchestrator/src/api.rs:632`

`let session_key = "00".repeat(32);` — all-zeros session key for enclave withdrawal signing.

**Fix:** Generate and securely store session key during enclave initialization.

### H-04: Deposits Re-Credited on Restart (Orchestrator)
**File:** `orchestrator/src/main.rs:398,446-459`

`last_ledger` is in-memory `u32` initialized to 0 on restart. All historical deposits get re-scanned and re-credited.

**Fix:** Persist `last_ledger` to disk. Use enclave's tx_hash dedup as backup (but see H-09).

### H-05: Funding Rate Always Zero (Orchestrator)
**File:** `orchestrator/src/main.rs:469`

`let rate = compute_funding_rate(current_price, current_price);` — mark and index are the same variable. Funding never corrects perpetual price deviation.

**Fix:** Use separate mark price (from orderbook mid) and index price (from oracle/Binance).

### H-06: FOK (Fill-or-Kill) Not Enforced (Orchestrator)
**File:** `orchestrator/src/orderbook.rs:242-245`

No pre-check for available liquidity. FOK orders that partially fill are not atomically rejected. Maker orders get consumed.

**Fix:** Pre-scan available liquidity before matching. Reject FOK if insufficient.

### H-07: Fills Not Rolled Back on Enclave Reject (Orchestrator)
**File:** `orchestrator/src/trading.rs:93-180`

When enclave rejects a position (insufficient margin), orderbook modifications are not reverted. Maker orders consumed without creating positions.

**Fix:** Use tentative matching: mark fills as pending, commit only after enclave confirms.

### H-08: Funding Drives Margin Negative Without Liquidation (Enclave)
**File:** `Enclave/Enclave.cpp:4446-4471`

Funding applied directly to `margin_balance` without checking for negative. No liquidation triggered. Creates bad debt.

**Fix:** Check margin after funding. If equity < maintenance margin, mark for liquidation.

### H-09: Tx Hash Dedup Stops After 500 Entries (Enclave)
**File:** `Enclave/Enclave.cpp:4223-4227`

After `MAX_TX_HASHES` (500), new hashes are not stored but deposits still credit. Same tx_hash can be replayed.

**Fix:** Use a rolling buffer or hash set with eviction policy. Reject deposits when hash table is full.

---

## MEDIUM

### M-01: FP8 Division by Zero Panic (Orchestrator)
**File:** `orchestrator/src/types.rs:113-116`

`Div` impl has no zero check. `FP8::ZERO` as divisor panics the server.

### M-02: FP8 Overflow on Large Values (Orchestrator)
**File:** `orchestrator/src/types.rs:103-108`

`i128` intermediate cast to `i64` can silently overflow for values > ~92 billion.

### M-03: Withdrawal Amount Corruption (Orchestrator)
**File:** `orchestrator/src/withdrawal.rs:187`

`trim_end_matches('0')` is greedy: "10.00000000" → "1." → "1". Withdrawals of 10, 100, 1000 RLUSD become 1.

**Fix:** Only trim zeros after decimal point: split on '.', trim fractional zeros, rejoin.

### M-04: P2P Batch Messages Not Authenticated (Orchestrator)
**File:** `orchestrator/src/p2p.rs:273-283`

Order batches from gossipsub not verified against elected leader. Rogue validators can inject fake batches.

### M-05: State Hash Is Timestamp, Not Real Hash (Orchestrator)
**File:** `orchestrator/src/trading.rs:211`

`state_hash: format!("{:016x}", now)` — no actual state verification between nodes.

### M-06: CORS Allows All Origins (Orchestrator)
**File:** `orchestrator/src/api.rs:141-144`

`allow_origin("*")` enables cross-origin attacks on web clients.

### M-07: Withdrawal Destination Not Validated (Orchestrator)
**File:** `orchestrator/src/withdrawal.rs`, `orchestrator/src/api.rs:626-654`

No XRPL address format validation. Invalid destination → fund loss after enclave deducts balance.

### M-08: Deposit Amount Through f64 — Precision Loss (Orchestrator)
**File:** `orchestrator/src/xrpl_monitor.rs:131-148`

XRPL amounts (up to 15 digits) lose precision through `f64` conversion.

### M-09: Integer Overflow in Margin Calculations (Enclave)
**File:** `Enclave/PerpState.h:161-168`

`fp_mul` uses `__int128` but casts result to `int64_t`. Notional > ~92B overflows, causing under-collateralized positions.

### M-10: fp_div Returns 0 on Division by Zero (Enclave)
**File:** `Enclave/PerpState.h:166`

Silent corruption. Vault share price of 0 → deposit accepted but 0 shares issued → funds stolen.

### M-11: Closed Positions Never Garbage-Collected (Enclave)
**File:** `Enclave/Enclave.cpp:4344`

`position_count` only increments. After 800 positions, no new positions possible even if all closed.

### M-12: Session Key Comparison Not Constant-Time (Enclave)
**File:** `Enclave/Enclave.cpp:2130`

`memcmp` enables timing side-channel attacks on session key.

### M-13: State Persistence Not Atomic (Enclave)
**File:** `Enclave/Enclave.cpp:4739-4797`

5 separate seal/unseal operations. Crash mid-save → inconsistent state on reload.

### M-14: Vault Deposit Checks Raw Margin Instead of Available (Enclave)
**File:** `Enclave/Enclave.cpp:4587-4588`

Users with open positions can deposit entire margin_balance into vault, leaving positions under-collateralized.

### M-15: No Rate Limiting on Enclave API (Enclave)
**File:** `server/server.cpp:92-178`

4 civetweb threads, no rate limiting. Trivial DoS.

---

## LOW / INFORMATIONAL

| # | Component | Issue | File |
|---|-----------|-------|------|
| L-01 | Enclave | secp256k1 stubs return 0 — if linked, all crypto broken | `secp256k1_stubs.cpp` |
| L-02 | Enclave | PerpState static global without mutex — race conditions | `PerpEngine.cpp:22` |
| L-03 | Enclave | SGX_DEBUG_FLAG=1 — keys extractable with debugger | `App.cpp:33` |
| L-04 | Enclave | session_key validation checks wrong variable | `pool_handler.cpp:249` |
| L-05 | Enclave | TLS disabled in Python client | `perp_client.py:9-10` |
| L-06 | Enclave | DCAP quote generation without auth | `pool_handler.cpp:90-125` |
| L-07 | Enclave | Debug logging leaks sensitive info via ocall | `Enclave.cpp:170-179` |
| L-08 | Enclave | `system("rm -f")` command injection risk | `App.cpp:44` |
| L-09 | Orchestrator | Enclave TLS cert verification disabled | `perp_client.rs:19` |
| L-10 | Orchestrator | Election heartbeat seq_num not validated | `election.rs:129-169` |
| L-11 | Orchestrator | Auth bypass for non-JSON bodies | `auth.rs:177-195` |
| L-12 | Orchestrator | GET signature not normalized | `auth.rs:97-101` |
| L-13 | Orchestrator | Gossipsub message ID uses non-crypto hash | `p2p.rs:117-121` |
| L-14 | Orchestrator | URL injection in perp_client | `perp_client.rs:99` |
| L-15 | Orchestrator | Order/Trade ID u64 overflow (theoretical) | `orderbook.rs:124-125` |
| L-16 | Orchestrator | `recent_trades.remove(0)` is O(n) | `orderbook.rs:468` |
| L-17 | Orchestrator | mid_price truncation bias | `orderbook.rs:158` |

---

## Recommended Fix Priority

**Immediate (before any deployment):**
1. C-02: Add authentication to price/funding/liquidation endpoints
2. C-01: Use enclave signature in XRPL transaction submission
3. C-03: Add nonce/timestamp to authenticated requests
4. M-03: Fix withdrawal amount trimming (10→1 bug)
5. H-04: Persist last_ledger to disk

**Before testnet:**
6. C-04: Verify deposits on-chain or multi-party
7. H-02: Add user_id check to cancel_order
8. H-03: Replace hardcoded session key
9. H-05: Separate mark and index prices for funding
10. H-07: Roll back orderbook fills on enclave reject

**Before mainnet:**
11. All remaining High and Medium findings
12. L-03: Switch to release-signed enclave (SGX_DEBUG_FLAG=0)
13. L-02: Add mutex to PerpState for concurrent access
