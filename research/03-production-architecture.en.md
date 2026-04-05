# Production Architecture

**Date:** 2026-03-30
**Status:** Design

---

## Overview

```
┌──────────────────────────────────────────────────────────────┐
│                        Internet                               │
│                                                               │
│   User/Trader ─── HTTPS ───► nginx :443                      │
│                          (api-perp.ph18.io)                   │
│                               │                               │
│                               ▼                               │
│                     ┌──────────────────┐                      │
│                     │  Orchestrator    │                      │
│                     │  (Rust :3000)    │                      │
│                     │  Order book      │                      │
│                     │  Auth (XRPL sig) │                      │
│                     │  Mutex → enclave │                      │
│                     └────────┬─────────┘                      │
│                              │ HTTPS (localhost)              │
│                     ┌────────┼────────┐                      │
│                     ▼        ▼        ▼                      │
│                  :9088    :9089    :9090                      │
│                ┌────────────────────────┐                    │
│                │  SGX Enclave Instances  │                    │
│                │  (perp-dex-server)      │                    │
│                │  TCSNum=1, single-threaded │                 │
│                │  XRPL multisig 2-of-3   │                    │
│                └────────────────────────┘                    │
│                              │                                │
│   Orchestrator also:                                          │
│       ├──► XRPL Mainnet (deposit monitor)                    │
│       ├──► Binance/CEX (price feed)                          │
│       └──► P2P gossipsub (order replication)                 │
└──────────────────────────────────────────────────────────────┘
```

**Architecture:** nginx → Orchestrator → Enclave.
- **nginx** terminates TLS, proxies to Orchestrator (:3000)
- **Orchestrator** (Rust, multi-threaded) manages concurrency — serializes requests to enclave via Mutex
- **Enclave** (TCSNum=1, single-threaded) — receives one request at a time from Orchestrator

Enclave instances are not directly accessible from the internet (localhost only).

---

## MVP vs Production: why 3 enclave instances

### MVP (current state)

```
nginx :443 → Orchestrator :3000 → Enclave :9088 (single instance)
```

A single enclave instance is sufficient to demonstrate the full logic:
deposit → trade → liquidation → funding → withdraw. The escrow account on XRPL
is controlled by one key (regular signature).

### Production (target state)

```
nginx :443 → Orchestrator :3000 → Enclave :9088 ─┐
                                 → Enclave :9089 ─┤ SignerListSet 2-of-3
                                 → Enclave :9090 ─┘
```

3 instances are needed **not for performance** (each is single-threaded anyway),
but for **withdrawal security**:

1. **XRPL native multisig (SignerListSet quorum=2).** The escrow account is
   configured so that sending a Payment (withdrawal) requires signatures from
   at least 2 of 3 instances. Master key is disabled — single point of failure
   is eliminated.

2. **Protection against single-instance compromise.** Even if an attacker gains
   access to one enclave's memory (side-channel attack, SGX vulnerability), they
   obtain only 1 of 3 keys — insufficient to sign a withdrawal transaction.

3. **Fault tolerance.** If one instance crashes or its sealed state is corrupted,
   the remaining 2 continue to serve withdrawals (2-of-3).

4. **Independent verification.** Each instance independently checks margin
   before signing. Orchestrator collects 2 signatures and assembles the XRPL
   Signers array for ledger submission.

> **Note:** All 3 instances run identical `enclave.signed.so` (same MRENCLAVE),
> which is verified via DCAP remote attestation. The only difference is the
> generated ECDSA keys.

---

## API Separation: Public vs Internal

### Public API (via nginx, accessible to users)

| Method | Endpoint | Description |
|--------|----------|-------------|
| GET | `/v1/perp/balance` | User balance and positions |
| POST | `/v1/perp/position/open` | Open position |
| POST | `/v1/perp/position/close` | Close position |
| POST | `/v1/perp/withdraw` | Withdraw funds (margin check + SGX signing) |
| GET | `/v1/perp/liquidations/check` | View liquidatable positions |
| GET | `/v1/pool/status` | Enclave status |
| POST | `/v1/pool/report` | Attestation report (legacy) |
| POST | `/v1/attestation/quote` | DCAP remote attestation (SGX Quote v3, Azure DCsv3 only) |

### Internal API (localhost only, not exposed externally)

| Method | Endpoint | Description | Called by |
|--------|----------|-------------|----------|
| POST | `/v1/perp/deposit` | Credit deposit | Orchestrator |
| POST | `/v1/perp/price` | Price update | Orchestrator |
| POST | `/v1/perp/liquidate` | Execute liquidation | Orchestrator |
| POST | `/v1/perp/funding/apply` | Apply funding rate | Orchestrator |
| POST | `/v1/perp/state/save` | Save state | Orchestrator |
| POST | `/v1/perp/state/load` | Load state | Orchestrator |
| POST | `/v1/pool/generate` | Key generation | Admin |
| POST | `/v1/pool/sign` | Direct signing | Admin |
| POST | `/v1/pool/frost/*` | FROST operations (Bitcoin Taproot, not XRPL) | Admin |
| POST | `/v1/pool/dkg/*` | DKG operations (Bitcoin Taproot, not XRPL) | Admin |

---

## nginx Configuration

```nginx
# /etc/nginx/sites-available/api-perp.ph18.io

server {
    listen 443 ssl http2;
    server_name api-perp.ph18.io;

    ssl_certificate     /etc/letsencrypt/live/api-perp.ph18.io/fullchain.pem;
    ssl_certificate_key /etc/letsencrypt/live/api-perp.ph18.io/privkey.pem;

    # === Public API → Orchestrator (:3000) ===
    # Orchestrator handles auth, orderbook, concurrency (Mutex → enclave)

    location /v1/perp/balance     { proxy_pass http://127.0.0.1:3000; }
    location /v1/perp/position/   { proxy_pass http://127.0.0.1:3000; }
    location /v1/perp/withdraw    { proxy_pass http://127.0.0.1:3000; }
    location /v1/perp/liquidations/check { proxy_pass http://127.0.0.1:3000; }
    location /v1/pool/status      { proxy_pass http://127.0.0.1:3000; }
    location /v1/pool/report      { proxy_pass http://127.0.0.1:3000; }
    location /v1/attestation/     { proxy_pass http://127.0.0.1:3000; }

    # WebSocket (orderbook, trades, liquidations)
    location /ws {
        proxy_pass http://127.0.0.1:3000;
        proxy_http_version 1.1;
        proxy_set_header Upgrade $http_upgrade;
        proxy_set_header Connection "upgrade";
        proxy_read_timeout 86400;
    }

    # Block everything else — internal endpoints not exposed
    location / {
        return 403;
    }

    # Standard proxy headers
    proxy_set_header Host $host;
    proxy_set_header X-Real-IP $remote_addr;
    proxy_set_header X-Forwarded-For $proxy_add_x_forwarded_for;
    proxy_set_header X-Forwarded-Proto $scheme;

    # Rate limiting
    limit_req zone=perp_api burst=20 nodelay;
}

# Rate limit zone (in http block)
# limit_req_zone $binary_remote_addr zone=perp_api:10m rate=10r/s;
```

**Concurrency:** Orchestrator uses `tokio::sync::Mutex` to serialize
requests to each enclave instance. This guarantees that a single-threaded
enclave (TCSNum=1) does not receive parallel ecalls. nginx only proxies
to Orchestrator — direct access to enclave is impossible.

---

## Components

### 1. nginx (reverse proxy)

- Terminates TLS for users (Let's Encrypt)
- Proxies only public endpoints to Orchestrator (:3000)
- Blocks everything else (return 403)
- WebSocket support for real-time data
- Rate limiting on user endpoints

### 2. SGX Enclave Instances (perp-dex-server)

- 3 instances on ports 9088-9090
- Each with identical `enclave.signed.so` (same MRENCLAVE)
- TCSNum=1 (single-threaded per instance)
- XRPL native multisig (SignerListSet): each instance holds its own independent ECDSA key
- State sealed to disk (per-instance)
- Listen on 127.0.0.1 (not directly accessible from outside)

### 3. Orchestrator (Rust binary)

- Single process, listens on :3000 (localhost, behind nginx)
- Connects **directly** to enclave instances (localhost:9088-9090)
- Serializes requests via `tokio::sync::Mutex` (one request at a time per instance)
- XRPL signature auth for user requests
- CLOB orderbook with price-time priority
- libp2p gossipsub for order flow replication between operators
- Functions:
  - **Price feed**: Binance API -> enclave price update (every 5 sec)
  - **Deposit monitor**: XRPL ledger -> enclave deposit credit
  - **Liquidation**: enclave check -> enclave liquidate (every 10 sec)
  - **Funding rate**: computation + application (every 8 hours)
  - **State save**: periodic persistence (every 5 minutes)

### 4. XRPL Mainnet

- Escrow account controlled by SGX (3 independent ECDSA keys, SignerListSet quorum=2, master key disabled)
- RLUSD collateral on escrow
- Deposits: user -> Payment -> escrow -> Orchestrator detects -> enclave credits
- Withdrawals: user requests -> enclave checks margin -> orchestrator collects 2 ECDSA signatures from 2 instances -> assembles Signers array -> submits to XRPL

---

## Network Rules

```
# Enclave instances — localhost only
iptables -A INPUT -p tcp --dport 9088:9099 -s 127.0.0.1 -j ACCEPT
iptables -A INPUT -p tcp --dport 9088:9099 -j DROP

# nginx — public
iptables -A INPUT -p tcp --dport 443 -j ACCEPT

# Orchestrator — listens :3000 localhost only
iptables -A INPUT -p tcp --dport 3000 -s 127.0.0.1 -j ACCEPT
iptables -A INPUT -p tcp --dport 3000 -j DROP
# Outbound: localhost:9088-9090, XRPL (51234), Binance (443)
```

---

## Ports

| Port | Service | Access |
|------|---------|--------|
| 443 | nginx (public API) | Internet |
| 3000 | Orchestrator | localhost only |
| 9088 | Enclave instance 1 | localhost only |
| 9089 | Enclave instance 2 | localhost only |
| 9090 | Enclave instance 3 | localhost only |
| 8085-8087 | Phoenix PM (do not touch) | localhost only |
