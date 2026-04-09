# Deployment Guide

## Two roles — read the one that applies to you

| Role | Who | What they do | What they DON'T do |
|------|-----|-------------|-------------------|
| **Operator** | Runs the server (SGX hardware) | Starts enclave + orchestrator + nginx | — |
| **Developer / SDK / Frontend** | Builds apps on top | Calls `https://api-perp.ph18.io/*` | **Never runs enclave. Never runs orchestrator. Never connects to port 9088 or 3000.** |

**If you are a developer building an SDK or frontend:**
- You only talk to `https://api-perp.ph18.io` (or the operator's public URL)
- The enclave does not exist from your perspective
- See [docs/frontend-api-guide.md](docs/frontend-api-guide.md) for the API reference
- **Stop reading here** — the rest of this document is for operators

---

## Architecture Overview (for operators only)

```
Internet
   │
   ▼
nginx :443 (api-perp.ph18.io)      ← only entry point for users
   │
   ▼
Orchestrator :3000 (localhost)      ← trading engine, auth, orderbook
   │
   ▼
SGX Enclave :9088 (localhost)       ← private keys, margin engine, signing
```

**Everything behind nginx is localhost-only. Users never touch the Orchestrator or Enclave directly.**

---

## What is exposed externally (via nginx)

These are the **Orchestrator** endpoints that users/frontends call.
The Orchestrator handles auth, orderbook matching, and proxies state queries to the Enclave internally.

### Trading (require XRPL signature auth)

| Method | Endpoint | Description |
|--------|----------|-------------|
| POST | `/v1/orders` | Submit order (limit/market) |
| GET | `/v1/orders?user_id=rXXX` | Get user's open orders |
| DELETE | `/v1/orders?user_id=rXXX` | Cancel all user's orders |
| DELETE | `/v1/orders/{order_id}` | Cancel specific order |
| GET | `/v1/account/balance?user_id=rXXX` | Balance, positions, unrealized PnL |
| POST | `/v1/withdraw` | Withdraw RLUSD to XRPL address (margin check + SGX signing) |

### Market data (no auth)

| Method | Endpoint | Description |
|--------|----------|-------------|
| GET | `/v1/markets` | List available markets (name, mark price, fees, leverage) |
| GET | `/v1/markets/{market}/orderbook?levels=20` | Order book depth (bids/asks) |
| GET | `/v1/markets/{market}/ticker` | Best bid/ask/mid price |
| GET | `/v1/markets/{market}/trades` | Last 100 trades |
| GET | `/v1/markets/{market}/funding` | Current funding rate, mark price, next funding time |
| WS | `/ws` | Real-time events (public: trade, orderbook, ticker, liquidation; per-user: fill, order_update, position_changed). Channel subscriptions via JSON control frames — see `docs/frontend-api-guide.md` |

### System (no auth)

| Method | Endpoint | Description |
|--------|----------|-------------|
| GET | `/v1/openapi.json` | OpenAPI 3.0 specification |
| POST | `/v1/attestation/quote` | DCAP remote attestation (SGX Quote v3) |
| GET | `/v1/attestation/commitment` | Sepolia on-chain state proof info |

### NOT exposed (internal Enclave endpoints, blocked by nginx)

These endpoints exist on the Enclave (:9088) but are **never accessible from outside**.
Only the Orchestrator calls them on localhost:

| Endpoint | Called by | Purpose |
|----------|-----------|---------|
| `/v1/perp/deposit` | Orchestrator (XRPL deposit monitor) | Credit user balance |
| `/v1/perp/price` | Orchestrator (Binance price feed) | Update mark/index price |
| `/v1/perp/position/open` | Orchestrator (after orderbook match) | Open position with margin check |
| `/v1/perp/position/close` | Orchestrator | Close position, realize PnL |
| `/v1/perp/withdraw` | Orchestrator | Margin check + SGX signing |
| `/v1/perp/liquidate` | Orchestrator (liquidation scanner) | Force-close position |
| `/v1/perp/funding/apply` | Orchestrator (every 8 hours) | Apply funding rate |
| `/v1/perp/state/save` | Orchestrator (every 5 min) | Seal state to disk |
| `/v1/perp/state/load` | Orchestrator (on startup) | Unseal state |

**Users submit orders via `POST /v1/orders` → Orchestrator matches on the orderbook →
Orchestrator calls Enclave `/v1/perp/position/open` for each fill.**
Users never call Enclave endpoints directly.

**Everything not listed above returns 403.** nginx uses a whitelist — only explicitly
listed endpoints are proxied.

---

## What is NOT exposed and why

| Service | Port | Accessible from | Why it must be closed |
|---------|------|-----------------|----------------------|
| **SGX Enclave** | 9088 | localhost only | Holds private keys. Direct access allows calling deposit/withdraw without any checks. |
| **Orchestrator** | 3000 | localhost only | Bypasses nginx whitelist and rate limiting. Must only be reached via nginx. |

**iptables rules:**
```bash
# Enclave — localhost only
iptables -A INPUT -p tcp --dport 9088 -s 127.0.0.1 -j ACCEPT
iptables -A INPUT -p tcp --dport 9088 -j DROP

# Orchestrator — localhost only (nginx proxies to it)
iptables -A INPUT -p tcp --dport 3000 -s 127.0.0.1 -j ACCEPT
iptables -A INPUT -p tcp --dport 3000 -j DROP

# nginx — public
iptables -A INPUT -p tcp --dport 443 -j ACCEPT
```

---

## What the Orchestrator does internally

The Orchestrator runs background tasks that call the Enclave on localhost.
No external service calls these endpoints — the Orchestrator does it automatically:

| Task | Interval | Enclave call | Description |
|------|----------|--------------|-------------|
| Price feed | 5 sec | `POST /v1/perp/price` | Fetches XRP/USDT from Binance, pushes to enclave |
| Deposit monitor | 1 sec | `POST /v1/perp/deposit` | Watches XRPL ledger for payments to escrow |
| Liquidation scan | 10 sec | `GET /v1/perp/liquidations/check` + `POST /v1/perp/liquidate` | Finds and closes undercollateralized positions |
| Funding rate | 8 hours | `POST /v1/perp/funding/apply` | Applies funding to all open positions |
| State save | 5 min | `POST /v1/perp/state/save` | Seals state to disk for crash recovery |

**These are internal calls (localhost:9088). They are never exposed to users.**

---

## How to start

### Prerequisites

- Server with Intel SGX (Hetzner bare metal or Azure DCsv3)
- SGX enclave binary (`enclave.signed.so`) built and deployed
- nginx installed
- Rust toolchain (for building orchestrator)

### Step 1: Start the Enclave

```bash
cd /path/to/EthSignerEnclave
./app -p 9088 -b 127.0.0.1
```

The enclave listens on `127.0.0.1:9088` — **not** on `0.0.0.0`.

### Step 2: Start the Orchestrator

```bash
cd /path/to/orchestrator
cargo build --release

./target/release/perp-dex-orchestrator \
  --enclave-url https://localhost:9088/v1 \
  --api-listen 127.0.0.1:3000 \
  --xrpl-url https://s.altnet.rippletest.net:51234 \
  --priority 0
```

Key flags:
| Flag | Value | Description |
|------|-------|-------------|
| `--enclave-url` | `https://localhost:9088/v1` | **Must point to the enclave, NOT the orchestrator** |
| `--api-listen` | `127.0.0.1:3000` | Bind to localhost only (nginx proxies to it) |
| `--priority` | `0` | Sequencer election priority (0=leader, 1-2=validators) |
| `--escrow-address` | `rXXX...` | XRPL escrow account to monitor for deposits |

### Step 3: Configure nginx

```bash
sudo cp nginx/api-perp.ph18.io /etc/nginx/sites-available/
sudo ln -s /etc/nginx/sites-available/api-perp.ph18.io /etc/nginx/sites-enabled/
sudo certbot --nginx -d api-perp.ph18.io
sudo nginx -t && sudo nginx -s reload
```

### Step 4: Verify

```bash
# From outside — should work
curl https://api-perp.ph18.io/v1/pool/status

# From outside — should return 403 (internal endpoint blocked by nginx)
curl https://api-perp.ph18.io/v1/perp/deposit

# Enclave port should be unreachable from outside
curl https://your-server:9088/v1/pool/status   # should timeout/refuse
```

---

## Common mistakes

### "I'm a developer and I'm trying to run the orchestrator"

**Don't.** The orchestrator is run by the operator on the SGX server.
You are a developer — you call the public API. That's it.

```
# This is ALL you need:
curl https://api-perp.ph18.io/v1/pool/status
```

You do not need `--enclave-url`, you do not need port 9088,
you do not need to compile anything from the enclave repo.

### "401 Unauthorized on price update"

```
ERROR perp_dex_orchestrator: price update failed: HTTP status client error
(401 Unauthorized) for url (http://94.130.18.162:3000/perp/price)
```

**Cause:** `--enclave-url` points to the Orchestrator (port 3000) instead of the Enclave (port 9088). The Orchestrator calls itself, hits its own auth middleware, and gets rejected.

**Fix (for operators only):**
```bash
# Wrong:
--enclave-url http://94.130.18.162:3000

# Correct:
--enclave-url https://localhost:9088/v1
```

### "Connection refused on port 9088 from remote machine"

**This is correct behavior.** The enclave only listens on `127.0.0.1`. It is not accessible from outside. Only the Orchestrator on the same machine can reach it.

### "Why can't I call /v1/perp/deposit from my frontend?"

Because `/v1/perp/deposit` is an internal endpoint. Only the Orchestrator calls it when it detects an XRPL deposit on the escrow account. Users deposit by sending an XRPL Payment to the escrow address — the Orchestrator detects it automatically and credits the balance in the enclave.

---

## Port summary

| Port | Service | Bind address | Accessible from |
|------|---------|-------------|-----------------|
| 443 | nginx | 0.0.0.0 | Internet (users) |
| 3000 | Orchestrator | 127.0.0.1 | localhost only (nginx) |
| 9088 | SGX Enclave | 127.0.0.1 | localhost only (orchestrator) |
| 4001 | P2P gossipsub | 0.0.0.0 | Other operators only |
| 8085-8087 | Phoenix PM | 127.0.0.1 | localhost only (do not touch) |
