---
marp: true
theme: default
paginate: true
backgroundColor: #111112
color: #e0e0e1
style: |
  section {
    font-family: 'Segoe UI', Arial, sans-serif;
  }
  h1 {
    color: #00d4ff;
    font-size: 1.8em;
  }
  h2 {
    color: #00d4ff;
    font-size: 1.6em;
  }
  strong {
    color: #ffffff;
  }
  a {
    color: #4da6ff;
  }
  code {
    background: #1a1a2e;
    color: #00d4ff;
  }
  pre {
    background: #1e1e28 !important;
    color: #b0b0c0;
  }
  pre code {
    background: transparent !important;
    color: #b0b0c0;
  }
  blockquote {
    border-left: 4px solid #00d4ff;
    background: #232325;
    padding: 0.5em 1em;
    font-style: normal;
    color: #a2a2a4;
    font-size: 0.75em;
  }
  table {
    font-size: 0.85em;
  }
  th {
    background: #1a1a2e;
    color: #00d4ff;
  }
  td {
    background: #111118;
  }
  section.lead h1 {
    font-size: 2.8em;
    text-align: center;
  }
  section.lead p {
    text-align: center;
    font-size: 1.2em;
  }
  .columns {
    display: grid;
    grid-template-columns: 1fr 1fr;
    gap: 1.5em;
  }
  em {
    color: #aaaacc;
    font-style: normal;
  }
---

<!-- _class: lead -->

# Perp DEX

**Perpetual Futures on XRPL — Powered by TEE**

No smart contracts. No sidechains. No bridges.
Settlement in RLUSD.

*[perp.ph18.io](https://perp.ph18.io) · April 2026*

---

# The Problem

**XRPL has no smart contracts. DeFi can't exist on XRPL... or can it?**

Today, to trade perpetual futures you need:
- **Ethereum/Solana** — smart contracts, gas fees, MEV, bridge risk
- **Centralized exchanges** — custody risk, opaque execution
- **Sidechains** — lose XRPL security guarantees

**RLUSD holders have nowhere to go.** No yield, no hedging, no DeFi — on XRPL mainnet.

> We replace smart contracts with **hardware-secured computation (Intel SGX)**.
> XRPL handles what it does best: settlement.

---

# The Solution: TEE Instead of Smart Contracts

**Trusted Execution Environments run the same logic a smart contract would — but with hardware-enforced integrity.**

| Smart Contracts | TEE (Intel SGX) |
|---|---|
| Requires chain support | Works with **any** chain |
| Code is public (MEV) | Encrypted memory |
| Gas per operation | **Free** computation |
| Flash loan attacks | Not applicable |
| Re-entrancy bugs | Not applicable |

```
User ──► nginx (TLS) ──► Orchestrator (Rust) ──► SGX Enclave (C/C++)
                                                        │
                                                        ▼
                                                  XRPL Mainnet
                                                  (RLUSD settlement)
```

---

# Why XRPL?

| Feature | Why it matters for Perp DEX |
|---|---|
| **RLUSD** | Regulated stablecoin — institutional trust |
| **3-4 sec finality** | Fast enough for trading |
| **Native multisig** | SignerListSet 2-of-3 — no smart contract needed |
| **Low fees** | < $0.001 per transaction |
| **No MEV** | No mempool front-running |
| **No bridges** | Funds stay on XRPL L1 |

**XRPL is the only L1 where you can build a custody-secure DEX using native primitives alone.**

---

# What We Built — Live at api-perp.ph18.io

<style scoped>table { font-size: 0.75em; }</style>

| Component | Technology | Status |
|---|---|---|
| **Margin engine** | C/C++ inside SGX enclave | ✅ Live |
| **Order book** | Rust CLOB, price-time priority | ✅ Live |
| **XRPL auth** | secp256k1 signature verification | ✅ Live |
| **Price feed** | Binance XRP/USDT (5s updates) | ✅ Live |
| **Deposit monitor** | XRPL ledger watcher | ✅ Live |
| **Liquidation** | Automated (10s scan) | ✅ Live |
| **WebSocket** | Real-time trades, orderbook, ticker | ✅ Live |
| **DCAP attestation** | Intel SGX Quote v3 (Azure DCsv3) | ✅ Verified |
| **Multi-operator P2P** | libp2p gossipsub, sequencer election | ✅ Implemented |
| **On-chain proofs** | TEE-signed Merkle root on Sepolia | ✅ Implemented |

**Live API:** `https://api-perp.ph18.io/v1/openapi.json`

---

# Architecture: Single Operator

```
┌──────────────────────────────────────────────────┐
│                   Internet                        │
│                                                   │
│   User ─── HTTPS ──► nginx :443                  │
│                     (api-perp.ph18.io)            │
│                          │                        │
│                          ▼                        │
│                ┌──────────────────┐               │
│                │  Orchestrator    │               │
│                │  Rust :3000      │               │
│                │  • CLOB orderbook│               │
│                │  • XRPL auth     │               │
│                │  • WebSocket     │               │
│                └────────┬─────────┘               │
│                         │ localhost                │
│                         ▼                         │
│                ┌──────────────────┐               │
│                │  SGX Enclave     │               │
│                │  :9088           │               │
│                │  • Margin engine │               │
│                │  • ECDSA signing │               │
│                │  • Sealed state  │               │
│                └──────────────────┘               │
└──────────────────────────────────────────────────┘
```

---

# Architecture: Multi-Operator (Production)

```
Operator A (Azure)        Operator B (Azure)        Operator C (OVH)
┌───────────────┐        ┌───────────────┐        ┌───────────────┐
│ nginx         │        │ nginx         │        │ nginx         │
│ Orchestrator  │        │ Orchestrator  │        │ Orchestrator  │
│ SGX Enclave   │        │ SGX Enclave   │        │ SGX Enclave   │
│ ECDSA key A   │        │ ECDSA key B   │        │ ECDSA key C   │
└───────┬───────┘        └───────┬───────┘        └───────┬───────┘
        │                        │                        │
        └──── XRPL SignerListSet 2-of-3: escrow ─────────┘
```

**3 independent operators. 3 independent servers. 3 independent keys.**

- Sequencer election via heartbeat + priority failover
- P2P gossipsub for state replication
- DCAP attestation verifies identical enclave code
- **Attacker must compromise 2 of 3 servers to steal funds**

---

# How Trading Works

**1.** User signs order with XRPL secp256k1 key → `POST /v1/orders`

**2.** Orchestrator matches on CLOB (price-time priority, ~5ms)

**3.** For each fill → Orchestrator calls Enclave:
```
Enclave: check margin → open position → deduct fee → update state
```

**4.** Enclave rejects if insufficient margin — **hardware-enforced**

**5.** WebSocket broadcasts: trade, orderbook, ticker, liquidation

**6.** P2P replicates to validators (multi-operator mode)

> The operator cannot override the enclave's margin check.
> It's not software policy — it's hardware.

---

# Withdrawal: The Critical Path

**Funds never leave XRPL without enclave approval.**

```
User: "withdraw 100 RLUSD to rMyAddress"
  │
  ▼
Orchestrator: build XRPL Payment tx → compute signing hash
  │                                    (xrpl-mithril-codec)
  ▼
Enclave: check margin → ECDSA sign hash → return signature
  │
  ▼
Orchestrator: inject signature → serialize blob → submit to XRPL
  │
  ▼
XRPL: Payment from escrow → rMyAddress (100 RLUSD)
```

**If margin insufficient → enclave refuses to sign → no withdrawal possible.**

Production: 2-of-3 operators must sign (SignerListSet).

---

# Market Parameters

| Parameter | Value |
|---|---|
| Market | XRP-RLUSD-PERP |
| Settlement | RLUSD |
| Collateral | RLUSD (100% LTV) + XRP (90% LTV) |
| Max leverage | 20x |
| Taker fee | 0.05% |
| Maintenance margin | 0.5% |
| Funding interval | 8 hours |
| Liquidation penalty | 0.5% |
| XRP staking | 5 tiers (10-50% fee discount) |

---

# Security: TEE vs Smart Contracts

**March 2025: Drift Protocol drained for $280M (Solidity DEX)**

| Attack vector | Drift (Solidity) | Perp DEX (TEE) |
|---|---|---|
| Flash loans | **Exploited** | Not applicable (no composability) |
| Re-entrancy | Risk | Not possible (C, not EVM) |
| Governance manipulation | Risk | No governance (hardware rules) |
| Oracle manipulation | Risk | Enclave verifies independently |
| Bridge exploit | Risk (if bridged) | **No bridge** — XRPL native |
| Single operator compromise | N/A | 2-of-3 multisig required |

> **Our threat model: SGX side-channel attacks.** These require physical access
> to the CPU and advanced lab equipment. Compare with: "send one transaction
> to drain $280M."

---

# Verifiability: DCAP Remote Attestation

**Anyone can verify the enclave runs genuine, unmodified code.**

```bash
# Request attestation quote (public, no auth)
curl -X POST https://api-perp.ph18.io/v1/attestation/quote \
  -d '{"user_data": "0xdeadbeef"}'

# Returns: Intel-signed SGX Quote v3 with ECDSA certificate chain
# Verify: MRENCLAVE matches published code hash
```

| What it proves | How |
|---|---|
| Code identity | MRENCLAVE hash matches git commit |
| Hardware authenticity | Intel ECDSA signature chain |
| No tampering | Quote generated inside enclave |
| Freshness | user_data as nonce prevents replay |

**Smart contracts are public but exploitable. Our code is private but verifiable.**

---

# Testing: 128 Automated Tests

| Suite | Tests | What it verifies |
|---|---|---|
| **Rust unit** | 65 | Auth, election, orderbook, types, WebSocket |
| **Rust integration** | 6 | Mock enclave → full API flow |
| **Python e2e** | 22 | Real server: auth + trading + WebSocket |
| **Python invariants** | 21 | **Enclave FP8 arithmetic correctness** |
| **XRPL withdrawal** | 1 | Margin check + rollback on failure |

**Invariant tests verify the C/C++ code inside SGX:**
- deposit → balance exact match
- margin = notional / leverage (FP8 precision)
- fee = 0.05% of notional
- PnL: long = size × (mark - entry)
- liquidation at 0.5% margin ratio
- XRP collateral at 90% haircut

---

# Security Audit: 52 Findings, All Addressed

| Severity | Found | Fixed | By Design |
|---|---|---|---|
| Critical | 4 | 2 | 2 |
| High | 9 | 9 | 0 |
| Medium | 15 | 15 | 0 |
| Low/Info | 24 | 24 | 0 |
| **Total** | **52** | **50** | **2** |

**"By design" findings:** Single-operator trust boundary. Mitigated by multi-operator architecture (2-of-3). Adding per-ecall auth would require storing keys on disk — attacker with shell reads them anyway. Security theater vs. real architectural solution.

---

# Comparison with Hyperliquid

| | Hyperliquid | Perp DEX (XRPL) |
|---|---|---|
| Chain | Custom L1 | XRPL mainnet |
| Consensus | HyperBFT (4 validators) | Single sequencer + failover |
| Fund custody | Bridge multisig | **XRPL escrow (no bridge)** |
| Code | Closed source | **Open + DCAP attestation** |
| TPS | 100K+ | ~200 (enclave bottleneck) |
| Settlement | Sub-second | 3-4 sec (XRPL) |

**Hyperliquid: build a new blockchain.**
**We: extend an existing blockchain with TEE.**

Hyperliquid's JELLY incident (Mar 2025): validators unilaterally intervened.
Our enclave enforces rules regardless of operator intent.

---

# Roadmap

| Milestone | Timeline | Status |
|---|---|---|
| **PoC** — margin engine, orderbook, auth, XRPL integration | Apr 2-7 | ✅ Complete |
| **Multi-operator testnet** — 3 Azure DCsv3, XRPL multisig | Week 1-4 | Next |
| **Mainnet beta** — RLUSD settlement, public DCAP, SDK | Week 5-8 | Planned |
| **Production** — vaults, staking, additional markets | Week 9-12 | Planned |

**PoC delivered in 6 days:**
- 14 modules, ~5K LOC Rust, ~2K LOC C/C++
- 128 automated tests
- Live API: `https://api-perp.ph18.io`
- 10 research documents (bilingual RU/EN)

---

# Open Source

| | License | Repo |
|---|---|---|
| **Orchestrator** | BSL 1.1 → Apache 2.0 (4 years) | `xrpl-perp-dex` |
| **Enclave** | BSL 1.1 → Apache 2.0 (4 years) | `xrpl-perp-dex-enclave` |
| **Research** | CC BY-NC-ND 4.0 | 10 documents (RU + EN) |

**Research documents:**
01 Feasibility · 02 TEE Mechanics · 03 Production Architecture
04 Multi-Operator · 05 TEE Rationale · 06 Latency Analysis
07 Failure Modes · 08 TEE vs Smart Contracts · 09 Grant Narrative
10 Hyperliquid Comparison

---

<!-- _class: lead -->

# Summary

**XRPL + TEE = DeFi without smart contracts**

No bridges. No sidechains. No new chains.
Hardware-enforced rules. RLUSD settlement.

**Live:** `https://api-perp.ph18.io`
**Frontend:** `https://perp.ph18.io`

---

<!-- _class: lead -->

# Thank You

**[ph18.io](https://ph18.io)**

info@ph18.io

*API: api-perp.ph18.io · Frontend: perp.ph18.io*
