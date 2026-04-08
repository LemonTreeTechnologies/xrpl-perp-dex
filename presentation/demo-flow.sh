#!/bin/bash
# Self-paced demo flow for recording (asciinema)
# Each step has a sleep for narration timing

DIR="$(dirname "$0")"
API="https://api-perp.ph18.io"

cecho() { echo -e "\033[1;36m$1\033[0m"; sleep 1; }
cmd() { echo -e "\033[0;32m$ $1\033[0m"; sleep 1; }

clear
cecho "═══════════════════════════════════════════════"
cecho "  Perp DEX on XRPL — Live Demo"
cecho "  https://api-perp.ph18.io"
cecho "═══════════════════════════════════════════════"
echo ""
sleep 2

cecho "[1/5] List markets"
cmd "curl https://api-perp.ph18.io/v1/markets"
curl -s "$API/v1/markets" | python3 -m json.tool
sleep 3
echo ""

cecho "[2/5] Current funding rate"
cmd "curl https://api-perp.ph18.io/v1/markets/XRP-RLUSD-PERP/funding"
curl -s "$API/v1/markets/XRP-RLUSD-PERP/funding" | python3 -m json.tool
sleep 3
echo ""

cecho "[3/5] Place orders (live trade)"
bash "$DIR/demo-trade.sh" 2>&1 | grep -A2 "→\|trades\|trade_id\|═" | head -40
sleep 3
echo ""

cecho "[4/5] DCAP attestation (verify enclave)"
cmd "curl -X POST https://api-perp.ph18.io/v1/attestation/quote -d '{\"user_data\":\"0xdeadbeef\"}'"
curl -s -X POST "$API/v1/attestation/quote" \
  -H "Content-Type: application/json" \
  -d '{"user_data":"0xdeadbeef"}' | python3 -m json.tool | head -10
sleep 3
echo ""

cecho "[5/5] OpenAPI spec"
cmd "curl https://api-perp.ph18.io/v1/openapi.json | jq '.paths | keys'"
curl -s "$API/v1/openapi.json" | python3 -c "import sys,json; print(json.dumps(list(json.load(sys.stdin)['paths'].keys()), indent=2))"
sleep 3
echo ""

cecho "═══════════════════════════════════════════════"
cecho "  ✓ Live, audited, open source"
cecho "  github.com/77ph/xrpl-perp-dex"
cecho "═══════════════════════════════════════════════"
sleep 2
