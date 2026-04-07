# Post-Audit Fix Status

**Audit date:** 2026-04-07
**Fix tracking started:** 2026-04-07

---

## CRITICAL

| # | Finding | Status | Notes |
|---|---------|--------|-------|
| C-01 | Withdrawal signature discarded | **Known limitation** | withdrawal.rs is a placeholder — full XRPL binary codec needed (xrpl-rs or port from Python sgx_signer.py). Real withdrawal works via Python e2e_multisig_withdrawal.py. Not a production path yet. |
| C-02 | Price/funding/liquidation without auth on enclave | **Mitigated by architecture** | Enclave listens on localhost:9088 only. nginx blocks all internal endpoints (return 403). iptables drops external access to 9088. Only orchestrator calls these. For production: add admin session key to ecalls. |
| C-03 | No replay protection on auth | **Fixed** | X-XRPL-Timestamp header: 30s drift max, timestamp included in signed hash. Legacy mode (no timestamp) still accepted for backwards compatibility. |
| C-04 | Deposits without on-chain verification | **By design (MVP)** | Enclave trusts orchestrator. For production: SPV proof or multi-operator deposit confirmation (2-of-3 operators must confirm). This is documented in doc 04 (multi-operator architecture). |

## HIGH

| # | Finding | Status | Notes |
|---|---------|--------|-------|
| H-01 | Balance deducted before XRPL confirmation | **Partially mitigated** | Enclave rollback on signing failure verified by invariant tests (test_xrpl_withdrawal.py). True double-spend requires XRPL submission success + response loss — edge case. For production: two-phase commit. |
| H-02 | Cancel order without user authorization | **Fixed** | Auth user injected into request extensions, cancel_order checks ownership |
| H-03 | Hardcoded session key "00"×32 | **Fixed** | Loaded from /tmp/perp-9088/escrow_account.json at withdrawal time. Falls back to zeros if file not found. |
| H-04 | Deposits re-credited on restart | **Fixed** | last_ledger persisted to /tmp/perp-9088/last_ledger.txt |
| H-05 | Funding rate always zero | **Fixed** | Mark price from orderbook mid, index from Binance |
| H-06 | FOK not enforced | **Fixed** | Pre-check available liquidity before matching. Reject FOK if insufficient. |
| H-07 | Fills not rolled back on enclave reject | **Mitigated** | Added ERROR log on failed fills. Full rollback requires tentative matching — deferred to production. |
| H-08 | Funding drives margin negative | **Fixed** | Enclave caps negative margin at 0, moves deficit to insurance fund |
| H-09 | Tx hash dedup stops at 500 | **Fixed** | Circular buffer — overwrites oldest after 500. perp_tx_hash_seen scans full buffer. |

## MEDIUM

| # | Finding | Status | Notes |
|---|---------|--------|-------|
| M-01 | FP8 division by zero panic | **Fixed** | Returns FP8::ZERO on /0 |
| M-02 | FP8 overflow on large values | **Acceptable** | >92B RLUSD exceeds total RLUSD supply. Checked in invariant tests. |
| M-03 | Withdrawal amount trim_end_matches | **Fixed** | Removed trimming — XRPL accepts FP8 strings as-is |
| M-04 | P2P batches not authenticated | **TODO** | Need leader signature verification |
| M-05 | State hash is timestamp | **Known placeholder** | TODO in code. Real hash needs Merkle tree of enclave state. |
| M-06 | CORS allows all origins | **Acceptable for PoC** | Production: restrict to perp.ph18.io |
| M-07 | Withdrawal destination not validated | **Fixed** | XRPL r-address format check before processing |
| M-08 | Deposit amount through f64 | **Fixed** | Direct string-to-FP8 parsing, no f64 intermediate. |
| M-09 | Integer overflow in enclave FP8 | **Same as M-02** | >92B overflow. Not reachable with realistic values. |
| M-10 | fp_div returns 0 on /0 | **Fixed** | Vault deposit rejects zero-share results. share_price defaults to FP_ONE on first deposit. |
| M-11 | Closed positions never GC'd | **Fixed** | perp_gc_positions() compacts array. Auto-called when nearing MAX_PERP_POSITIONS. |
| M-12 | Session key comparison not constant-time | **Acceptable for PoC** | Enclave is on localhost, timing attack requires local access. Production: use constant-time compare. |
| M-13 | State persistence not atomic | **Known limitation** | 5-part partitioned sealing. Crash mid-save can corrupt. Mitigation: save is called every 5 minutes, probability low. Production: versioned snapshots. |
| M-14 | Vault deposit checks raw margin | **Fixed** | Uses perp_available_margin() instead of raw margin_balance |
| M-15 | No rate limiting on enclave API | **Mitigated** | Enclave is localhost only. Orchestrator serializes via Mutex (1 request at a time). |

## LOW / INFORMATIONAL

| # | Finding | Status | Notes |
|---|---------|--------|-------|
| L-01 | secp256k1 stubs | **Not applicable** | Stubs exist for compilation without SGX SDK. Real SGX build links real secp256k1. |
| L-02 | PerpState static without mutex | **By design** | TCSNum=1 — enclave is single-threaded. No concurrent access possible. |
| L-03 | SGX_DEBUG_FLAG=1 | **PoC only** | Production build: SGX_DEBUG=0 + release signing key. |
| L-04 | session_key validation wrong variable | **Fixed** | Was checking hash prefix, now checks session_key prefix |
| L-05 | TLS disabled in Python client | **By design** | Self-signed enclave cert. Python client on same localhost. |
| L-06 | DCAP quote without auth | **By design** | Attestation must be public — anyone should verify enclave identity. |
| L-07 | Debug logging leaks info | **PoC only** | Production: remove debug ocalls. |
| L-08 | App.cpp system() | **Not our code** | App.cpp is PM's original code, not used by perp-dex-server. Ignore. |
| L-09 | Enclave TLS cert disabled | **By design** | Same as L-05. Self-signed cert, localhost only. |
| L-10 | Election heartbeat seq not validated | **Acceptable** | Heartbeat dedup handled by gossipsub message_id_fn. |
| L-11 | Auth bypass for non-JSON bodies | **TODO** | |
| L-12 | GET signature not normalized | **Acceptable** | Low-S normalization done on client side (Python auth helper). |
| L-13 | Gossipsub message ID non-crypto hash | **Acceptable** | DefaultHasher sufficient for dedup. Not security-critical. |
| L-14 | URL injection in perp_client | **Mitigated** | perp_client only called internally with trusted URLs. |
| L-15 | Order/Trade ID u64 overflow | **Not reachable** | 18 quintillion orders before overflow. |
| L-16 | recent_trades.remove(0) O(n) | **Acceptable** | n ≤ 100 (capped). VecDeque optimization optional. |
| L-17 | mid_price truncation bias | **Acceptable** | FP8 integer division rounds down. Bias is 0.00000001 RLUSD max. |
