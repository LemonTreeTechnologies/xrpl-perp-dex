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
│                              ▼                                │
│                     ┌──────────────────┐                      │
│                     │  SGX Enclave     │                      │
│                     │  :9088           │                      │
│                     │  TCSNum=1        │                      │
│                     │  ECDSA key       │                      │
│                     └──────────────────┘                      │
│                              │                                │
│   Orchestrator also:                                          │
│       ├──► XRPL Mainnet (deposit monitor)                    │
│       ├──► Binance/CEX (price feed)                          │
│       └──► P2P gossipsub (order replication)                 │
└──────────────────────────────────────────────────────────────┘
```

**Single-operator architecture:** nginx → Orchestrator → Enclave.
- **nginx** terminates TLS, proxies to Orchestrator (:3000)
- **Orchestrator** (Rust, multi-threaded) manages concurrency — serializes requests to enclave via Mutex
- **Enclave** (TCSNum=1, single-threaded) — receives one request at a time from Orchestrator

This document describes the architecture of **a single operator (single server)**.
Multi-operator coordination (3 operators, XRPL multisig 2-of-3, P2P) is described
in [04-multi-operator-architecture](04-multi-operator-architecture.en.md).

---

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
    server_name api-perp.ph18.io;

    # === Whitelist (restricted during testing, removed for public launch) ===
    # include /etc/nginx/perp_whitelist.conf;

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

    # CORS preflight
    if ($request_method = 'OPTIONS') {
        add_header 'Access-Control-Allow-Origin' '*';
        add_header 'Access-Control-Allow-Methods' 'GET, POST, OPTIONS';
        add_header 'Access-Control-Allow-Headers' '*';
        add_header 'Content-Length' 0;
        add_header 'Content-Type' 'text/plain';
        return 204;
    }

    listen 443 ssl; # managed by Certbot
    ssl_certificate /etc/letsencrypt/live/api-perp.ph18.io/fullchain.pem; # managed by Certbot
    ssl_certificate_key /etc/letsencrypt/live/api-perp.ph18.io/privkey.pem; # managed by Certbot
    include /etc/letsencrypt/options-ssl-nginx.conf; # managed by Certbot
    ssl_dhparam /etc/letsencrypt/ssl-dhparams.pem; # managed by Certbot
}

server {
    if ($host = api-perp.ph18.io) {
        return 301 https://$host$request_uri;
    } # managed by Certbot

    listen 80;
    server_name api-perp.ph18.io;
    return 404; # managed by Certbot
}
```

Optional — IP whitelist via `/etc/nginx/perp_whitelist.conf` (same pattern as PM API).
During testing access is restricted; after launch the whitelist is removed for public access.

**Whitelist approach:** nginx proxies **only explicitly listed** public endpoints.
Everything else (including internal endpoints like `/v1/perp/deposit`, `/v1/perp/price`,
`/v1/pool/generate`, etc.) falls through to `location /` → `return 403`. This is safer
than a blacklist: if a new endpoint is added to the enclave, it **will not be accessible**
from outside until explicitly added to the nginx configuration.

**Concurrency:** Orchestrator uses `tokio::sync::Mutex` to serialize
requests to the enclave. This guarantees that a single-threaded
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

### 2. SGX Enclave (perp-dex-server)

- Single instance on port 9088
- TCSNum=1 (single-threaded)
- ECDSA key generated inside enclave (non-extractable)
- State sealed to disk (partitioned, 5 parts <64KB each)
- Listens on 127.0.0.1 (not directly accessible from outside)
- DCAP remote attestation (Azure DCsv3)

### 3. Orchestrator (Rust binary)

- Single process, listens on :3000 (localhost, behind nginx)
- Connects **directly** to enclave (localhost:9088)
- Serializes requests via `tokio::sync::Mutex` (one request at a time)
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

- Escrow account controlled by SGX ECDSA key
- RLUSD collateral on escrow
- Deposits: user -> Payment -> escrow -> Orchestrator detects -> enclave credits
- Withdrawals: enclave checks margin -> signs -> Orchestrator submits to XRPL
- Multi-operator (multisig 2-of-3) — see [04-multi-operator-architecture](04-multi-operator-architecture.en.md)

---

## Network Rules

```
# Enclave — localhost only
iptables -A INPUT -p tcp --dport 9088 -s 127.0.0.1 -j ACCEPT
iptables -A INPUT -p tcp --dport 9088 -j DROP

# nginx — public
iptables -A INPUT -p tcp --dport 443 -j ACCEPT

# Orchestrator — listens :3000 localhost only
iptables -A INPUT -p tcp --dport 3000 -s 127.0.0.1 -j ACCEPT
iptables -A INPUT -p tcp --dport 3000 -j DROP
# Outbound: localhost:9088, XRPL (51234), Binance (443)
```

---

## Ports

| Port | Service | Access |
|------|---------|--------|
| 443 | nginx (public API) | Internet |
| 3000 | Orchestrator | localhost only |
| 9088 | SGX Enclave | localhost only |
| 8085-8087 | Phoenix PM (do not touch) | localhost only |
