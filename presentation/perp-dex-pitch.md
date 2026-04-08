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
    font-size: 0.85em;
  }
  table {
    font-size: 0.95em;
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
  em {
    color: #aaaacc;
    font-style: normal;
  }
---

<!-- _class: lead -->

# Perp DEX on XRPL

**Perpetual Futures with TEE — No Smart Contracts Needed**

Settlement in RLUSD · Live now: `api-perp.ph18.io`

*Hack the Block · Paris · April 2026*

---

# The Problem

**XRPL has no smart contracts. RLUSD has no DeFi.**

- Want to trade perp futures? → Bridge to Ethereum (MEV, gas, exploits)
- Want yield on RLUSD? → No options on XRPL mainnet
- Want decentralized custody? → Sidechains lose XRPL security

> Drift Protocol drained for **$280M** in March 2025 — Solidity smart contract exploit.
> The smart contract model is fundamentally broken for DEX security.

**There is no perpetual DEX on XRPL today. We built one.**

---

# The Solution

**Trusted Execution Environments (Intel SGX) replace smart contracts.**

```
User ──► nginx ──► Orchestrator (Rust) ──► SGX Enclave (C/C++)
                                                  │
                                                  ▼
                                            XRPL Mainnet
                                          (RLUSD settlement)
```

**The enclave enforces rules in hardware:**
- Margin checks before every position open
- Withdrawal signing only if collateralized
- State integrity verified by Intel attestation

**Native XRPL primitives:**
- SignerListSet 2-of-3 multisig (no smart contract for custody)
- RLUSD escrow account (regulated stablecoin)
- 3-second finality (fast enough for trading)

---

# Why XRPL

| Feature | Smart Contract DEX | Perp DEX (XRPL + TEE) |
|---|---|---|
| Custody | Bridge multisig | **XRPL escrow (no bridge)** |
| Code verifiability | Public Solidity | **DCAP attestation (Intel)** |
| Attack surface | Logic exploits, MEV | Hardware side-channels |
| Settlement | L2 → L1 (7 days) | **3 sec on XRPL** |
| Stablecoin | USDC/USDT bridges | **Native RLUSD** |

> *"A project that uses XRPL escrow, multisig, RLUSD natively scores higher than one that just processes a payment."* — XRPL Commons

**We use everything XRPL has, the way it was meant to be used.**

---

# Live Demo

**`https://api-perp.ph18.io`**

```bash
# Public market data — no auth
curl https://api-perp.ph18.io/v1/markets

# Place a limit order (XRPL signature auth)
curl -X POST https://api-perp.ph18.io/v1/orders \
  -H "X-XRPL-Address: rBy1xS..." \
  -H "X-XRPL-Signature: 3045..." \
  -H "X-XRPL-Timestamp: 1712500000" \
  -d '{"user_id":"rBy1xS...","side":"buy","price":"0.55","size":"100","leverage":5}'

# Real-time WebSocket
wscat -c wss://api-perp.ph18.io/ws
```

**It's live. Right now.** Try it during my pitch.

---

# What We Built (in 6 days)

| Component | Tech | Status |
|---|---|---|
| Margin engine | C/C++ in SGX | ✅ Live |
| Order book | Rust CLOB | ✅ Live |
| XRPL auth | secp256k1 sigs | ✅ Live |
| Price feed | Binance, 5s | ✅ Live |
| WebSocket | Real-time events | ✅ Live |
| DCAP attestation | Azure DCsv3 | ✅ Verified |
| Multi-operator P2P | libp2p, election | ✅ Implemented |
| Trade history | PostgreSQL | ✅ Live |

**130 automated tests** · **52 audit findings closed** · **Open source (BSL 1.1)**

---

# Why This Wins

**Innovation:** First perpetual DEX on XRPL. TEE instead of smart contracts.

**Execution:** Live working API. 130 tests. Audited. Not a 36-hour prototype.

**Impact:** Unlocks DeFi for RLUSD holders. Expands XRPL beyond payments.

**Native XRPL:** SignerListSet, escrow, RLUSD — used the way they were designed.

> Smart contracts are public but exploitable.
> **Our code is private but verifiable** (DCAP attestation).

---

<!-- _class: lead -->

# Try it now

**[api-perp.ph18.io](https://api-perp.ph18.io)**

Open source · BSL 1.1 · ph18.io

*Andrey Lebedev · info@ph18.io*
