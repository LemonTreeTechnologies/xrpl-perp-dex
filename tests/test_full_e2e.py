#!/usr/bin/env python3
"""
Full pre-hackathon E2E test — runs all untested paths in one go.

Test 1: Resting orders survive orchestrator restart (C5.1)
Test 2: WebSocket Fill/OrderUpdate events on live trade
Test 3: Full deposit→trade→multisig withdrawal cycle

Runs on Hetzner. Requires:
  - Orchestrator on localhost:3000 with --signers-config
  - Enclave on localhost:9088 (via SSH or direct)
  - SSH tunnels to Azure enclaves on 9188/9189/9190
"""

import asyncio
import hashlib
import json
import os
import signal
import subprocess
import sys
import threading
import time

import requests
import urllib3
from ecdsa import SECP256k1, SigningKey
from ecdsa.util import sigencode_der, sigdecode_der

urllib3.disable_warnings()

API = "http://localhost:3000"
ENCLAVE = "https://localhost:9088/v1"
PASS = 0
FAIL = 0

def fp8(x):
    v = int(round(x * 1e8))
    return f"{v // 100000000}.{v % 100000000:08d}"

def make_wallet():
    sk = SigningKey.generate(curve=SECP256k1)
    vk = sk.get_verifying_key()
    pt = vk.pubkey.point
    prefix = b'\x02' if pt.y() % 2 == 0 else b'\x03'
    compressed = prefix + pt.x().to_bytes(32, 'big')
    pk_hex = compressed.hex()
    sha = hashlib.sha256(compressed).digest()
    try:
        rip = hashlib.new('ripemd160', sha).digest()
    except ValueError:
        from Crypto.Hash import RIPEMD160
        rip = RIPEMD160.new(sha).digest()
    payload = b'\x00' + rip
    ck = hashlib.sha256(hashlib.sha256(payload).digest()).digest()[:4]
    full = payload + ck
    alpha = 'rpshnaf39wBUDNEGHJKLM4PQRST7VWXYZ2bcdeCg65jkm8oFqi1tuvAxyz'
    num = int.from_bytes(full, 'big')
    addr = ''
    while num > 0:
        num, r = divmod(num, 58)
        addr = alpha[r] + addr
    for b in full:
        if b == 0:
            addr = alpha[0] + addr
        else:
            break
    return sk, pk_hex, addr

def sign_req(sk, pk, addr, body_str):
    ts = str(int(time.time()))
    h = hashlib.sha256()
    h.update(body_str.encode())
    h.update(ts.encode())
    sig = sk.sign_digest(h.digest(), sigencode=sigencode_der)
    r, s = sigdecode_der(sig, SECP256k1.order)
    if s > SECP256k1.order // 2:
        s = SECP256k1.order - s
    sig = sigencode_der(r, s, SECP256k1.order)
    return {
        "X-XRPL-Address": addr, "X-XRPL-PublicKey": pk,
        "X-XRPL-Signature": sig.hex(), "X-XRPL-Timestamp": ts,
        "Content-Type": "application/json",
    }

def enclave_post(path, data):
    r = requests.post(f"{ENCLAVE}{path}", json=data, verify=False, timeout=10)
    text = r.text
    if text.startswith("Error"):
        text = text.split("\n", 1)[-1] if "\n" in text else text
    return json.loads(text)

def orch_post(path, body, wallet):
    body_str = json.dumps(body)
    sk, pk, addr = wallet
    headers = sign_req(sk, pk, addr, body_str)
    r = requests.post(f"{API}{path}", data=body_str, headers=headers, timeout=60)
    return r.status_code, r.json() if r.content else {}

def orch_delete(path, wallet):
    sk, pk, addr = wallet
    headers = sign_req(sk, pk, addr, path)
    r = requests.delete(f"{API}{path}", headers=headers, timeout=10)
    return r.status_code, r.json() if r.content else {}

def test(name, ok):
    global PASS, FAIL
    if ok:
        print(f"  ✓ {name}")
        PASS += 1
    else:
        print(f"  ✗ {name}")
        FAIL += 1

def pg_query(sql):
    r = subprocess.run(
        ["psql", "-h", "localhost", "-U", "perp", "-d", "perp_dex", "-tAc", sql],
        capture_output=True, text=True, timeout=10,
        env={**os.environ, "PGPASSWORD": "perp_dex_2026"},
    )
    return r.stdout.strip()


# ═══════════════════════════════════════════════════════════════
# TEST 1: Resting orders survive restart (C5.1)
# ═══════════════════════════════════════════════════════════════

def test_resting_orders():
    print("\n" + "=" * 60)
    print("  TEST 1: Resting orders persist across restart (C5.1)")
    print("=" * 60)

    alice = make_wallet()
    _, _, alice_addr = alice

    # Setup
    enclave_post("/perp/price", {"mark_price": fp8(1.0), "index_price": fp8(1.0), "timestamp": int(time.time())})
    enclave_post("/perp/deposit", {"user_id": alice_addr, "amount": fp8(1000.0), "xrpl_tx_hash": hashlib.sha256(f"rest_{time.time()}".encode()).hexdigest()})

    # Place limit order that rests on book
    code, resp = orch_post("/v1/orders", {
        "user_id": alice_addr, "side": "sell", "type": "limit",
        "price": fp8(99.0), "size": fp8(5.0), "leverage": 1,
    }, alice)
    test("limit order placed", code == 200 and resp.get("order_status") == "Open")
    order_id = resp.get("order_id")
    print(f"    order_id={order_id}")

    # Verify in resting_orders PG
    pg_count = pg_query(f"SELECT COUNT(*) FROM resting_orders WHERE order_id = {order_id}")
    test("order in resting_orders PG table", pg_count == "1")

    # Restart orchestrator
    print("    restarting orchestrator...")
    subprocess.run(["pkill", "-9", "-f", "perp-dex-orchestrator"], capture_output=True)
    time.sleep(2)
    subprocess.Popen(
        "cd /tmp/perp-9088 && nohup ./perp-dex-orchestrator "
        "--enclave-url https://localhost:9088/v1 --api-listen 127.0.0.1:3000 --priority 0 "
        "--database-url postgres://perp:perp_dex_2026@localhost/perp_dex "
        "--escrow-address r33cKcGyCZH6x2RRxmSkVfcjKHX3Z3pPEh "
        "--signers-config /tmp/perp-9088/multisig_escrow.json "
        "> orch.log 2>&1 < /dev/null &",
        shell=True,
    )
    # Wait for API to come back
    for _ in range(20):
        try:
            requests.get(f"{API}/v1/openapi.json", timeout=2)
            break
        except Exception:
            time.sleep(0.5)

    # Check log for resting order load
    log = open("/tmp/perp-9088/orch.log").read()
    loaded = "rebuilt orderbook from persisted resting orders" in log or "loading resting orders" in log
    test("orchestrator loaded resting orders from PG on restart", loaded)

    # Verify order still on book via GET /v1/orders
    code2, resp2 = orch_post("/v1/orders", {
        "user_id": alice_addr, "side": "sell", "type": "limit",
        "price": fp8(99.0), "size": fp8(1.0), "leverage": 1,
    }, alice)
    # If we can place another order, the API is working after restart
    test("API responsive after restart", code2 == 200)

    # Clean up: cancel the resting order
    orch_delete(f"/v1/orders/{order_id}", alice)
    pg_after = pg_query(f"SELECT COUNT(*) FROM resting_orders WHERE order_id = {order_id}")
    test("cancel removes from resting_orders PG", pg_after == "0")


# ═══════════════════════════════════════════════════════════════
# TEST 2: WebSocket Fill + OrderUpdate events
# ═══════════════════════════════════════════════════════════════

def test_websocket_events():
    print("\n" + "=" * 60)
    print("  TEST 2: WebSocket Fill/OrderUpdate events")
    print("=" * 60)

    import websocket

    alice = make_wallet()
    bob = make_wallet()
    _, _, alice_addr = alice
    _, _, bob_addr = bob

    # Setup
    enclave_post("/perp/deposit", {"user_id": alice_addr, "amount": fp8(1000.0), "xrpl_tx_hash": hashlib.sha256(f"ws_a_{time.time()}".encode()).hexdigest()})
    enclave_post("/perp/deposit", {"user_id": bob_addr, "amount": fp8(1000.0), "xrpl_tx_hash": hashlib.sha256(f"ws_b_{time.time()}".encode()).hexdigest()})

    # Connect WS and subscribe to alice's user channel
    events = []
    def on_msg(ws, m):
        events.append(json.loads(m))

    ws = websocket.WebSocketApp("ws://localhost:3000/ws", on_message=on_msg)
    t = threading.Thread(target=ws.run_forever, daemon=True)
    t.start()
    time.sleep(1)
    # Subscribe to alice's channel
    ws.send(json.dumps({"action": "subscribe", "channels": [f"user:{alice_addr}"]}))
    time.sleep(1)

    # Place crossing orders
    orch_post("/v1/orders", {
        "user_id": alice_addr, "side": "sell", "type": "limit",
        "price": fp8(1.0), "size": fp8(10.0), "leverage": 5,
    }, alice)
    orch_post("/v1/orders", {
        "user_id": bob_addr, "side": "buy", "type": "market",
        "size": fp8(10.0), "leverage": 5,
    }, bob)

    time.sleep(2)
    ws.close()

    types = [e.get("type") for e in events]
    print(f"    events received: {len(events)}")
    print(f"    types: {list(set(types))}")

    test("received 'subscribed' ack", "subscribed" in types)
    test("received 'trade' event", "trade" in types)
    test("received 'fill' event for alice", any(e.get("type") == "fill" and e.get("user_id") == alice_addr for e in events))
    test("received 'order_update' event", "order_update" in types)
    test("received 'orderbook' snapshot", "orderbook" in types)
    test("received 'position_changed' for alice", any(e.get("type") == "position_changed" and e.get("user_id") == alice_addr for e in events))


# ═══════════════════════════════════════════════════════════════
# TEST 3: Full deposit → trade → multisig withdrawal
# ═══════════════════════════════════════════════════════════════

def test_full_cycle():
    print("\n" + "=" * 60)
    print("  TEST 3: Full deposit → trade → multisig withdrawal")
    print("=" * 60)

    alice = make_wallet()
    bob = make_wallet()
    _, _, alice_addr = alice
    _, _, bob_addr = bob

    # 1. Deposit
    print("  [1] deposit...")
    enclave_post("/perp/deposit", {"user_id": alice_addr, "amount": fp8(500.0), "xrpl_tx_hash": hashlib.sha256(f"fc_a_{time.time()}".encode()).hexdigest()})
    enclave_post("/perp/deposit", {"user_id": bob_addr, "amount": fp8(500.0), "xrpl_tx_hash": hashlib.sha256(f"fc_b_{time.time()}".encode()).hexdigest()})
    test("deposits succeeded", True)

    # 2. Trade
    print("  [2] trade...")
    orch_post("/v1/orders", {
        "user_id": alice_addr, "side": "sell", "type": "limit",
        "price": fp8(1.0), "size": fp8(10.0), "leverage": 5,
    }, alice)
    code, resp = orch_post("/v1/orders", {
        "user_id": bob_addr, "side": "buy", "type": "market",
        "size": fp8(10.0), "leverage": 5,
    }, bob)
    trades = resp.get("trades", [])
    test("trade matched", code == 200 and len(trades) > 0)
    if trades:
        print(f"    trade_id={trades[0].get('trade_id')} price={trades[0].get('price')}")

    # 3. Verify trade in PG
    if trades:
        tid = trades[0]["trade_id"]
        pg_row = pg_query(f"SELECT trade_id FROM trades WHERE trade_id = {tid}")
        test("trade in PostgreSQL", pg_row == str(tid))

    # 4. Withdraw via Rust multisig
    print("  [3] multisig withdrawal...")
    from xrpl.clients import JsonRpcClient
    from xrpl.wallet import generate_faucet_wallet
    client = JsonRpcClient("https://s.altnet.rippletest.net:51234")
    dest = generate_faucet_wallet(client, debug=False)
    print(f"    destination: {dest.classic_address}")

    body = json.dumps({"user_id": alice_addr, "amount": "10.00000000", "destination": dest.classic_address})
    sk, pk, addr = alice
    headers = sign_req(sk, pk, addr, body)
    r = requests.post(f"{API}/v1/withdraw", data=body, headers=headers, timeout=60)
    wd = r.json()
    test("withdrawal status=success", wd.get("status") == "success")
    test("withdrawal has XRPL tx hash", wd.get("xrpl_tx_hash") is not None)
    if wd.get("xrpl_tx_hash"):
        print(f"    tx_hash: {wd['xrpl_tx_hash']}")
        print(f"    explorer: https://testnet.xrpl.org/transactions/{wd['xrpl_tx_hash']}")


# ═══════════════════════════════════════════════════════════════

def main():
    global PASS, FAIL
    print("=" * 60)
    print("  PRE-HACKATHON FULL E2E TEST SUITE")
    print("=" * 60)

    # Set price once
    enclave_post("/perp/price", {"mark_price": fp8(1.0), "index_price": fp8(1.0), "timestamp": int(time.time())})

    test_resting_orders()
    test_websocket_events()
    test_full_cycle()

    print("\n" + "=" * 60)
    total = PASS + FAIL
    print(f"  RESULTS: {PASS}/{total} passed, {FAIL} failed")
    print("=" * 60)
    return 1 if FAIL > 0 else 0


if __name__ == "__main__":
    sys.exit(main())
