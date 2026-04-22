# Frontend Developer API Guide

**Base URL (dev/testnet):** `https://api-dev.xperp.fi`
**Base URL (prod):** `https://api.xperp.fi` — currently also points at the testnet dev instance while mainnet escrow is being re-provisioned (see `/v1/system/status` for the live `network` value).
**Market:** `XRP-USD-PERP`
**OpenAPI Spec:** `https://api-dev.xperp.fi/v1/openapi.json`

> **Status (2026-04-22):** The previous mainnet escrow `r4rwwSM9PUu7VcvPRWdu9pmZpmhCZS9mmc` was closed via `AccountDelete` on 2026-04-18. Do not send XRP to that address — it no longer exists. Only testnet deposits are accepted until mainnet is re-enabled.

---

## Getting Started: Discovering Network & Deposit Address

**Do not hardcode the deposit address.** It is returned by the server and may change (testnet ↔ mainnet, address rotation). Always read it from `GET /v1/system/status` at app startup.

### 1. Query system status (no auth)

```bash
curl https://api-dev.xperp.fi/v1/system/status
```

```json
{
  "network": "testnet",
  "deposit_address": "rhfcqLFTi3UFfpAwjqSKoYs3UjK99Kth6K",
  "is_in_maintenance": false,
  "market": "XRP-USD-PERP",
  "escrow_quorum": 2,
  "escrow_signer_count": 3,
  "version": "0.1.0"
}
```

Frontend contract:
- **`is_in_maintenance: true`** — show a maintenance banner and disable order submission / deposit CTAs.
- **`network`** — one of `"testnet"`, `"mainnet"`, `"devnet"`. Use this to pick the right wallet network mode and XRPL explorer (`testnet.xrpl.org` vs `livenet.xrpl.org`).
- **`deposit_address`** — the live escrow account. Show this to users for deposits. Re-fetch on every app load; do not cache across sessions.
- **`escrow_quorum` / `escrow_signer_count`** — optional trust UI (e.g. "Funds protected by 2-of-3 SGX multisig").

### 2. Choose a Wallet

| Wallet | Platform | Recommended for |
|--------|----------|-----------------|
| **Crossmark** | Browser extension (Chrome) | Desktop development, browser signing |
| **GemWallet** | Browser extension (Chrome) | Alternative to Crossmark |
| **Xaman (XUMM)** | iOS, Android | Mobile |
| **xrpl.js (code)** | Node.js / Browser | Automated testing, bots |
| **`tools/xrpl_auth.py`** | CLI (Python) | Quick testing from terminal |

> **Network selector:** set your wallet to the network returned by `/v1/system/status`. For the current dev instance that is **XRPL Testnet**. When connecting to `api.xperp.fi` post-mainnet-relaunch, switch wallets back to Mainnet.

### 3. Fund your wallet (testnet)

Use the XRPL Testnet Faucet: <https://xrpl.org/xrp-testnet-faucet.html> — it returns a funded r-address + seed you can import into Crossmark/Xaman/xrpl.js.

### 4. Deposit XRP to Trade

The escrow is protected by a **2-of-3 SGX multisig** (`SignerListSet`) — no single operator can move funds.

Fetch the current escrow address from `/v1/system/status`, then send a standard XRPL `Payment` to it. The orchestrator monitors the ledger and credits your margin automatically — no API call needed from your side.

**Via xrpl.js (testnet):**
```javascript
import { Client, Wallet } from 'xrpl';

// 1. Discover the deposit address
const status = await fetch('https://api-dev.xperp.fi/v1/system/status').then(r => r.json());
if (status.is_in_maintenance) {
  throw new Error('DEX is in maintenance');
}

// 2. Connect to the right XRPL network
const rpc = status.network === 'mainnet'
  ? 'wss://xrplcluster.com'
  : 'wss://s.altnet.rippletest.net:51233';
const client = new Client(rpc);
await client.connect();

// 3. Send deposit
const wallet = Wallet.fromSeed('sYOUR_SECRET');
const tx = await client.submitAndWait({
  TransactionType: 'Payment',
  Account: wallet.address,
  Destination: status.deposit_address,
  Amount: '100000000', // 100 XRP in drops
}, { wallet });
console.log('Deposited:', tx.result.hash);
```

After deposit, check your balance:
```bash
python3 tools/xrpl_auth.py --secret spXXX... \
  --request GET "https://api-dev.xperp.fi/v1/account/balance?user_id=rYOUR_ADDRESS"
```

### 5. Verify Your Setup

For testnet, use the Testnet explorer:
```
https://testnet.xrpl.org/accounts/rYOUR_ADDRESS_HERE
https://testnet.xrpl.org/accounts/{deposit_address from /v1/system/status}
```

For mainnet, use `https://livenet.xrpl.org/` with the same paths.

### Important Notes

- **Testnet XRP has no real value** — the current dev instance accepts testnet deposits only.
- **Reserve requirement** — each XRPL account needs minimum 10 XRP base reserve (both networks).
- **Auto-detection** — deposits are detected automatically (1s scan interval). No manual API call needed.
- **No DestinationTag support (yet)** — do not deposit from a shared/exchange wallet (Binance, Kraken, etc.). Use a personal wallet only; exchange deposits with `DestinationTag` are not routed correctly in the current build.
- **Signing** — the DEX uses secp256k1 ECDSA signatures (same curve as XRPL). All wallets above support this natively.

---

## Quick Start

### 1. Public endpoints (no authentication)

```bash
# DCAP Remote Attestation (verify enclave integrity)
curl -X POST https://api-dev.xperp.fi/v1/attestation/quote \
  -H "Content-Type: application/json" \
  -d '{"user_data": "0xdeadbeef"}'

# Order book
curl https://api-dev.xperp.fi/v1/markets/XRP-USD-PERP/orderbook

# Ticker (best bid/ask)
curl https://api-dev.xperp.fi/v1/markets/XRP-USD-PERP/ticker

# Recent trades
curl https://api-dev.xperp.fi/v1/markets/XRP-USD-PERP/trades
```

### 1b. WebSocket (real-time feed, no authentication)

```javascript
const ws = new WebSocket('wss://api-dev.xperp.fi/ws');
ws.onmessage = (e) => console.log(JSON.parse(e.data));

// Default: trades, orderbook, ticker, liquidations (market-wide).
// Subscribe to your own fills + order updates:
ws.onopen = () => ws.send(JSON.stringify({
    action: "subscribe",
    channels: ["user:rYourXrplAddress..."]
}));
// Full event set: trade, orderbook, ticker, liquidation,
//                 fill, order_update, position_changed
```

### 2. Authenticated endpoints (require XRPL signature)

```bash
# Install dependencies
pip install xrpl-py ecdsa requests

# Generate a wallet
python3 tools/xrpl_auth.py --generate
# Output: {"seed": "spXXX...", "address": "rXXX...", ...}

# Submit an order
python3 tools/xrpl_auth.py --secret spXXX... \
  --request POST https://api-dev.xperp.fi/v1/orders \
  '{"user_id":"X","side":"buy","type":"limit","price":"0.55000000","size":"100.00000000","leverage":5}'
```

---

## Authentication

All trading endpoints (orders, balance, cancel) require authentication. Two methods are available:

1. **Session token (recommended for browsers):** Sign once → get a Bearer token valid 30 min → use `Authorization: Bearer <token>` on all requests.
2. **Per-request signing:** Sign every request with 4 XRPL headers.

### Method 1: Session Token (recommended)

```
POST /v1/auth/login
Headers: X-XRPL-Address, X-XRPL-PublicKey, X-XRPL-Signature, X-XRPL-Timestamp
Body: empty

Response: { "status": "success", "token": "uuid...", "expires_in": 1800, "address": "rXXX" }
```

Then use `Authorization: Bearer <token>` on all subsequent requests. No more signing needed until the token expires (30 min).

### Method 2: Per-request signing

1. You have an XRPL secp256k1 keypair (seed → private key + public key → r-address)
2. For each request, you sign the request body (POST) or URI path (GET) with your private key
3. You send 4 headers with every authenticated request

### Headers

| Header | Value | Example |
|--------|-------|---------|
| `X-XRPL-Address` | Your XRPL r-address | `rBy1xSMqCesQ11Nh23KoddAfa5vBNHEK7` |
| `X-XRPL-PublicKey` | Compressed secp256k1 public key (hex, 66 chars) | `03c768238bf134...` |
| `X-XRPL-Signature` | DER-encoded ECDSA signature (hex) | `3045022100a461...` |
| `X-XRPL-Timestamp` | Unix epoch seconds (**mandatory**, max 60s drift) | `1712500000` |

### Signing algorithm (step by step)

**For POST/DELETE (with body):**

```
1. timestamp = current Unix epoch seconds (e.g., "1712500000")
2. body_bytes = UTF-8 encode the JSON body string
3. hash = SHA-256(body_bytes + timestamp_bytes)  → 32 bytes
4. signature = ECDSA_sign(hash, private_key)     → DER encoded
5. Normalize S to low-S (if S > curve_order/2, S = order - S)
6. headers = {
     "X-XRPL-Address": your r-address,
     "X-XRPL-PublicKey": compressed pubkey hex,
     "X-XRPL-Signature": DER signature hex,
     "X-XRPL-Timestamp": timestamp string
   }
```

**For GET (no body):**

```
1. timestamp = current Unix epoch seconds
2. path = full URI path with query string (e.g., "/v1/orders?user_id=rXXX")
3. hash = SHA-256(path_bytes + timestamp_bytes)
4. signature = ECDSA_sign(hash, private_key)
5. Same headers (including X-XRPL-Timestamp)
```

**Browser wallet note:** Crossmark and GemWallet apply SHA-512Half (first 32 bytes of SHA-512) before ECDSA internally. The server accepts both direct SHA-256 and SHA-512Half-wrapped signatures automatically.

**Important:** Timestamp must be within 60 seconds of server time. Requests with missing or expired timestamps are rejected.

**Important:** The `user_id` field in the request body (or query parameter) MUST match the `X-XRPL-Address` header (or session token address). The server rejects mismatches.

### Session token login (recommended for browser wallets)

Sign once to get a session token, then use `Authorization: Bearer <token>` for all subsequent requests. Valid 30 minutes.

```
POST /v1/auth/login
Headers: X-XRPL-Address, X-XRPL-PublicKey, X-XRPL-Signature, X-XRPL-Timestamp

Response: { "status": "success", "token": "uuid...", "expires_in": 1800, "address": "rXXX" }
```

Then use: `Authorization: Bearer <token>` instead of signing every request.

---

## Implementation Examples

### Python

```python
import hashlib
import json
import requests
from ecdsa import SECP256k1, SigningKey
from ecdsa.util import sigencode_der, sigdecode_der
from xrpl.core.keypairs import derive_keypair, derive_classic_address

# Your XRPL secret (secp256k1)
SECRET = "YOUR_XRPL_SECRET"  # from xrpl_auth.py --generate

# Derive keys
pub_hex, priv_hex = derive_keypair(SECRET)
address = derive_classic_address(pub_hex)

# Build signing key (strip 00 prefix from XRPL private key)
pk = priv_hex[2:] if len(priv_hex) == 66 else priv_hex
sk = SigningKey.from_string(bytes.fromhex(pk), curve=SECP256k1)

def sign_request(body_str):
    """Sign a POST body, return auth headers."""
    hash_bytes = hashlib.sha256(body_str.encode()).digest()
    sig = sk.sign_digest(hash_bytes, sigencode=sigencode_der)

    # Normalize to low-S
    r, s = sigdecode_der(sig, SECP256k1.order)
    if s > SECP256k1.order // 2:
        s = SECP256k1.order - s
    sig = sigencode_der(r, s, SECP256k1.order)

    return {
        "X-XRPL-Address": address,
        "X-XRPL-PublicKey": pub_hex.lower(),
        "X-XRPL-Signature": sig.hex(),
        "Content-Type": "application/json",
    }

# Submit order
body = json.dumps({
    "user_id": address,
    "side": "buy",
    "type": "limit",
    "price": "0.55000000",
    "size": "100.00000000",
    "leverage": 5,
})

resp = requests.post(
    "https://api-dev.xperp.fi/v1/orders",
    headers=sign_request(body),
    data=body,
)
print(resp.json())
```

### JavaScript (Node.js)

```javascript
const crypto = require('crypto');
const secp256k1 = require('secp256k1');  // npm install secp256k1
const fetch = require('node-fetch');      // npm install node-fetch

// Your keys (from xrpl_auth.py --generate)
const PRIVATE_KEY = Buffer.from('FA8076D0FB53AA4182AB3AF2B58EEEA5776D983E6CD9EA8580A676D5B82563C0', 'hex');
const PUBLIC_KEY = Buffer.from('03c768238bf134803cf864767dbfbdfcc134d4dac8124f0686c1d83fcfb56c16dc', 'hex');
const ADDRESS = 'rBy1xSMqCesQ11Nh23KoddAfa5vBNHEK7';

function signRequest(bodyStr) {
    // SHA-256 hash of body
    const hash = crypto.createHash('sha256').update(bodyStr, 'utf8').digest();

    // ECDSA sign (secp256k1 library returns low-S by default)
    const sigObj = secp256k1.ecdsaSign(hash, PRIVATE_KEY);

    // Convert to DER format
    const derSig = secp256k1.signatureExport(sigObj.signature);

    return {
        'X-XRPL-Address': ADDRESS,
        'X-XRPL-PublicKey': PUBLIC_KEY.toString('hex'),
        'X-XRPL-Signature': Buffer.from(derSig).toString('hex'),
        'Content-Type': 'application/json',
    };
}

// Submit order
const body = JSON.stringify({
    user_id: ADDRESS,
    side: 'buy',
    type: 'limit',
    price: '0.55000000',
    size: '100.00000000',
    leverage: 5,
});

fetch('https://api-dev.xperp.fi/v1/orders', {
    method: 'POST',
    headers: signRequest(body),
    body: body,
})
.then(r => r.json())
.then(console.log);
```

### JavaScript (Browser with ethers.js)

```javascript
// Using ethers.js (already common in web3 frontends)
import { SigningKey, sha256 } from 'ethers';

const PRIVATE_KEY = '0xFA8076D0FB53AA4182AB3AF2B58EEEA5776D983E6CD9EA8580A676D5B82563C0';
const signingKey = new SigningKey(PRIVATE_KEY);
const ADDRESS = 'rBy1xSMqCesQ11Nh23KoddAfa5vBNHEK7';

function signRequest(bodyStr) {
    const hash = sha256(new TextEncoder().encode(bodyStr));
    const sig = signingKey.sign(hash);

    // ethers returns { r, s, v } — need DER encoding
    // For simplicity, send r+s as hex and let server parse
    // Or use a DER encoding library

    return {
        'X-XRPL-Address': ADDRESS,
        'X-XRPL-PublicKey': signingKey.compressedPublicKey.slice(2), // remove 0x
        'X-XRPL-Signature': derEncode(sig.r, sig.s), // implement DER encoding
    };
}
```

---

## API Reference

### System Status

```
GET /v1/system/status
Auth: None
```

Returns the current network, deposit address, and maintenance flag. Call this at app startup to configure the client; do not hardcode the deposit address or network.

**Response:**
```json
{
    "network": "testnet",
    "deposit_address": "rhfcqLFTi3UFfpAwjqSKoYs3UjK99Kth6K",
    "is_in_maintenance": false,
    "market": "XRP-USD-PERP",
    "escrow_quorum": 2,
    "escrow_signer_count": 3,
    "version": "0.1.0"
}
```

**Fields:**

| Field | Type | Description |
|-------|------|-------------|
| `network` | string | `"testnet"`, `"mainnet"`, or `"devnet"` — matches the XRPL network the orchestrator is connected to |
| `deposit_address` | string | Current escrow r-address; show this to users for deposits |
| `is_in_maintenance` | boolean | When `true`, the frontend should display a maintenance banner and block new order/deposit CTAs |
| `market` | string | Trading pair (currently `"XRP-USD-PERP"` only) |
| `escrow_quorum` | integer\|null | Multisig signatures required (e.g. `2`); `null` if the orchestrator was started without a signers-config (single-operator fallback) |
| `escrow_signer_count` | integer\|null | Total SGX signers in the multisig (e.g. `3`); `null` under the same condition as `escrow_quorum` |
| `version` | string | Orchestrator build version |

### Submit Order

```
POST /v1/orders
Auth: Required
```

**Request:**
```json
{
    "user_id": "rBy1xSMqCesQ11Nh23KoddAfa5vBNHEK7",
    "side": "buy",
    "type": "limit",
    "price": "0.55000000",
    "size": "100.00000000",
    "leverage": 5,
    "time_in_force": "gtc",
    "reduce_only": false,
    "client_order_id": "my-order-123"
}
```

**Fields:**

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `user_id` | string | Yes | Must match X-XRPL-Address |
| `side` | string | Yes | `"buy"` or `"sell"` (aliases: `"long"`, `"short"`) |
| `type` | string | No | `"limit"` (default) or `"market"` |
| `price` | string | For limit | FP8 format: `"0.55000000"` |
| `size` | string | Yes | FP8 format, quantity in XRP |
| `leverage` | integer | No | 1-20, default 1 |
| `time_in_force` | string | No | `"gtc"` (default), `"ioc"`, `"fok"` |
| `reduce_only` | boolean | No | Default false |
| `client_order_id` | string | No | Your custom ID |

**Response:**
```json
{
    "status": "success",
    "order_id": 1,
    "order_status": "Open",
    "filled": "0.00000000",
    "remaining": "100.00000000",
    "trades": [],
    "failed_fills": 0
}
```

**Order status values:** `Open`, `PartiallyFilled`, `Filled`, `Cancelled`

**If order matches immediately:**
```json
{
    "status": "success",
    "order_id": 2,
    "order_status": "Filled",
    "filled": "100.00000000",
    "remaining": "0.00000000",
    "trades": [
        {
            "trade_id": 1,
            "price": "0.55000000",
            "size": "100.00000000",
            "maker_user_id": "rAlice...",
            "taker_user_id": "rBob...",
            "taker_side": "buy"
        }
    ],
    "failed_fills": 0
}
```

---

### Cancel Order

```
DELETE /v1/orders/{order_id}
Auth: Required
```

**Response:**
```json
{
    "status": "success",
    "order_id": 1,
    "status": "Cancelled"
}
```

---

### Cancel All Orders

```
DELETE /v1/orders?user_id=rXXX
Auth: Required (user_id must match)
```

**Response:**
```json
{
    "status": "success",
    "cancelled": 3
}
```

---

### Get Open Orders

```
GET /v1/orders?user_id=rXXX
Auth: Required (user_id must match)
```

**Response:**
```json
{
    "status": "success",
    "orders": [
        {
            "order_id": 1,
            "side": "long",
            "type": "Limit",
            "price": "0.55000000",
            "size": "100.00000000",
            "filled": "0.00000000",
            "remaining": "100.00000000",
            "status": "Open"
        }
    ]
}
```

---

### Get Balance & Positions

```
GET /v1/account/balance?user_id=rXXX
Auth: Required (user_id must match)
```

**Response:**
```json
{
    "status": "success",
    "data": {
        "margin_balance": "200.00000000",
        "unrealized_pnl": "5.50000000",
        "used_margin": "26.24400000",
        "available_margin": "179.25600000",
        "positions": [
            {
                "position_id": 0,
                "side": "long",
                "size": "100.00000000",
                "entry_price": "1.31220000",
                "margin": "26.24400000",
                "unrealized_pnl": "5.50000000"
            }
        ]
    }
}
```

---

### Order Book

```
GET /v1/markets/XRP-USD-PERP/orderbook?levels=20
Auth: Not required
```

**Response:**
```json
{
    "status": "success",
    "bids": [
        ["0.55000000", "100.00000000"],
        ["0.54000000", "200.00000000"]
    ],
    "asks": [
        ["0.56000000", "150.00000000"],
        ["0.57000000", "50.00000000"]
    ]
}
```

Format: `[price, total_size_at_price]`, bids descending, asks ascending.

---

### Ticker

```
GET /v1/markets/XRP-USD-PERP/ticker
Auth: Not required
```

**Response:**
```json
{
    "status": "success",
    "best_bid": "0.55000000",
    "best_ask": "0.56000000",
    "mid_price": "0.55500000"
}
```

Values are `null` if no orders on that side.

---

### Recent Trades

```
GET /v1/markets/XRP-USD-PERP/trades
Auth: Not required
```

**Response:**
```json
{
    "status": "success",
    "trades": [
        {
            "trade_id": 1,
            "price": "0.55000000",
            "size": "100.00000000",
            "taker_side": "long",
            "timestamp_ms": 1743500000000
        }
    ]
}
```

Last 100 trades, most recent first.

---

### Funding Rate

```
GET /v1/markets/XRP-USD-PERP/funding
Auth: Not required
```

**Response:**
```json
{
    "status": "success",
    "funding_rate": "0.00010000",
    "mark_price": "1.31000000",
    "next_funding_time": 1712528800,
    "interval_hours": 8
}
```

---

### List Markets

```
GET /v1/markets
Auth: Not required
```

**Response:**
```json
{
    "status": "success",
    "markets": [{
        "market": "XRP-USD-PERP",
        "base": "XRP",
        "quote": "USD",
        "mark_price": "1.31000000",
        "best_bid": "1.30500000",
        "best_ask": "1.31500000",
        "max_leverage": 20,
        "maintenance_margin": "0.00500000",
        "taker_fee": "0.00050000",
        "funding_interval_hours": 8,
        "status": "active"
    }]
}
```

---

### Withdraw

```
POST /v1/withdraw
Auth: Required
```

**Request:**
```json
{
    "user_id": "rBy1xSMqCesQ11Nh23KoddAfa5vBNHEK7",
    "amount": "100.00000000",
    "destination": "rMyXRPLAddress..."
}
```

**Response (success):**
```json
{
    "status": "success",
    "amount": "100.00000000",
    "destination": "rMyXRPLAddress...",
    "xrpl_tx_hash": "ABC123...",
    "message": "withdrawal submitted to XRPL"
}
```

**Response (insufficient margin):**
```json
{
    "status": "error",
    "message": "enclave rejected withdrawal"
}
```

---

### DCAP Remote Attestation

```
POST /v1/attestation/quote
Auth: Not required
```

Verifies that the SGX enclave is running genuine, untampered code on Intel hardware.
Returns an Intel-signed SGX Quote v3 with ECDSA certificate chain.

**Request:**
```json
{"user_data": "0xdeadbeef"}
```

`user_data` is a challenge nonce (up to 64 bytes hex). Include a random value to prevent replay attacks.

**Response (Azure DCsv3 — DCAP available):**
```json
{
    "status": "success",
    "quote_hex": "0x030002000000000...",
    "quote_size": 4734
}
```

**Response (Hetzner / no DCAP hardware):**
```json
{
    "status": "error",
    "message": "DCAP attestation not available on this platform. Use Azure DCsv3 for hardware attestation."
}
```
HTTP status: 503

**Verification:** Use `dcap_verifier.py` from the enclave repo to independently verify the quote:
```bash
python3 dcap_verifier.py --url https://api-dev.xperp.fi/v1 --expected-mrenclave <HASH>
```

### Building the "Verify Enclave" Page

The attestation verifier is a standalone page (e.g. `perp.ph18.io/verify`) that
lets anyone confirm the SGX enclave is genuine. No authentication needed.

**What it does:**
1. User clicks "Verify Enclave"
2. Frontend generates a random nonce, calls `POST /v1/attestation/quote`
3. Displays the result: MRENCLAVE hash, quote size, verification status

**Implementation (step by step):**

```javascript
// 1. Generate random challenge nonce (prevents replay)
const nonce = '0x' + crypto.getRandomValues(new Uint8Array(32))
  .reduce((s, b) => s + b.toString(16).padStart(2, '0'), '');

// 2. Fetch attestation quote from live enclave
const res = await fetch('https://api-dev.xperp.fi/v1/attestation/quote', {
  method: 'POST',
  headers: { 'Content-Type': 'application/json' },
  body: JSON.stringify({ user_data: nonce }),
});
const data = await res.json();

// 3. Parse the SGX Quote v3 structure
if (data.status === 'success') {
  const quoteBytes = hexToBytes(data.quote_hex);

  // SGX Quote v3 layout:
  //   bytes [0..1]   = version (0x0003)
  //   bytes [2..3]   = attestation key type
  //   bytes [112..143] = MRENCLAVE (32 bytes) — the enclave identity hash
  //   bytes [144..175] = MRSIGNER  (32 bytes) — the signer identity hash
  const mrenclave = bytesToHex(quoteBytes.slice(112, 144));
  const mrsigner  = bytesToHex(quoteBytes.slice(144, 176));

  // 4. Display to user
  // - MRENCLAVE: unique hash of the enclave binary code
  // - Quote size: should be 4,734 bytes (Intel SGX Quote v3 with cert chain)
  // - Compare MRENCLAVE against the published enclave binary hash
}
```

**UI elements to show:**

| Field | Value | What it means |
|-------|-------|---------------|
| Status | "Intel SGX Verified ✅" or "Not available ❌" | Whether DCAP attestation succeeded |
| MRENCLAVE | `a3b7c9d1e5f2...` (32-byte hex) | Hash of the exact code running inside the enclave. If this matches the published binary hash, the enclave is running the authentic code |
| MRSIGNER | `8c4f5a6b7d2e...` (32-byte hex) | Identity of who built/signed the enclave |
| Quote size | `4,734 bytes` | Size of the Intel-signed attestation proof |
| Nonce | The random value you sent | Proves the quote is fresh (not replayed) |

**Published MRENCLAVE for comparison:**

The enclave binary is published at the project repo. To compute the expected MRENCLAVE:
```bash
# Hash the published enclave .so binary (SGX uses SHA-256 of page measurements)
# The exact MRENCLAVE is printed during enclave build:
#   MRENCLAVE: a3b7c9d1e5f2...
# Compare this with what the attestation quote returns.
```

**Error handling:**
- HTTP 503 → enclave runs on Hetzner (no SGX hardware). Show: "Attestation requires Intel SGX hardware (Azure DCsv3). This node does not support DCAP."
- HTTP 502 → enclave is unreachable. Show: "Enclave offline, try again later."
- `quote_size` not 4,734 → unexpected quote format, warn user.

**Key point:** This page does NOT show any enclave source code. It only proves
that the enclave binary matches what was published, using Intel's hardware-based
attestation. Anyone can verify, no trust required.

---

## WebSocket (Real-Time Feed)

```
wss://api-dev.xperp.fi/ws   (dev, testnet)
wss://api.xperp.fi/ws       (prod, currently aliased to dev until mainnet relaunch)
Auth: Not required
```

Connect and receive JSON events pushed by the server. On connect, clients are
automatically subscribed to the default public channels
`{trades, orderbook, ticker, liquidations}`. Send control frames to adjust
the subscription set, including `user:rXXX` channels for per-user events.

### Channels

| Channel | Events delivered |
|---|---|
| `trades` | `trade` |
| `orderbook` | `orderbook` |
| `ticker` | `ticker` |
| `liquidations` | `liquidation` (market-wide) |
| `user:rXXX` | `fill`, `order_update`, `position_changed`, plus any `liquidation` where `user_id == rXXX` |

### Control frames (client → server)

Send a JSON text frame at any time to change your subscription. Each control
frame is acknowledged with a `subscribed` event listing the current channels.

```json
// Add channels (does not remove existing)
{"action": "subscribe",   "channels": ["trades", "user:rAlice..."]}

// Remove specific channels
{"action": "unsubscribe", "channels": ["ticker"]}

// Replace the entire subscription set
{"action": "set",         "channels": ["ticker", "user:rBob..."]}

// Keepalive (server replies with {"type":"pong"})
{"action": "ping"}
```

Unknown channels are ignored silently. Invalid JSON produces an
`{"type":"error","message":"..."}` frame but keeps the connection open.

### Event types

**Trade** — broadcast to `trades` channel on each matched order:
```json
{
    "type": "trade",
    "trade_id": 42,
    "price": "0.55000000",
    "size": "100.00000000",
    "taker_side": "long",
    "maker_user_id": "rAlice...",
    "taker_user_id": "rBob...",
    "timestamp_ms": 1743500000000
}
```

**Orderbook** — broadcast to `orderbook` after each trade (depth 20):
```json
{
    "type": "orderbook",
    "bids": [["0.55000000", "100.00000000"], ["0.54000000", "200.00000000"]],
    "asks": [["0.56000000", "150.00000000"], ["0.57000000", "50.00000000"]]
}
```

**Ticker** — broadcast to `ticker` periodically from the price feed loop:
```json
{
    "type": "ticker",
    "mark_price": "0.55120000",
    "index_price": "0.55120000",
    "timestamp": 1743500005
}
```

**Liquidation** — broadcast to `liquidations` AND to the victim's `user:rXXX`:
```json
{
    "type": "liquidation",
    "position_id": 7,
    "user_id": "rCharlie...",
    "price": "0.48000000"
}
```

**Fill** — per-user execution notification. Each trade emits TWO `fill` events
(one for the taker, one for the maker) delivered only to matching `user:rXXX`
channels:
```json
{
    "type": "fill",
    "user_id": "rBob...",
    "order_id": 199,
    "trade_id": 42,
    "side": "long",
    "role": "taker",
    "price": "0.55000000",
    "size": "100.00000000",
    "timestamp_ms": 1743500000000
}
```

**OrderUpdate** — order lifecycle. Delivered to the order owner's `user:rXXX`:
```json
{
    "type": "order_update",
    "user_id": "rBob...",
    "order_id": 199,
    "status": "partiallyfilled",   // "open" | "partiallyfilled" | "filled" | "cancelled"
    "filled": "50.00000000",
    "remaining": "50.00000000",
    "client_order_id": "my-42"     // null if not set by client
}
```

**PositionChanged** — nudge to re-fetch `GET /v1/account/positions`. The
orchestrator does not mirror positions (they live in the SGX enclave), so
this is the signal to ask the enclave for fresh state. Delivered to the
owner's `user:rXXX`:
```json
{
    "type": "position_changed",
    "user_id": "rBob...",
    "reason": "fill"               // "fill" | "liquidation"
}
```

**subscribed** — ACK after a control frame (server → client):
```json
{
    "type": "subscribed",
    "channels": ["trades", "orderbook", "ticker", "liquidations", "user:rBob..."]
}
```

### JavaScript example

```javascript
const ws = new WebSocket('wss://api-dev.xperp.fi/ws');
const myAddress = 'rBobXRPLAddress...';

ws.onopen = () => {
    // Add our own user channel on top of the default public set.
    ws.send(JSON.stringify({
        action: 'subscribe',
        channels: [`user:${myAddress}`]
    }));
};

ws.onmessage = (event) => {
    const msg = JSON.parse(event.data);
    switch (msg.type) {
        case 'trade':
            console.log(`Trade: ${msg.size} XRP @ ${msg.price}`);
            break;
        case 'orderbook':
            console.log(`Orderbook: ${msg.bids.length} bids, ${msg.asks.length} asks`);
            break;
        case 'ticker':
            console.log(`Mark: ${msg.mark_price}`);
            break;
        case 'liquidation':
            console.log(`Liquidation: position ${msg.position_id}`);
            break;
        case 'fill':
            console.log(`Fill ${msg.role}: ${msg.size} @ ${msg.price}`);
            break;
        case 'order_update':
            console.log(`Order ${msg.order_id}: ${msg.status}`);
            break;
        case 'position_changed':
            // Re-fetch /v1/account/positions here.
            fetchPositions();
            break;
        case 'subscribed':
            console.log(`Subscribed: ${msg.channels}`);
            break;
    }
};

ws.onclose = () => console.log('Disconnected, reconnecting...');
```

### Python example

```python
import asyncio
import json
import websockets

MY_ADDR = "rBobXRPLAddress..."

async def listen():
    async with websockets.connect("wss://api-dev.xperp.fi/ws") as ws:
        await ws.send(json.dumps({
            "action": "subscribe",
            "channels": [f"user:{MY_ADDR}"],
        }))
        async for message in ws:
            event = json.loads(message)
            print(f"[{event['type']}] {event}")

asyncio.run(listen())
```

### Notes

- No authentication on `/ws`. Data is either public (market data) or
  references xrpl_addresses that are already public. If you want to gate
  `user:rXXX` channels, add a signed X-XRPL-Signature check on upgrade.
- Slow clients skip events (no backpressure, no blocking of producers).
- Reconnect on disconnect — the server keeps no per-client state across
  connections, so always re-send your `subscribe` on reconnect.
- All prices/sizes in FP8 string format.

---

## Number Format (FP8)

All prices and sizes use **FP8 format**: strings with exactly 8 decimal places.

```
"0.55000000"    = 0.55
"100.00000000"  = 100
"1.31220000"    = 1.3122
```

Always send as strings, not numbers. The server rejects numeric values.

---

## Error Responses

```json
{"status": "error", "message": "missing X-XRPL-Address header"}
{"status": "error", "message": "signature verification failed"}
{"status": "error", "message": "user_id 'rAttacker' does not match authenticated address 'rBy1x...'"}
{"status": "error", "message": "leverage must be 1-20"}
{"status": "error", "message": "invalid or non-positive size"}
```

HTTP status codes: `200` success, `400` bad request, `401` unauthorized, `403` forbidden, `500` server error.

---

## Testing

```bash
# Generate wallet
python3 tools/xrpl_auth.py --generate

# Use the generated secret for all subsequent requests
export SECRET="spXXX..."

# Place a limit buy
python3 tools/xrpl_auth.py --secret $SECRET \
  --request POST https://api-dev.xperp.fi/v1/orders \
  '{"user_id":"X","side":"buy","type":"limit","price":"0.55","size":"100","leverage":5}'

# Check orderbook (no auth needed)
curl https://api-dev.xperp.fi/v1/markets/XRP-USD-PERP/orderbook

# Get your orders
python3 tools/xrpl_auth.py --secret $SECRET \
  --request GET "https://api-dev.xperp.fi/v1/orders?user_id=YOUR_ADDRESS"
```

Note: For `--request GET`, the tool signs the URI path. For `--request POST`, it signs the body.
The `user_id` field is auto-replaced with your authenticated address.
