# TEE Rationale & API Design

**Date:** 2026-03-30
**Status:** Design

---

## 1. Why TEE? The Problem We Are Solving

XRPL mainnet does not have smart contracts. This is a deliberate design choice — XRPL is optimized for fast payments, not for programmable logic. But perpetual futures require:

- Margin engine (collateral verification before every trade)
- Liquidation engine (monitoring and forced closure of positions)
- Funding rate (periodic rebalancing between longs and shorts)
- Order matching (matching orders against each other)

**Problem:** How do we run all of this on XRPL if there are no smart contracts?

**Standard approach:** A centralized server. Users send funds to the operator and trust them completely. This is a CEX — exactly what DeFi is trying to move away from.

**Our approach:** TEE (Trusted Execution Environment) as a verifiable computation layer.

---

## 2. What TEE Specifically Provides

### 2.1 Custody Without Trusting the Operator

```
Without TEE:                      With TEE:
User → sends funds                User → sends to escrow
       to operator                       controlled by SGX
       (operator can steal)              (private key INSIDE enclave,
                                          operator has no access)
```

The private key of the escrow account on XRPL is **generated inside the SGX enclave** and never leaves it. The operator launches the enclave but physically cannot extract the key — this is a hardware-level guarantee (Intel SGX).

### 2.2 Verifiable execution

A user can verify that **exactly the code** that is published (open source) is running inside the enclave:

1. On startup, the enclave creates an **MRENCLAVE** — a cryptographic hash of the binary code
2. The user verifies MRENCLAVE via **remote attestation** (signed by Intel)
3. If MRENCLAVE matches the hash of the published code → the enclave is running exactly that code

**What this means for the perp DEX:**
- Margin check — cannot be bypassed (code is in the enclave, operator cannot modify it)
- Liquidation rules — transparent and immutable (part of the attested code)
- Withdrawals — signed only after a margin check (atomically inside the enclave)

### 2.3 Anti-MEV

```
Traditional DEX:              TEE DEX:
Order → mempool (visible to all)  Order → encrypted → TEE decrypts inside
      → frontrun possible               → matching inside enclave
      → sandwich possible               → operator sees only ciphertext
```

The operator sees only encrypted traffic. Orders are decrypted and matched **only inside the enclave**. Front-running, sandwich attacks, MEV extraction — impossible.

### 2.4 Trust Model Comparison

| Property | CEX | Smart Contract DEX | TEE DEX (ours) |
|----------|-----|-------------------|---------------|
| Custody | Operator | On-chain contract | SGX enclave |
| Verifiable logic | No | Yes (on-chain) | Yes (attestation) |
| Anti-MEV | No | Partially | Yes (encrypted orders) |
| Speed | ~1ms | 3-12 sec (block time) | ~1ms computation + 3-5 sec settlement |
| XRPL compatible | Yes (offchain) | **Impossible** (no contracts) | **Yes** |
| Key risk | Trust in operator | Smart contract bugs | Intel SGX side-channels |

**Key takeaway:** A smart contract DEX on XRPL is impossible. TEE is the only path to verifiable computation on XRPL without a sidechain.

### 2.5 XRPL + TEE = Native L1 Settlement

Unlike sidechain approaches (EVM sidechain, Xahau):
- Settlement happens **directly on XRPL mainnet** in RLUSD
- No bridge risk (no bridge between chains)
- No separate token or chain
- Deposit/withdrawal — standard XRPL Payment transactions

TEE adds a computation layer **on top of** XRPL, not alongside it.

---

## 3. API Design: Orders, Positions, Trades

### 3.1 Current State (PoC)

The PoC API is minimalistic, combining orders and positions in a single endpoint. For production, separation is needed.

### 3.2 Production API (Target Architecture)

Reference: [Thalex API](https://thalex.com/docs/thalex_api.yaml)

**Principle:** Orders are intentions. Positions are the result of filled orders. Trades are records of executions.

#### Orders API

| Method | Endpoint | Description |
|--------|----------|-------------|
| POST | `/v1/orders` | Create a new order (limit, market, stop) |
| DELETE | `/v1/orders/{order_id}` | Cancel an order |
| GET | `/v1/orders` | List of user's active orders |
| GET | `/v1/orders/{order_id}` | Status of a specific order |
| DELETE | `/v1/orders` | Cancel all orders (cancel all) |

**Order types:**
- `limit` — order at a specified price, rests on the book
- `market` — execute immediately at the best available price
- `stop_market` — activates when trigger price is reached
- `take_profit` — closes position when target price is reached

**Order request:**
```json
{
    "market": "XRP-RLUSD-PERP",
    "side": "buy",
    "type": "limit",
    "size": "1000.00000000",
    "price": "0.55000000",
    "leverage": 10,
    "reduce_only": false,
    "time_in_force": "GTC",
    "client_order_id": "user-defined-id-123"
}
```

#### Positions API

| Method | Endpoint | Description |
|--------|----------|-------------|
| GET | `/v1/positions` | All open positions for the user |
| GET | `/v1/positions/{market}` | Position for a specific market |

A position is the **net** of all filled orders for a market. If a user bought 100 XRP and sold 30 XRP → position = 70 XRP long.

**Position response:**
```json
{
    "market": "XRP-RLUSD-PERP",
    "side": "long",
    "size": "70.00000000",
    "entry_price": "0.54285714",
    "mark_price": "0.55000000",
    "liquidation_price": "0.51500000",
    "unrealized_pnl": "5.00000000",
    "margin": "38.00000000",
    "leverage": "10.00"
}
```

#### Trades API

| Method | Endpoint | Description |
|--------|----------|-------------|
| GET | `/v1/trades` | User's trade history |
| GET | `/v1/trades/recent` | Recent trades for a market (public) |

**Trade record:**
```json
{
    "trade_id": "T-00001234",
    "order_id": "O-00005678",
    "market": "XRP-RLUSD-PERP",
    "side": "buy",
    "size": "100.00000000",
    "price": "0.55000000",
    "fee": "0.02750000",
    "timestamp": "2026-03-30T12:00:00Z",
    "role": "taker"
}
```

#### Account API

| Method | Endpoint | Description |
|--------|----------|-------------|
| GET | `/v1/account/balance` | Balance (margin, available, equity) |
| GET | `/v1/account/deposits` | Deposit history |
| GET | `/v1/account/withdrawals` | Withdrawal history |
| POST | `/v1/account/withdraw` | Withdrawal request |

#### Market Data API (public, no authentication)

| Method | Endpoint | Description |
|--------|----------|-------------|
| GET | `/v1/markets` | List of available markets |
| GET | `/v1/markets/{market}/orderbook` | Order book |
| GET | `/v1/markets/{market}/ticker` | Current price, volume, funding |
| GET | `/v1/markets/{market}/trades` | Recent trades |
| GET | `/v1/markets/{market}/funding` | Funding rate history |

### 3.3 API Scaling

To support a large number of users:

**Read-heavy endpoints** (balance, positions, orderbook, trades):
- Cached at the HAProxy/nginx level
- Do not require an ecall for every request
- Orchestrator publishes snapshots → read replicas

**Write endpoints** (orders, withdraw):
- Go through HAProxy → enclave instance
- maxconn 1 per instance, queue for waiting
- Horizontal scaling: more enclave instances

**WebSocket** (real-time updates):
- Separate service outside the enclave
- Subscribes to the event stream from the orchestrator
- Streams: orderbook updates, trades, positions, funding

```
┌─────────┐     ┌──────────┐     ┌──────────────┐
│ Users   │────►│ HAProxy  │────►│ Enclave (write)│
│ (HTTP)  │     │          │     └──────────────┘
│         │     │          │     ┌──────────────┐
│ (WS)    │────►│          │────►│ WS Gateway   │◄── Orchestrator events
└─────────┘     └──────────┘     └──────────────┘
                      │          ┌──────────────┐
                      └─────────►│ Read Replica │◄── State snapshots
                                 └──────────────┘
```

### 3.4 Migration from PoC to Production API

| PoC Endpoint | Production Equivalent |
|---|---|
| `POST /v1/perp/position/open` | `POST /v1/orders` (order → fill → position) |
| `POST /v1/perp/position/close` | `POST /v1/orders` (reduce_only=true) |
| `GET /v1/perp/balance` | `GET /v1/account/balance` + `GET /v1/positions` |
| `POST /v1/perp/withdraw` | `POST /v1/account/withdraw` |
| `GET /v1/perp/liquidations/check` | Internal (orchestrator only) |
| `POST /v1/perp/price` | Internal (orchestrator only) |
