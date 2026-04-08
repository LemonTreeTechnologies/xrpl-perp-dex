# Live Demo Script — Hack the Block

**Duration:** ~2 minutes (within 5-min pitch)
**URL:** https://api-perp.ph18.io
**Backup:** screen recording + curl outputs in this file

---

## Pre-flight check (do this 5 min before pitch)

```bash
# 1. Server is up
curl -s https://api-perp.ph18.io/v1/markets | jq

# 2. Mark price updating
curl -s https://api-perp.ph18.io/v1/markets/XRP-RLUSD-PERP/funding | jq

# 3. WebSocket works
wscat -c wss://api-perp.ph18.io/ws  # ctrl+c after first message

# 4. Test wallet ready
python3 tools/xrpl_auth.py --generate
```

---

## Demo flow (live, in front of judges)

### Step 1: "Here's the live API. No mocks." (15 sec)

```bash
curl -s https://api-perp.ph18.io/v1/markets | jq
```

**Show:**
```json
{
  "markets": [{
    "market": "XRP-RLUSD-PERP",
    "mark_price": "1.30740000",
    "max_leverage": 20,
    "taker_fee": "0.00050000",
    "status": "active"
  }]
}
```

**Say:** *"Mark price updates every 5 seconds from Binance. Real perp market."*

---

### Step 2: "Here's the orderbook." (15 sec)

```bash
curl -s https://api-perp.ph18.io/v1/markets/XRP-RLUSD-PERP/orderbook | jq
```

**Say:** *"Native CLOB. Price-time priority. ~5ms matching."*

---

### Step 3: "Watch a trade happen." (45 sec)

**Terminal 1 — WebSocket subscribe:**
```bash
wscat -c wss://api-perp.ph18.io/ws
```

**Terminal 2 — place orders (use pre-prepared script):**
```bash
./demo-trade.sh
```

The script:
1. Generates 2 wallets
2. Wallet A places limit sell @ 1.31
3. Wallet B places market buy
4. Trade matches

**Show in WebSocket:**
```json
{"type":"trade","trade_id":1,"price":"1.31000000","size":"100.00000000","taker_side":"long",...}
{"type":"orderbook","bids":[],"asks":[]}
```

**Say:** *"Trade matched in milliseconds. Real-time WebSocket. Production-ready."*

---

### Step 4: "Verify with attestation." (15 sec)

```bash
curl -s -X POST https://api-perp.ph18.io/v1/attestation/quote \
  -H 'Content-Type: application/json' \
  -d '{"user_data":"0xdeadbeef"}' | jq
```

**Say:** *"DCAP attestation. Intel-signed proof that the enclave runs the published code. You can verify this against our GitHub MRENCLAVE hash."*

---

### Step 5: "And it's tested." (15 sec)

**Show on screen (not terminal):**
```
130 automated tests passing:
  64 Rust unit
  6 Rust integration
  22 Python e2e
  21 invariant tests (FP8 arithmetic in SGX)
  17 audit verification

52 security findings — all closed.
```

**Say:** *"This isn't a hackathon prototype. It's production-grade code."*

---

## Fallback (if API is down)

**Plan A:** Switch to localhost demo
```bash
ssh andrey@94.130.18.162 "curl https://localhost:9088/v1/pool/status"
```

**Plan B:** Show recorded demo video
- File: `presentation/demo-recording.mp4`
- Length: 90 seconds
- Captures full flow above

**Plan C:** Show test output
```bash
cd orchestrator && cargo test 2>&1 | tail -20
```

---

## Pre-recorded curl outputs (for Plan B)

### markets
```json
{
  "status": "success",
  "markets": [{
    "market": "XRP-RLUSD-PERP",
    "base": "XRP",
    "quote": "RLUSD",
    "mark_price": "1.30740000",
    "best_bid": null,
    "best_ask": null,
    "max_leverage": 20,
    "maintenance_margin": "0.00500000",
    "taker_fee": "0.00050000",
    "funding_interval_hours": 8,
    "status": "active"
  }]
}
```

### funding
```json
{
  "status": "success",
  "funding_rate": "0.00010000",
  "mark_price": "1.30740000",
  "next_funding_time": 1712528800,
  "interval_hours": 8
}
```

### attestation/quote (Azure DCsv3)
```json
{
  "status": "success",
  "quote_hex": "0x030002000000000...",
  "quote_size": 4734
}
```

---

## One-sentence pitch

> **"We built the first perpetual futures DEX on XRPL mainnet, replacing smart contracts with Intel SGX enclaves and using RLUSD for settlement — and it's live right now at api-perp.ph18.io."**

## 30-second elevator

> "XRPL has no smart contracts, so DeFi can't exist there — until now. We use Intel SGX to run the trading logic in hardware-attested enclaves, with RLUSD settlement on XRPL mainnet. No bridges, no sidechains, no MEV. The code is open source, the audit found 52 issues — all closed. It's running live at api-perp.ph18.io. We process orders in 5 milliseconds and settle on XRPL in 3 seconds."
