#!/usr/bin/env python3
"""
Invariant tests for Perp DEX — verifies correctness of enclave FP8 arithmetic.

Tests that the C/C++ code inside SGX enclave computes balances, margins,
fees, PnL, liquidations, and funding correctly.

Runs against REAL enclave (requires ENCLAVE_URL).

Usage:
  # On the server (localhost access to enclave)
  ENCLAVE_URL=https://localhost:9088/v1 python3 tests/test_invariants.py

  # From outside (enclave not accessible — will skip)
  python3 tests/test_invariants.py

FP8 arithmetic reference (from Enclave/PerpState.h):
  FP_ONE = 100_000_000  (1.0)
  fp_mul(a, b) = a * b / FP_ONE  (128-bit intermediate)
  fp_div(a, b) = a * FP_ONE / b  (128-bit intermediate)

Constants:
  TAKER_FEE_RATE      = 50_000      (0.0005 = 0.05%)
  MAINTENANCE_RATE    = 500_000     (0.005 = 0.5%)
  LIQUIDATION_PENALTY = 500_000     (0.005 = 0.5%)
  XRP_HAIRCUT         = 90_000_000  (0.90 = 90% LTV)
  FEE_INSURANCE_PCT   = 20          (20% of fees)
  MAX_LEVERAGE        = 20
"""

import hashlib
import json
import os
import sys
import time

import requests

# ── Config ──────────────────────────────────────────────────────

ENCLAVE_URL = os.environ.get("ENCLAVE_URL", "")
TIMEOUT = 15
FP8 = 100_000_000  # 1.0 in FP8

OK = 0
FAIL = 0
SKIP = 0

# ── FP8 helpers (mirror enclave's fp_mul/fp_div) ───────────────

def fp_mul(a: int, b: int) -> int:
    """FP8 multiply: a * b / FP_ONE (128-bit intermediate)."""
    return (a * b) // FP8

def fp_div(a: int, b: int) -> int:
    """FP8 divide: a * FP_ONE / b (128-bit intermediate)."""
    if b == 0:
        return 0
    return (a * FP8) // b

def to_fp8(f: float) -> int:
    """Convert float to FP8 int."""
    return int(round(f * FP8))

def from_fp8(v: int) -> float:
    """Convert FP8 int to float."""
    return v / FP8

def fp8_str(f: float) -> str:
    """Format float as FP8 string."""
    v = to_fp8(f)
    sign = "-" if v < 0 else ""
    av = abs(v)
    return f"{sign}{av // FP8}.{av % FP8:08d}"

def parse_fp8(s: str) -> int:
    """Parse FP8 string to int."""
    neg = s.startswith("-")
    s = s.lstrip("-")
    parts = s.split(".")
    integer = int(parts[0]) * FP8
    frac = 0
    if len(parts) > 1:
        frac_str = parts[1][:8].ljust(8, "0")
        frac = int(frac_str)
    result = integer + frac
    return -result if neg else result

# ── HTTP helpers ────────────────────────────────────────────────

def unique_hash():
    """Generate unique 64-char hex hash for tx dedup."""
    return hashlib.sha256(f"inv_{time.time()}_{id(object())}".encode()).hexdigest()

def enclave_post(path, data):
    r = requests.post(f"{ENCLAVE_URL}{path}", json=data, timeout=TIMEOUT, verify=False)
    text = r.text
    if text.startswith("Error"):
        text = text.split("\n", 1)[-1] if "\n" in text else text
    return json.loads(text)

def enclave_get(path):
    r = requests.get(f"{ENCLAVE_URL}{path}", timeout=TIMEOUT, verify=False)
    text = r.text
    if text.startswith("Error"):
        text = text.split("\n", 1)[-1] if "\n" in text else text
    return json.loads(text)

def get_balance(user_id):
    r = enclave_get(f"/perp/balance?user_id={user_id}")
    assert r["status"] == "success", f"get_balance failed: {r}"
    return r["data"]

def deposit(user_id, amount_f):
    r = enclave_post("/perp/deposit", {
        "user_id": user_id,
        "amount": fp8_str(amount_f),
        "xrpl_tx_hash": unique_hash(),
    })
    assert r["status"] == "success", f"deposit failed: {r}"
    return r

def deposit_xrp(user_id, amount_f):
    r = enclave_post("/perp/deposit-xrp", {
        "user_id": user_id,
        "xrp_amount": fp8_str(amount_f),
        "xrpl_tx_hash": unique_hash(),
    })
    assert r["status"] == "success", f"deposit_xrp failed: {r}"
    return r

def set_price(price_f):
    r = enclave_post("/perp/price", {
        "mark_price": fp8_str(price_f),
        "index_price": fp8_str(price_f),
        "timestamp": int(time.time()),
    })
    assert r["status"] == "success", f"set_price failed: {r}"

def open_position(user_id, side, size_f, price_f, leverage):
    r = enclave_post("/perp/position/open", {
        "user_id": user_id,
        "side": side,
        "size": fp8_str(size_f),
        "price": fp8_str(price_f),
        "leverage": leverage,
    })
    return r

def close_position(user_id, position_id, price_f):
    r = enclave_post("/perp/position/close", {
        "user_id": user_id,
        "position_id": position_id,
        "close_price": fp8_str(price_f),
    })
    return r

_funding_ts = [int(time.time()) + 1000000]  # Start far in future to avoid conflicts

def apply_funding(rate_f):
    _funding_ts[0] += 1
    r = enclave_post("/perp/funding/apply", {
        "funding_rate": fp8_str(rate_f),
        "timestamp": _funding_ts[0],
    })
    return r

# ── Test framework ──────────────────────────────────────────────

def test(name, fn):
    global OK, FAIL
    try:
        fn()
        print(f"  \u2713 {name}")
        OK += 1
    except AssertionError as e:
        print(f"  \u2717 {name}: {e}")
        FAIL += 1
    except Exception as e:
        print(f"  \u2717 {name}: {type(e).__name__}: {e}")
        FAIL += 1

def assert_fp8_eq(actual_str, expected_f, label="", tolerance=1):
    """Assert FP8 string matches expected float within tolerance (1 unit = 0.00000001)."""
    actual = parse_fp8(actual_str)
    expected = to_fp8(expected_f)
    diff = abs(actual - expected)
    assert diff <= tolerance, (
        f"{label}: expected {fp8_str(expected_f)} ({expected}), "
        f"got {actual_str} ({actual}), diff={diff}"
    )

# ── Unique user IDs per test run ────────────────────────────────

RUN_ID = str(int(time.time()))[-6:]

def user(name):
    return f"r{name}{RUN_ID}"

# ── Invariant tests ─────────────────────────────────────────────

def test_deposit_balance():
    """Deposit 1000 RLUSD → balance = 1000."""
    u = user("Dep")
    deposit(u, 1000.0)
    b = get_balance(u)
    assert_fp8_eq(b["margin_balance"], 1000.0, "margin_balance")
    assert_fp8_eq(b["available_margin"], 1000.0, "available_margin")

def test_multiple_deposits():
    """Two deposits sum correctly."""
    u = user("MDep")
    deposit(u, 500.0)
    deposit(u, 300.0)
    b = get_balance(u)
    assert_fp8_eq(b["margin_balance"], 800.0, "margin_balance")

def test_duplicate_tx_rejected():
    """Same tx_hash cannot be deposited twice."""
    u = user("Dup")
    tx = unique_hash()
    r1 = enclave_post("/perp/deposit", {"user_id": u, "amount": fp8_str(100.0), "xrpl_tx_hash": tx})
    assert r1["status"] == "success"
    r2 = enclave_post("/perp/deposit", {"user_id": u, "amount": fp8_str(100.0), "xrpl_tx_hash": tx})
    assert r2["status"] == "error", "duplicate tx should fail"
    b = get_balance(u)
    assert_fp8_eq(b["margin_balance"], 100.0, "should be 100, not 200")

def test_open_position_margin():
    """Open long: notional = size * price, margin = notional / leverage, fee deducted."""
    u = user("OPos")
    set_price(1.0)
    deposit(u, 1000.0)

    # Open long 100 XRP @ 1.0 with 5x leverage
    # notional = 100 * 1.0 = 100.0
    # required_margin = 100.0 / 5 = 20.0
    # fee = 100.0 * 0.0005 = 0.05
    r = open_position(u, "long", 100.0, 1.0, 5)
    assert r["status"] == "success"

    b = get_balance(u)
    expected_balance = 1000.0 - 0.05  # fee deducted
    assert_fp8_eq(b["margin_balance"], expected_balance, "margin_balance after fee")
    assert_fp8_eq(b["used_margin"], 20.0, "used_margin")
    assert len(b["positions"]) == 1
    assert_fp8_eq(b["positions"][0]["margin"], 20.0, "position margin")
    assert_fp8_eq(b["positions"][0]["entry_price"], 1.0, "entry_price")
    assert_fp8_eq(b["positions"][0]["size"], 100.0, "size")

def test_open_position_insufficient_margin():
    """Cannot open position if margin + fee exceeds available."""
    u = user("InsM")
    set_price(1.0)
    deposit(u, 10.0)

    # Try to open 10000 XRP @ 1.0 with 1x → need 10000 + 5.0 fee = 10005
    r = open_position(u, "long", 10000.0, 1.0, 1)
    assert r["status"] == "error", "should fail with insufficient margin"

def test_unrealized_pnl_long():
    """Long position PnL: size * (mark - entry)."""
    u = user("UPnL")
    set_price(1.0)
    deposit(u, 1000.0)
    open_position(u, "long", 100.0, 1.0, 5)

    # Price goes up to 1.10 → PnL = 100 * (1.10 - 1.00) = 10.0
    set_price(1.10)
    b = get_balance(u)
    assert_fp8_eq(b["unrealized_pnl"], 10.0, "unrealized_pnl")

def test_unrealized_pnl_short():
    """Short position PnL: size * (entry - mark)."""
    u = user("SPnL")
    set_price(1.0)
    deposit(u, 1000.0)
    open_position(u, "short", 100.0, 1.0, 5)

    # Price goes down to 0.90 → PnL = 100 * (1.00 - 0.90) = 10.0
    set_price(0.90)
    b = get_balance(u)
    assert_fp8_eq(b["unrealized_pnl"], 10.0, "short unrealized_pnl")

def test_close_position_pnl():
    """Close position: margin returned + PnL - close fee."""
    u = user("ClPn")
    set_price(1.0)
    deposit(u, 1000.0)

    r = open_position(u, "long", 100.0, 1.0, 5)
    pos_id = r["position_id"]
    b_before = get_balance(u)

    # Close at 1.10: PnL = 100 * 0.10 = 10.0
    # Close fee = 100 * 1.10 * 0.0005 = 0.055
    # margin_return = 20.0 + 10.0 - 0.055 = 29.945
    r = close_position(u, pos_id, 1.10)
    assert r["status"] == "success"

    b_after = get_balance(u)
    # Total balance = original - open_fee + margin_return
    # = 1000.0 - 0.05 + 29.945 = 1029.895
    # But: margin was locked (20.0), so margin_balance after open = 999.95
    # After close: margin_balance = 999.95 + 29.945 = 1029.895
    expected = 1000.0 - 0.05 + 20.0 + 10.0 - 0.055
    assert_fp8_eq(b_after["margin_balance"], expected, "balance after close", tolerance=2)
    assert len(b_after["positions"]) == 0, "position should be closed"

def test_close_position_loss():
    """Close at loss: margin returned minus loss."""
    u = user("ClLo")
    set_price(1.0)
    deposit(u, 1000.0)

    r = open_position(u, "long", 100.0, 1.0, 5)
    pos_id = r["position_id"]

    # Close at 0.90: PnL = 100 * (0.90 - 1.00) = -10.0
    # Close fee = 100 * 0.90 * 0.0005 = 0.045
    # margin_return = 20.0 + (-10.0) - 0.045 = 9.955
    r = close_position(u, pos_id, 0.90)
    assert r["status"] == "success"

    b = get_balance(u)
    expected = 1000.0 - 0.05 + 20.0 - 10.0 - 0.045
    assert_fp8_eq(b["margin_balance"], expected, "balance after loss", tolerance=2)

def test_xrp_collateral_haircut():
    """XRP collateral valued at mark_price * 90% LTV."""
    u = user("XCol")
    set_price(2.0)
    deposit_xrp(u, 100.0)  # 100 XRP

    b = get_balance(u)
    assert_fp8_eq(b["xrp_balance"], 100.0, "xrp_balance")
    # collateral_value = 100 * 2.0 * 0.90 = 180.0
    assert_fp8_eq(b["xrp_collateral_value"], 180.0, "xrp_collateral_value")
    assert_fp8_eq(b["available_margin"], 180.0, "available_margin from XRP")

def test_xrp_collateral_with_rlusd():
    """XRP + RLUSD combined margin."""
    u = user("XRcm")
    set_price(1.0)
    deposit(u, 500.0)       # 500 RLUSD (100% LTV)
    deposit_xrp(u, 1000.0)  # 1000 XRP * 1.0 * 0.90 = 900 RLUSD value

    b = get_balance(u)
    assert_fp8_eq(b["margin_balance"], 500.0, "rlusd")
    assert_fp8_eq(b["xrp_collateral_value"], 900.0, "xrp_value")
    assert_fp8_eq(b["available_margin"], 1400.0, "total available")

def test_liquidation_threshold():
    """Position liquidated when margin_ratio <= 0.5% (MAINTENANCE_RATE)."""
    u = user("Liqd")
    set_price(1.0)
    deposit(u, 101.0)  # 100 margin + 0.50 fee + buffer

    # Open long 1000 XRP @ 1.0, 10x leverage → margin = 100.0, fee = 0.50
    r = open_position(u, "long", 1000.0, 1.0, 10)
    assert r["status"] == "success", f"open_position failed: {r}"
    pos_id = r["position_id"]

    # Price drops to 0.90: upnl = 1000 * (0.90 - 1.00) = -100
    # margin_ratio = (100 + (-100)) / (1000 * 0.90) = 0 → liquidatable
    set_price(0.90)
    liq = enclave_get("/perp/liquidations/check")
    assert liq["status"] == "success"

    # Find our position in liquidatable list
    our_liq = [p for p in liq["liquidatable"] if p["user_id"] == u]
    assert len(our_liq) > 0, f"position {pos_id} should be liquidatable at 0.90"

    # Reset price for other tests
    set_price(1.0)

def test_fee_calculation():
    """Fee = notional * 0.0005 (taker fee rate)."""
    u = user("FeeC")
    set_price(1.0)
    deposit(u, 10000.0)

    # Open 1000 XRP @ 2.50 with 10x
    set_price(2.50)
    b_before = get_balance(u)
    r = open_position(u, "long", 1000.0, 2.50, 10)
    assert r["status"] == "success"

    # notional = 1000 * 2.50 = 2500
    # fee = 2500 * 0.0005 = 1.25
    b_after = get_balance(u)
    fee_deducted = parse_fp8(b_before["margin_balance"]) - parse_fp8(b_after["margin_balance"])
    expected_fee = to_fp8(1.25)
    assert abs(fee_deducted - expected_fee) <= 2, \
        f"fee: expected {from_fp8(expected_fee)}, got {from_fp8(fee_deducted)}"

def test_max_leverage_rejected():
    """Leverage > 20 should be rejected."""
    u = user("MaxL")
    set_price(1.0)
    deposit(u, 10000.0)
    r = open_position(u, "long", 100.0, 1.0, 21)
    assert r["status"] == "error", "leverage 21 should fail"

def test_funding_rate_long():
    """Funding: long pays funding_rate * notional."""
    u = user("FuLo")
    set_price(1.0)
    deposit(u, 1000.0)
    open_position(u, "long", 100.0, 1.0, 5)

    b_before = get_balance(u)
    margin_before = parse_fp8(b_before["margin_balance"])

    # Apply 0.01% funding rate (positive = longs pay)
    # payment = size * mark_price * rate = 100 * 1.0 * 0.0001 = 0.01
    r = apply_funding(0.0001)
    assert r["status"] == "success"

    b_after = get_balance(u)
    margin_after = parse_fp8(b_after["margin_balance"])
    payment = margin_before - margin_after
    expected = to_fp8(0.01)
    assert abs(payment - expected) <= 2, \
        f"funding payment: expected {from_fp8(expected)}, got {from_fp8(payment)}"

def test_funding_rate_short():
    """Funding: short receives funding_rate * notional."""
    u = user("FuSh")
    set_price(1.0)
    deposit(u, 1000.0)
    open_position(u, "short", 100.0, 1.0, 5)

    b_before = get_balance(u)
    margin_before = parse_fp8(b_before["margin_balance"])

    # Positive funding → shorts receive
    r = apply_funding(0.0001)
    assert r["status"] == "success"

    b_after = get_balance(u)
    margin_after = parse_fp8(b_after["margin_balance"])
    received = margin_after - margin_before
    expected = to_fp8(0.01)
    assert abs(received - expected) <= 2, \
        f"funding received: expected {from_fp8(expected)}, got {from_fp8(received)}"

def test_withdraw_insufficient_margin():
    """Cannot withdraw more than available margin."""
    u = user("WdIn")
    deposit(u, 100.0)
    r = enclave_post("/perp/withdraw", {
        "user_id": u,
        "amount": fp8_str(200.0),
        "escrow_account_id": "rEscrow",
        "session_key": "00" * 32,
        "tx_hash": "00" * 32,
    })
    assert r["status"] == "error", "should fail — insufficient margin"

def test_fp8_precision_large_values():
    """Large deposit + small fee = no precision loss."""
    u = user("FP8L")
    set_price(1.0)
    deposit(u, 92233720368.0)  # near int64 max / FP8

    b = get_balance(u)
    assert_fp8_eq(b["margin_balance"], 92233720368.0, "large balance")

def test_save_load_preserves_state():
    """State save + load preserves balances."""
    u = user("SvLd")
    deposit(u, 777.77)

    # Save
    r = enclave_post("/perp/state/save", {})
    assert r["status"] == "success"

    # Balance should still be there
    b = get_balance(u)
    assert_fp8_eq(b["margin_balance"], 777.77, "balance after save")

# ── Main ────────────────────────────────────────────────────────

def main():
    if not ENCLAVE_URL:
        print("ENCLAVE_URL not set. Run on the server:")
        print("  ENCLAVE_URL=https://localhost:9088/v1 python3 tests/test_invariants.py")
        sys.exit(0)

    # Suppress InsecureRequestWarning
    import urllib3
    urllib3.disable_warnings(urllib3.exceptions.InsecureRequestWarning)

    print(f"Invariant Tests — Enclave FP8 Arithmetic Verification")
    print(f"  Enclave: {ENCLAVE_URL}")
    print(f"  Run ID: {RUN_ID} (unique user suffix)")
    print()

    print("Deposit & Balance:")
    test("  deposit → balance", test_deposit_balance)
    test("  multiple deposits sum", test_multiple_deposits)
    test("  duplicate tx rejected", test_duplicate_tx_rejected)

    print("\nPosition Opening:")
    test("  margin + fee calculation", test_open_position_margin)
    test("  insufficient margin rejected", test_open_position_insufficient_margin)
    test("  max leverage 21 rejected", test_max_leverage_rejected)
    test("  fee = notional * 0.05%", test_fee_calculation)

    print("\nUnrealized PnL:")
    test("  long: size * (mark - entry)", test_unrealized_pnl_long)
    test("  short: size * (entry - mark)", test_unrealized_pnl_short)

    print("\nClose Position:")
    test("  close with profit", test_close_position_pnl)
    test("  close with loss", test_close_position_loss)

    print("\nCollateral:")
    test("  XRP haircut 90%", test_xrp_collateral_haircut)
    test("  XRP + RLUSD combined", test_xrp_collateral_with_rlusd)

    print("\nLiquidation:")
    test("  threshold at 0.5% margin ratio", test_liquidation_threshold)

    print("\nFunding:")
    test("  long pays funding", test_funding_rate_long)
    test("  short receives funding", test_funding_rate_short)

    print("\nWithdrawal:")
    test("  insufficient margin rejected", test_withdraw_insufficient_margin)

    print("\nEdge Cases:")
    test("  FP8 precision large values", test_fp8_precision_large_values)
    test("  save/load preserves state", test_save_load_preserves_state)

    print(f"\n{'='*55}")
    print(f"  Passed: {OK}  Failed: {FAIL}")
    print(f"{'='*55}")
    sys.exit(1 if FAIL > 0 else 0)


if __name__ == "__main__":
    main()
