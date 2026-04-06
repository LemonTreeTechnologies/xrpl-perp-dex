# Answers to Outstanding Questions

Responses to questions raised in `docs/oustanding_questions.md`.

---

## 1. How to actually run the orchestrator?

See [DEPLOYMENT.md](../DEPLOYMENT.md).

**If you are a developer/SDK builder: you don't run it.** You call the public API at `https://api-perp.ph18.io`. The operator runs the orchestrator on the SGX server.

**If you are an operator:**
```bash
cd orchestrator
cargo build --release
./target/release/perp-dex-orchestrator \
  --enclave-url https://localhost:9088/v1 \
  --api-listen 127.0.0.1:3000 \
  --priority 0
```

There is no `make run` — it's a standard `cargo build && ./binary`.

---

## 2. How is the price calculated?

The price comes from **Binance API** (XRP/USDT spot), NOT from the enclave.

Flow:
```
Binance API → Orchestrator (fetches every 5 sec) → POST /v1/perp/price → Enclave stores it
```

The enclave does not fetch prices — it receives them from the orchestrator. In production with multiple operators, each operator fetches independently and the sequencer pushes the median. See `src/price_feed.rs`.

This is the oracle. It is implemented and working.

---

## 3. FP8 custom type vs rust_decimal

FP8 (int64 with 8 decimal places) is an **intentional design choice**, not a limitation:

- **The enclave uses FP8.** The C code inside SGX uses `int64_t` with `__int128` intermediate multiply. See `Enclave/PerpState.h`. Every price, size, and balance in the enclave is FP8.
- **Exact match = no conversion bugs.** If the orchestrator used `rust_decimal` and the enclave used FP8, we'd need conversion at every boundary, introducing rounding discrepancies.
- **Deterministic.** All operators must compute identical results. FP8 integer arithmetic is deterministic across platforms. `rust_decimal` uses different internal representations.
- **Performance is not a concern here.** The bottleneck is XRPL ledger close time (3-4 seconds), not arithmetic.

If we ever change the enclave's internal format, we change the orchestrator's format to match. They must always agree.

---

## 4. What is the purpose of the orchestrator?

The orchestrator is **NOT just an API gateway**. It does 5 things the enclave cannot:

| Function | Why the enclave can't do it |
|----------|---------------------------|
| **Order book (CLOB)** | Enclave is single-threaded (TCSNum=1), ~5ms per ecall. Matching must happen outside. |
| **Network I/O** | SGX enclaves cannot make outbound HTTP calls (no syscalls). |
| **XRPL monitoring** | Watching the ledger for deposits requires network access. |
| **Price feed** | Fetching from Binance requires HTTP. |
| **P2P replication** | libp2p gossipsub for multi-operator coordination. |

**The enclave only does:**
- Store balances and positions
- Margin checks (can this user open this position?)
- Sign XRPL transactions (withdrawal)
- Seal/unseal state

---

## 5. Where is the book held? How is it synchronized?

**The order book is in the Orchestrator.** The enclave has no order book.

```
User submits order
       │
       ▼
Orchestrator: orderbook matches (price-time priority)
       │
       ├── Fill found: Taker buys 100 XRP @ 0.55 from Maker
       │
       ▼
Orchestrator calls Enclave TWICE:
  1. POST /v1/perp/position/open (taker: long 100 @ 0.55, leverage 5)
  2. POST /v1/perp/position/open (maker: short 100 @ 0.55, leverage 5)
       │
       ▼
Enclave: checks margin for each user, opens positions if sufficient
       │
       ▼
Orchestrator: if enclave returns "success", trade is confirmed
              if enclave returns "insufficient margin", trade is rolled back
```

The enclave does NOT know about orders, bids, asks, or the book. It only knows about positions and balances. The orchestrator tells it "open a position for user X" after matching.

**Synchronization:** there is none. The orchestrator is the source of truth for the order book. The enclave is the source of truth for positions and balances. They are different things.

---

## 6. Ordering in distributed system

**One sequencer at a time.** This is how ordering is guaranteed:

```
User A → Orchestrator 1 (sequencer) ─── processes order A first
User B → Orchestrator 1 (sequencer) ─── processes order B second
```

Users do NOT send orders to different orchestrators. All orders go to the **sequencer** (current leader). Validators receive the ordered batches via P2P gossipsub and replay deterministically.

**What if 2 orders arrive at 2 different operators?**
They don't. The API returns `503 Service Unavailable` on validators:

```rust
if !state.is_sequencer.load(Ordering::Relaxed) {
    return err(StatusCode::SERVICE_UNAVAILABLE, "this node is not the sequencer");
}
```

Users must send orders to the sequencer. If the sequencer fails, a new one is elected (heartbeat timeout → priority-based failover). See `src/election.rs`.

**This is NOT a consensus protocol.** It is a leader-based system. One leader, deterministic replay. Similar to how Hyperliquid works.

---

## 7. Missing endpoints

### positions/get

Already exists: `GET /v1/account/balance?user_id=rXXX` returns positions.

```json
{
  "positions": [
    {"position_id": 0, "side": "long", "size": "100.00000000", "entry_price": "0.55000000", ...}
  ]
}
```

This is proxied to the enclave's `/v1/perp/balance` endpoint.

### funding/rates/get

Not yet implemented as a separate endpoint. Currently the funding rate is computed inside the orchestrator (`compute_funding_rate()` in `main.rs`) and applied every 8 hours. Can be exposed as `GET /v1/markets/{market}/funding` — easy to add.

### funding/payments/get

Not tracked per-user currently. The enclave applies funding to all open positions atomically. Individual payment history would require adding a transaction log to the enclave state. This is a valid feature request for post-MVP.

### markets/get

**Added:** `GET /v1/markets` — returns market details (name, mark price, fees, leverage, status).

---

## 8. OpenAPI spec drift

Valid concern. The OpenAPI spec in `/v1/openapi.json` is hand-written in `src/api.rs`. It could drift from actual endpoints.

Options:
1. **Generate from code** — use `utoipa` crate to derive OpenAPI from axum handlers
2. **Generate code from spec** — use spec in `build.rs` to generate models

For the current PoC, hand-written spec is acceptable. For production, we should switch to `utoipa` (option 1) since the Rust handlers are the source of truth, not the spec.

---

## 9. DEPLOYMENT.md endpoints mismatch (follow-up)

**Fixed.** The original DEPLOYMENT.md mixed Enclave endpoints (`/v1/perp/position/open`)
with Orchestrator endpoints (`/v1/orders`). This was confusing.

DEPLOYMENT.md now has three clear tables:

1. **Trading endpoints** (require auth): `/v1/orders`, `/v1/account/balance`
2. **Market data** (no auth): `/v1/markets/*`, `/ws`
3. **Internal Enclave endpoints** (NOT exposed): `/v1/perp/*` — called by Orchestrator only

The key clarification: **users submit orders via `POST /v1/orders`**, not via
`/v1/perp/position/open`. The Orchestrator matches on the orderbook first, then
calls the Enclave internally for each fill. Users never see or call Enclave endpoints.

---

## 10. Dead code

Valid. The warnings fall into 3 categories:

| Category | Files | Action |
|----------|-------|--------|
| **Future use (keep)** | `perp_client.rs` (deposit_xrp, withdraw, close_position), `types.rs` (FP8::ONE, to_f64, abs) | These will be used when full trading flow is wired. Mark with `#[allow(dead_code)]`. |
| **Enclave client (remove)** | `enclave_client.rs` (entire file) | Legacy — replaced by `perp_client.rs`. Safe to delete. |
| **XRPL signer (keep)** | `xrpl_signer.rs` | Used by multisig withdrawal flow (not yet wired in orchestrator, works in Python). Keep for now. |
| **Commitment (keep)** | `commitment.rs` | Sepolia on-chain proofs. Functions called from background tasks (not yet wired). Keep. |

We can clean up `enclave_client.rs` and add `#[allow(dead_code)]` annotations to intentionally unused-for-now code.
