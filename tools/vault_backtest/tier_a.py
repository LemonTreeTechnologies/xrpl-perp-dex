#!/usr/bin/env python3
"""Tier A of docs/vault-backtest-plan.md — are the advertised vault APY bands plausible?

Advertised on the UI: MM 12-18 %, Delta-Neutral 15-25 %, Delta One 20-35 %.

This deliberately does NOT produce a single headline APY for MM/DN, and the
reason is the main finding rather than a limitation. A first implementation did
produce one — and it scaled exactly linearly with the assumed quote size,
reporting 383 % APY with 0 % drawdown. That is an arithmetic identity
(size x edge x number of crossed minutes), not a market model: our quote is
capped on 100 % of the minutes where price crosses our band, because the median
taker flow in such a minute is ~65,000 XRP against the ~150 XRP we would show.
A model whose answer is its own input assumption answers nothing.

So Tier A reports what the data can settle without a fill model:

1. ADDRESSABLE FLOW — the taker volume that actually crossed a band at each
   half-spread, and therefore the gross spread available to a maker who caught
   all of it. Measured from 1-minute highs/lows and the taker-buy split.
2. REQUIRED CAPTURE — inverted: what share of that flow the vault must win to
   hit the advertised band on $100k. This converts an unanswerable question
   into a requirement the team can judge.
3. DELTA ONE — settled outright, because it needs no fill model at all:
   realised funding carry x leverage, less borrow on the levered part.

Caveat that applies to (1) and (2): the flow is BINANCE's. Our venue has its own,
much smaller, book. The required share being tiny says the edge exists in the
market, not that our order book will see it.
"""
from __future__ import annotations

import argparse
import glob
import sys
from pathlib import Path

import pandas as pd

HALF_SPREADS_BPS = (10, 15, 25, 40, 60)
NOTIONAL = 100_000.0
CAPTURE_EFF = 0.85


def load(d: Path):
    def one(prefix):
        hits = sorted(glob.glob(str(d / (prefix + "*.parquet"))))
        if not hits:
            raise SystemExit(f"no {prefix}*.parquet in {d} — run fetch.py first")
        return pd.read_parquet(hits[-1])

    spot = one("binance_spot_1m")
    spot["open_time"] = pd.to_datetime(spot["open_time"], utc=True)
    spot = spot.set_index("open_time").sort_index()
    funding = one("binance_perp_funding")
    borrow = one("usd_borrow")
    return spot, funding, borrow


def addressable(spot: pd.DataFrame):
    """Gross spread available at each half-spread, from measured price action."""
    tb = spot["taker_buy_base_volume"]
    tsell = spot["volume"] - tb
    mark = spot["close"]
    out = {}
    for bps in HALF_SPREADS_BPS:
        h = bps / 1e4
        lifted = spot["high"] > spot["open"] * (1 + h)
        hit = spot["low"] < spot["open"] * (1 - h)
        vol_usd = ((tb.where(lifted, 0.0) + tsell.where(hit, 0.0)) * mark).sum()
        out[bps] = (vol_usd, vol_usd * h)
    return out


def main():
    ap = argparse.ArgumentParser(description=__doc__,
                                 formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--data-dir", required=True)
    args = ap.parse_args()
    spot, funding, borrow = load(Path(args.data_dir))

    print(f"window: {spot.index[0].date()} -> {spot.index[-1].date()}  "
          f"({len(spot):,} 1m bars, {len(funding):,} funding periods)\n")

    flow = addressable(spot)
    print("1. ADDRESSABLE FLOW (Binance XRPUSDT, not our venue)")
    print(f"{'half-spread':>12} {'taker flow crossing it':>24} {'gross spread if we took ALL':>30}")
    print("-" * 70)
    for bps, (usd, gross) in flow.items():
        print(f"{bps:>10}bp {usd:>24,.0f} {gross:>30,.0f}")

    print("\n2. REQUIRED CAPTURE SHARE on $100k notional")
    print(f"{'half-spread':>12} {'for 12% APY':>14} {'for 18% APY':>14} {'for 25% APY':>14}")
    print("-" * 58)
    for bps, (_usd, gross) in flow.items():
        print(f"{bps:>10}bp {12_000/gross*100:>13.3f}% "
              f"{18_000/gross*100:>13.3f}% {25_000/gross*100:>13.3f}%")

    r = funding["fundingRate"].astype(float)
    bcol = [c for c in borrow.columns if any(k in c.lower() for k in ("rate", "apr", "borrow"))]
    b = borrow[bcol[0]].astype(float).mean() if bcol else 0.06
    if b > 1:
        b /= 100.0
    realised = r.sum()

    print("\n3. DELTA ONE — no fill model involved")
    print(f"   realised funding carry over the window : {realised*100:>8.2f}%")
    print(f"   funding per 8h: mean {r.mean()*1e4:+.3f} bp, median {r.median()*1e4:+.2f} bp, "
          f"q95 {r.quantile(0.95)*1e4:+.2f} bp")
    print(f"   borrow proxy (SOFR + 100bp), mean      : {b*100:>8.2f}% APR")
    print(f"\n{'leverage':>9} {'capture':>8} {'realised APY':>14}   {'funding needed for 20%':>24}")
    print("-" * 62)
    for lev in (1.5, 2.0, 3.0):
        apy = (realised * lev * CAPTURE_EFF - b * (lev - 1)) * 100
        need = (0.20 + b * (lev - 1)) / (lev * CAPTURE_EFF)
        print(f"{lev:>9} {CAPTURE_EFF:>8} {apy:>13.2f}%   "
              f"{need*100:>10.1f}% = {need/(3*365)*1e4:>5.2f} bp/8h")
    return 0


if __name__ == "__main__":
    sys.exit(main())
