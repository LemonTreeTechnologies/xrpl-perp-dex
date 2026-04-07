# Frontend Developer API Guide

**Base URL:** `https://api-perp.ph18.io` (production) or `http://localhost:3000` (dev)
**Market:** `XRP-RLUSD-PERP`
**OpenAPI Spec:** `https://api-perp.ph18.io/v1/openapi.json`

---

## Quick Start

### 1. Public endpoints (no authentication)

```bash
# DCAP Remote Attestation (verify enclave integrity)
curl -X POST http://YOUR_SERVER:3000/v1/attestation/quote \
  -H "Content-Type: application/json" \
  -d '{"user_data": "0xdeadbeef"}'

# Order book
curl http://YOUR_SERVER:3000/v1/markets/XRP-RLUSD-PERP/orderbook

# Ticker (best bid/ask)
curl http://YOUR_SERVER:3000/v1/markets/XRP-RLUSD-PERP/ticker

# Recent trades
curl http://YOUR_SERVER:3000/v1/markets/XRP-RLUSD-PERP/trades
```

### 1b. WebSocket (real-time feed, no authentication)

```javascript
const ws = new WebSocket('ws://YOUR_SERVER:3000/ws');
ws.onmessage = (e) => console.log(JSON.parse(e.data));
// Events: trade, orderbook, ticker, liquidation
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
  --request POST http://YOUR_SERVER:3000/v1/orders \
  '{"user_id":"X","side":"buy","type":"limit","price":"0.55000000","size":"100.00000000","leverage":5}'
```

---

## Authentication

All trading endpoints (orders, balance, cancel) require XRPL signature authentication.

### How it works

1. You have an XRPL secp256k1 keypair (seed → private key + public key → r-address)
2. For each request, you sign the request body (POST) or URI path (GET) with your private key
3. You send 3 extra headers with every authenticated request

### Headers

| Header | Value | Example |
|--------|-------|---------|
| `X-XRPL-Address` | Your XRPL r-address | `rBy1xSMqCesQ11Nh23KoddAfa5vBNHEK7` |
| `X-XRPL-PublicKey` | Compressed secp256k1 public key (hex, 66 chars) | `03c768238bf134...` |
| `X-XRPL-Signature` | DER-encoded ECDSA signature (hex) | `3045022100a461...` |
| `X-XRPL-Timestamp` | Unix epoch seconds (**mandatory**, max 30s drift) | `1712500000` |

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

**Important:** Timestamp must be within 30 seconds of server time. Requests with missing or expired timestamps are rejected.

**Important:** The `user_id` field in the request body (or query parameter) MUST match the `X-XRPL-Address` header. The server rejects mismatches.

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
    "http://YOUR_SERVER:3000/v1/orders",
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

fetch('http://YOUR_SERVER:3000/v1/orders', {
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
GET /v1/markets/XRP-RLUSD-PERP/orderbook?levels=20
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
GET /v1/markets/XRP-RLUSD-PERP/ticker
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
GET /v1/markets/XRP-RLUSD-PERP/trades
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
GET /v1/markets/XRP-RLUSD-PERP/funding
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
        "market": "XRP-RLUSD-PERP",
        "base": "XRP",
        "quote": "RLUSD",
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
python3 dcap_verifier.py --url http://YOUR_SERVER:3000/v1 --expected-mrenclave <HASH>
```

---

## WebSocket (Real-Time Feed)

```
ws://YOUR_SERVER:3000/ws
wss://api-perp.ph18.io/ws   (production, via nginx)
Auth: Not required
```

Connect and receive JSON events pushed by the server. No subscription messages needed —
all events are broadcast to all clients.

### Event types

**Trade** (on each fill):
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

**Orderbook** (depth snapshot after trades):
```json
{
    "type": "orderbook",
    "bids": [["0.55000000", "100.00000000"], ["0.54000000", "200.00000000"]],
    "asks": [["0.56000000", "150.00000000"], ["0.57000000", "50.00000000"]]
}
```

**Ticker** (every 5 seconds):
```json
{
    "type": "ticker",
    "mark_price": "0.55120000",
    "index_price": "0.55120000",
    "timestamp": 1743500005
}
```

**Liquidation** (when position is liquidated):
```json
{
    "type": "liquidation",
    "position_id": 7,
    "user_id": "rCharlie...",
    "price": "0.48000000"
}
```

### JavaScript example

```javascript
const ws = new WebSocket('wss://api-perp.ph18.io/ws');

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
            console.log(`Price: ${msg.mark_price}`);
            break;
        case 'liquidation':
            console.log(`Liquidation: position ${msg.position_id}`);
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

async def listen():
    async with websockets.connect("wss://api-perp.ph18.io/ws") as ws:
        async for message in ws:
            event = json.loads(message)
            print(f"[{event['type']}] {event}")

asyncio.run(listen())
```

### Notes

- No authentication — public market data feed
- Slow clients skip events (no backpressure, no blocking)
- Reconnect on disconnect — server doesn't store state per client
- All prices/sizes in FP8 string format

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
  --request POST http://YOUR_SERVER:3000/v1/orders \
  '{"user_id":"X","side":"buy","type":"limit","price":"0.55","size":"100","leverage":5}'

# Check orderbook (no auth needed)
curl http://YOUR_SERVER:3000/v1/markets/XRP-RLUSD-PERP/orderbook

# Get your orders
python3 tools/xrpl_auth.py --secret $SECRET \
  --request GET "http://YOUR_SERVER:3000/v1/orders?user_id=YOUR_ADDRESS"
```

Note: For `--request GET`, the tool signs the URI path. For `--request POST`, it signs the body.
The `user_id` field is auto-replaced with your authenticated address.
