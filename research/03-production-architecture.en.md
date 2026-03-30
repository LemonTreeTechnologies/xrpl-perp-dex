# Production Architecture

**Date:** 2026-03-30
**Status:** Design

---

## Overview

```
┌─────────────────────────────────────────────────────────────┐
│                        Internet                              │
│                                                              │
│   User/Trader ──── HTTPS ────► HAProxy :443                 │
│                                   │                          │
│                          ┌────────┼────────┐                │
│                          ▼        ▼        ▼                │
│                       :9088    :9089    :9090                │
│                     ┌────────────────────────┐              │
│                     │  SGX Enclave Instances  │              │
│                     │  (perp-dex-server)      │              │
│                     │  FROST 2-of-3 signing   │              │
│                     └────────────────────────┘              │
│                          ▲                                   │
│                          │ localhost only                    │
│                     ┌────┴─────┐                            │
│                     │Orchestrator│                           │
│                     │  (Rust)    │                           │
│                     └────┬──┬───┘                           │
│                          │  │                                │
│                    ┌─────┘  └──────┐                        │
│                    ▼               ▼                         │
│              XRPL Mainnet    Binance/CEX                    │
│            (deposit monitor)  (price feed)                  │
└─────────────────────────────────────────────────────────────┘
```

---

## API Separation: Public vs Internal

### Public API (via HAProxy, accessible to users)

| Method | Endpoint | Description |
|--------|----------|-------------|
| GET | `/v1/perp/balance` | User balance and positions |
| POST | `/v1/perp/position/open` | Open position |
| POST | `/v1/perp/position/close` | Close position |
| POST | `/v1/perp/withdraw` | Withdraw funds (margin check + SGX signing) |
| GET | `/v1/perp/liquidations/check` | View liquidatable positions |
| GET | `/v1/pool/status` | Enclave status |
| POST | `/v1/pool/report` | Attestation report |

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
| POST | `/v1/pool/frost/*` | FROST operations | Admin |
| POST | `/v1/pool/dkg/*` | DKG operations | Admin |

---

## HAProxy Configuration

```haproxy
frontend perp-dex
    bind *:443 ssl crt /etc/ssl/perp.pem
    mode http

    # Block internal endpoints
    acl is_internal path_beg /v1/perp/deposit
    acl is_internal path_beg /v1/perp/price
    acl is_internal path_beg /v1/perp/liquidate
    acl is_internal path_beg /v1/perp/funding
    acl is_internal path_beg /v1/perp/state
    acl is_internal path_beg /v1/pool/generate
    acl is_internal path_beg /v1/pool/sign
    acl is_internal path_beg /v1/pool/frost
    acl is_internal path_beg /v1/pool/dkg
    acl is_internal path_beg /v1/pool/load
    acl is_internal path_beg /v1/pool/unload
    acl is_internal path_beg /v1/pool/schnorr
    acl is_internal path_beg /v1/pool/musig
    acl is_internal path_beg /v1/pool/regenerate
    acl is_internal path_beg /v1/pool/validate
    acl is_internal path_beg /v1/pool/recovery
    http-request deny if is_internal

    default_backend enclave_instances

backend enclave_instances
    mode http
    balance roundrobin
    option httpchk GET /v1/pool/status
    server enclave1 127.0.0.1:9088 check ssl verify none
    server enclave2 127.0.0.1:9089 check ssl verify none
    server enclave3 127.0.0.1:9090 check ssl verify none
```

---

## Components

### 1. HAProxy/nginx (reverse proxy)

- Terminates TLS for users
- Blocks internal endpoints
- Round-robin across enclave instances
- Health check via `/v1/pool/status`
- Rate limiting on user endpoints

### 2. SGX Enclave Instances (perp-dex-server)

- 3 instances on ports 9088-9090
- Each with identical `enclave.signed.so` (same MRENCLAVE)
- TCSNum=1 (single-threaded per instance)
- FROST 2-of-3: each instance holds its own share
- State sealed to disk (per-instance)
- Listen on 127.0.0.1 (not directly accessible from outside)

### 3. Orchestrator (Rust binary)

- Single process, runs on localhost
- Connects to enclave instance directly (127.0.0.1:9088)
- Functions:
  - **Price feed**: Binance API -> enclave price update (every 5 sec)
  - **Deposit monitor**: XRPL ledger -> enclave deposit credit
  - **Liquidation**: enclave check -> enclave liquidate (every 10 sec)
  - **Funding rate**: computation + application (every 8 hours)
  - **State save**: periodic persistence (every 5 minutes)

### 4. XRPL Mainnet

- Escrow account controlled by SGX (private key inside enclave)
- RLUSD collateral on escrow
- Deposits: user -> Payment -> escrow -> Orchestrator detects -> enclave credits
- Withdrawals: user requests -> enclave checks margin -> SGX signs Payment -> XRPL

---

## Network Rules

```
# Enclave instances — localhost only
iptables -A INPUT -p tcp --dport 9088:9099 -s 127.0.0.1 -j ACCEPT
iptables -A INPUT -p tcp --dport 9088:9099 -j DROP

# HAProxy — public
iptables -A INPUT -p tcp --dport 443 -j ACCEPT

# Orchestrator — no listening ports, outbound only:
#   -> localhost:9088 (enclave)
#   -> XRPL nodes (port 51234)
#   -> Binance API (port 443)
```

---

## Ports

| Port | Service | Access |
|------|---------|--------|
| 443 | HAProxy (public API) | Internet |
| 9088 | Enclave instance 1 | localhost only |
| 9089 | Enclave instance 2 | localhost only |
| 9090 | Enclave instance 3 | localhost only |
| 8085-8087 | Phoenix PM (do not touch) | localhost only |
