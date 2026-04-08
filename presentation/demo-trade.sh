#!/bin/bash
# Live demo: place limit sell, then market buy that matches.
# Use during pitch to show real trading flow.
#
# Requires: python3 with ecdsa, requests, pycryptodome
# Generates 2 wallets and runs trade against api-perp.ph18.io

set -e
API="${API:-https://api-perp.ph18.io}"
PYTHON_AUTH="$(dirname "$0")/../tools/xrpl_auth.py"

if [ ! -f "$PYTHON_AUTH" ]; then
    echo "ERROR: $PYTHON_AUTH not found"
    exit 1
fi

echo "════════════════════════════════════════════"
echo "  Perp DEX Live Demo"
echo "  $API"
echo "════════════════════════════════════════════"
echo ""

# Generate 2 wallets
extract_json() {
    python3 -c "
import sys, json
text = sys.stdin.read()
# Extract first JSON object
start = text.find('{')
depth = 0
end = start
for i in range(start, len(text)):
    if text[i] == '{': depth += 1
    elif text[i] == '}':
        depth -= 1
        if depth == 0:
            end = i + 1
            break
print(json.loads(text[start:end])['$1'])
"
}

echo "→ Generating wallet A (seller)..."
SELLER_OUT=$(python3 "$PYTHON_AUTH" --generate)
SELLER_SECRET=$(echo "$SELLER_OUT" | extract_json seed)
SELLER_ADDR=$(echo "$SELLER_OUT" | extract_json address)
echo "  $SELLER_ADDR"

echo "→ Generating wallet B (buyer)..."
BUYER_OUT=$(python3 "$PYTHON_AUTH" --generate)
BUYER_SECRET=$(echo "$BUYER_OUT" | extract_json seed)
BUYER_ADDR=$(echo "$BUYER_OUT" | extract_json address)
echo "  $BUYER_ADDR"
echo ""

# Wallet A places limit sell
echo "→ Wallet A: limit SELL 100 @ 1.31000000 (5x leverage)"
python3 "$PYTHON_AUTH" --secret "$SELLER_SECRET" \
  --request POST "$API/v1/orders" \
  "{\"user_id\":\"$SELLER_ADDR\",\"side\":\"sell\",\"type\":\"limit\",\"price\":\"1.31000000\",\"size\":\"100.00000000\",\"leverage\":5}"
echo ""

sleep 1

# Wallet B places market buy that matches
echo "→ Wallet B: MARKET BUY 50 (matches sell)"
python3 "$PYTHON_AUTH" --secret "$BUYER_SECRET" \
  --request POST "$API/v1/orders" \
  "{\"user_id\":\"$BUYER_ADDR\",\"side\":\"buy\",\"type\":\"market\",\"size\":\"50.00000000\",\"leverage\":5}"
echo ""

# Show recent trades
echo "→ Recent trades (public endpoint):"
curl -s "$API/v1/markets/XRP-RLUSD-PERP/trades" | python3 -m json.tool
echo ""

echo "════════════════════════════════════════════"
echo "  ✓ Trade matched. Check WebSocket for events."
echo "════════════════════════════════════════════"
