# xrpl-perp-dex

Perpetual futures DEX on XRPL mainnet with TEE (Intel SGX) computation layer and RLUSD settlement.

## Architecture

```
Users ──[HTTPS]──► Orchestrator (Rust)
                      ├── Order book (CLOB, price-time priority)
                      ├── Order matching → enclave margin check
                      ├── Price feed (Binance live)
                      ├── XRPL deposit monitoring
                      ├── P2P replication (libp2p gossipsub)
                      └── XRPL signature auth (secp256k1)
                              │
                    [HTTPS to SGX enclave]
                              │
                      SGX Enclave (C/C++)
                      ├── Margin engine (11 ecalls)
                      ├── ECDSA key custody
                      ├── State persistence (sealed storage)
                      ├── DCAP remote attestation
                      └── XRPL multisig signing
                              │
                    [XRPL native multisig 2-of-3]
                              │
                      XRPL Mainnet
                      ├── RLUSD collateral (SignerListSet escrow)
                      ├── P&L settlement
                      └── Deposit / withdrawal
```

## Project Structure

```
├── orchestrator/          # Rust binary — trading API, order book, P2P, price feed
│   └── src/
│       ├── main.rs        # CLI + orchestration loop
│       ├── api.rs         # HTTP API (axum) — orders, balance, attestation
│       ├── auth.rs        # XRPL secp256k1 signature authentication
│       ├── orderbook.rs   # CLOB with price-time priority matching
│       ├── trading.rs     # Wires fills to enclave open_position
│       ├── p2p.rs         # libp2p gossipsub for order flow replication
│       ├── price_feed.rs  # Binance XRP/USDT polling
│       ├── xrpl_monitor.rs # XRPL deposit detection
│       ├── xrpl_signer.rs # XRPL address derivation, DER encoding, SHA-512Half
│       ├── enclave_client.rs # HTTP client to SGX enclave
│       ├── perp_client.rs # Perp-specific enclave API
│       └── types.rs       # FP8 fixed-point arithmetic
├── research/              # 7 research documents (RU + EN versions)
├── docs/                  # Frontend API guide
├── tools/                 # xrpl_auth.py — auth header generator
└── tests/                 # API self-test
```

**Enclave repo (separate):** [xrpl-perp-dex-enclave](https://github.com/77ph/xrpl-perp-dex-enclave)

```
EthSignerEnclave/
├── Enclave/               # SGX enclave — perp engine + DCAP (C/C++)
│   ├── Enclave.cpp        # 11 perp ecalls + DCAP report + signing
│   ├── PerpState.h        # State structures (users, positions, margin)
│   └── Enclave.edl        # Enclave definition (ecall/ocall interface)
├── libapp/                # Enclave manager (C++ host-side wrapper)
├── server/                # REST API server (CivetWeb + perp_handler)
├── xrpl_client/           # Python scripts (setup, tests, demo, DCAP verifier)
│   ├── sgx_signer.py      # XRPL tx signing via SGX (ECDSA + multisig)
│   ├── dcap_verifier.py   # Independent DCAP quote verification
│   ├── demo_perp_dex.py   # Full E2E demo (10 steps)
│   └── test_full_v2.py    # Failure mode test suite (13 scenarios)
└── Dockerfile.azure       # Docker build for Azure DCsv3
```

## Public API

**Base URL:** `http://YOUR_SERVER:3000` (testnet)

### Trading (requires XRPL signature auth)

| Method | Endpoint | Description |
|--------|----------|-------------|
| POST | `/v1/orders` | Submit order (limit/market) |
| DELETE | `/v1/orders/{id}` | Cancel order |
| GET | `/v1/orders?user_id=` | List open orders |
| GET | `/v1/account/balance?user_id=` | Balance + positions + PnL |

### Market Data (public, no auth)

| Method | Endpoint | Description |
|--------|----------|-------------|
| GET | `/v1/markets/{market}/orderbook` | Order book depth |
| GET | `/v1/markets/{market}/ticker` | Best bid/ask/mid |
| GET | `/v1/markets/{market}/trades` | Recent trades |
| GET | `/v1/openapi.json` | OpenAPI 3.0 specification |

### Attestation (public, no auth)

| Method | Endpoint | Description |
|--------|----------|-------------|
| POST | `/v1/attestation/quote` | DCAP remote attestation (SGX Quote v3) |

Returns Intel-signed quote proving enclave code integrity on Azure DCsv3.
Returns 503 on hardware without DCAP support.

**Request:**
```json
{"user_data": "0xdeadbeef"}
```

**Response (Azure):**
```json
{
  "status": "success",
  "quote_hex": "0x030002...",
  "quote_size": 4734
}
```

Verify with: `python3 dcap_verifier.py --url http://host:3000/v1/attestation/quote`

## Key Properties

- **Anti-MEV**: orders encrypted with enclave's attested public key; operator sees only ciphertext
- **RLUSD-native**: all collateral, settlement, and fees denominated in RLUSD
- **DCAP Attestation**: Intel-signed proof that enclave runs genuine, untampered code (Azure DCsv3)
- **XRPL Multisig**: 2-of-3 SignerListSet — no single operator can steal funds, master key disabled
- **No sidechain**: settles directly on XRPL L1 in RLUSD
- **Rust orchestrator**: order book, P2P replication, live price feed — single 14MB binary
- **Failure tested**: 13 scenarios verified on 3-node infrastructure (Hetzner + 2 Azure)

## Research

| Document | Description |
|---|---|
| [Feasibility Analysis (RU)](research/01-feasibility-analysis.md) | XRPL capabilities, architecture options, recommendation |
| [Feasibility Analysis (EN)](research/01-feasibility-analysis.en.md) | English version |
| [TEE Perp Mechanics (RU)](research/02-tee-perp-mechanics-design.md) | Margin, funding, liquidation, vaults, settlement design |
| [TEE Perp Mechanics (EN)](research/02-tee-perp-mechanics-design.en.md) | English version |
| [Production Architecture (RU)](research/03-production-architecture.md) | HAProxy, public/internal API split, network rules |
| [Production Architecture (EN)](research/03-production-architecture.en.md) | English version |
| [Multi-Operator (RU)](research/04-multi-operator-architecture.md) | XRPL multisig 2-of-3, sequencer/validator, failover |
| [Multi-Operator (EN)](research/04-multi-operator-architecture.en.md) | English version |
| [TEE Rationale & API (RU)](research/05-tee-rationale-and-api-design.md) | Why TEE, trust model comparison, production API design |
| [TEE Rationale & API (EN)](research/05-tee-rationale-and-api-design.en.md) | English version |
| [Latency Analysis (RU)](research/06-latency-analysis.md) | Per-operation latency, comparison with CEX/DEX |
| [Latency Analysis (EN)](research/06-latency-analysis.en.md) | English version |
| [Failure Modes & Recovery (RU)](research/07-failure-modes-and-recovery.md) | Full stack failure scenarios, split-brain, cascading failures |
| [Failure Modes & Recovery (EN)](research/07-failure-modes-and-recovery.en.md) | English version |
| [TEE vs Smart Contract Security (RU)](research/08-tee-vs-smart-contract-security.md) | Why Drift's $200M hack is impossible with TEE + multisig |
| [TEE vs Smart Contract Security (EN)](research/08-tee-vs-smart-contract-security.en.md) | English version |

## Guides

| Document | Description |
|---|---|
| [Frontend API Guide](docs/frontend-api-guide.md) | Authentication, signing, all endpoints with examples |

## References

- [SGX_project](https://github.com/77ph/SGX_project/blob/feature/bitcoin_dlc/EthSignerEnclave) — TEE signing enclave, prediction market MVP on XRPL
- [data_collection](https://github.com/8ball030/data_collection) — perp DEX reference
- [xrpl-perp-dex-enclave](https://github.com/77ph/xrpl-perp-dex-enclave) — SGX enclave code (separate repo)

## License

TBD
