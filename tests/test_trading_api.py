#!/usr/bin/env python3
"""
Self-test for Perp DEX Trading API.

Usage:
  python3 test_trading_api.py
  PERP_DEX_API=http://your-server:3000 python3 test_trading_api.py
"""

import os
import sys
import requests

BASE = sys.argv[1] if len(sys.argv) > 1 else os.environ.get("PERP_DEX_API", "http://localhost:3000")
MARKET = "XRP-RLUSD-PERP"
OK = 0
FAIL = 0


def test(name, fn):
    global OK, FAIL
    try:
        fn()
        print(f"  ✓ {name}")
        OK += 1
    except Exception as e:
        print(f"  ✗ {name}: {e}")
        FAIL += 1


def get(path):
    r = requests.get(f"{BASE}{path}", timeout=10)
    r.raise_for_status()
    return r.json()


def post(path, data):
    r = requests.post(f"{BASE}{path}", json=data, timeout=10)
    r.raise_for_status()
    return r.json()


def delete(path):
    r = requests.delete(f"{BASE}{path}", timeout=10)
    r.raise_for_status()
    return r.json()


def test_openapi():
    r = get("/v1/openapi.json")
    assert r["openapi"] == "3.0.3"
    assert "Perp DEX" in r["info"]["title"]


def test_ticker():
    r = get(f"/v1/markets/{MARKET}/ticker")
    assert r["status"] == "success"


def test_orderbook():
    r = get(f"/v1/markets/{MARKET}/orderbook")
    assert r["status"] == "success"
    assert "bids" in r
    assert "asks" in r


def test_trades():
    r = get(f"/v1/markets/{MARKET}/trades")
    assert r["status"] == "success"


def test_limit_buy():
    r = post("/v1/orders", {
        "user_id": "rTestUser1",
        "side": "buy",
        "type": "limit",
        "price": "0.50000000",
        "size": "200.00000000",
        "leverage": 5,
    })
    assert r["status"] == "success"
    assert r["order_status"] == "Open"


def test_limit_sell():
    r = post("/v1/orders", {
        "user_id": "rTestUser2",
        "side": "sell",
        "type": "limit",
        "price": "0.60000000",
        "size": "150.00000000",
        "leverage": 3,
    })
    assert r["status"] == "success"


def test_orderbook_has_orders():
    r = get(f"/v1/markets/{MARKET}/orderbook")
    assert len(r["bids"]) > 0, "no bids"
    assert len(r["asks"]) > 0, "no asks"


def test_user_orders():
    r = get("/v1/orders?user_id=rTestUser1")
    assert r["status"] == "success"
    assert len(r["orders"]) > 0, "no orders for rTestUser1"


def test_market_buy_match():
    r = post("/v1/orders", {
        "user_id": "rTestUser3",
        "side": "buy",
        "type": "market",
        "size": "50.00000000",
        "leverage": 5,
    })
    assert r["status"] == "success"
    assert len(r["trades"]) > 0, "no trades (expected match with sell @0.60)"


def test_trades_after_match():
    r = get(f"/v1/markets/{MARKET}/trades")
    assert len(r["trades"]) > 0, "no trades in history"


def test_cancel_all():
    r = delete("/v1/orders?user_id=rTestUser1")
    assert r["status"] == "success"


def main():
    print(f"Testing {BASE}\n")

    test("GET /v1/openapi.json", test_openapi)
    test("GET ticker", test_ticker)
    test("GET orderbook", test_orderbook)
    test("GET trades", test_trades)
    test("POST limit buy", test_limit_buy)
    test("POST limit sell", test_limit_sell)
    test("Orderbook has orders", test_orderbook_has_orders)
    test("GET user orders", test_user_orders)
    test("POST market buy (match)", test_market_buy_match)
    test("Trades after match", test_trades_after_match)
    test("DELETE cancel all", test_cancel_all)

    print(f"\n{'='*40}")
    print(f"  Passed: {OK}  Failed: {FAIL}")
    print(f"{'='*40}")
    sys.exit(1 if FAIL > 0 else 0)


if __name__ == "__main__":
    main()
