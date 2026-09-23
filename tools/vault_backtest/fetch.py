#!/usr/bin/env -S uv run --no-project --script
# /// script
# requires-python = ">=3.11"
# dependencies = [
#     "requests>=2.32",
#     "pandas>=2.2",
#     "pyarrow>=15",
# ]
# ///
"""Fetch historical data for the vault backtest harness.

Produces three Parquet files under `research/backtest-data/`:

  binance_spot_{symbol}_1m_{from}_{to}.parquet
      1-minute OHLCV plus per-minute taker-buy volume (the kline's
      `taker_buy_base_volume` field). Drives mark/index price and the
      counterparty-flow proxy used in Tier A's bucket model.

  binance_perp_funding_{symbol}_{from}_{to}.parquet
      8-hour realised funding rates for the USDT-margined perpetual.
      Used as the headline funding series and to gauge how often our
      ±5 bps clamp would have bound.

  usd_borrow_daily_{from}_{to}.parquet
      Daily USD borrow rate proxy for the Delta One vault leg.
      Source: FRED SOFR + configurable spread (default 100 bps). The
      original plan called for Aave V3 USDC variable borrow APR, but
      DefiLlama paywalled the historical `chartLendBorrow` endpoint;
      SOFR + spread is the documented fallback (Aave USDC variable
      borrow has historically tracked SOFR + a modest DeFi premium).
      Override the spread via `--borrow-spread-bps`.

Idempotent: existing files are skipped unless --force is passed.
"""

from __future__ import annotations

import argparse
import sys
import time
from datetime import datetime, timezone
from pathlib import Path

import pandas as pd
import requests

BINANCE_SPOT = "https://api.binance.com/api/v3/klines"
BINANCE_PERP = "https://fapi.binance.com/fapi/v1/fundingRate"
FRED_SOFR_CSV = "https://fred.stlouisfed.org/graph/fredgraph.csv?id=SOFR"

DEFAULT_FROM = "2025-05-01"
DEFAULT_TO = "2026-05-01"
DEFAULT_SYMBOL = "XRPUSDT"
DEFAULT_OUT = Path("research/backtest-data")


def to_ms(date_str: str) -> int:
    """Parse YYYY-MM-DD as UTC midnight, return epoch ms."""
    return int(
        datetime.strptime(date_str, "%Y-%m-%d")
        .replace(tzinfo=timezone.utc)
        .timestamp()
        * 1000
    )


def cache_path(out_dir: Path, name: str, frm: str, to: str, symbol: str | None = None) -> Path:
    sym = f"_{symbol}" if symbol else ""
    return out_dir / f"{name}{sym}_{frm}_{to}.parquet"


def fetch_binance_spot_klines(symbol: str, frm_ms: int, to_ms_: int) -> pd.DataFrame:
    """Page through Binance spot 1m klines. Limit is 1000 rows per call."""
    rows: list[list] = []
    cursor = frm_ms
    session = requests.Session()
    print(f"  binance spot {symbol} 1m: {frm_ms} → {to_ms_}", file=sys.stderr)
    while cursor < to_ms_:
        params = {
            "symbol": symbol,
            "interval": "1m",
            "startTime": cursor,
            "endTime": to_ms_,
            "limit": 1000,
        }
        r = session.get(BINANCE_SPOT, params=params, timeout=30)
        if r.status_code == 429:
            print("  429 — backing off 30s", file=sys.stderr)
            time.sleep(30)
            continue
        r.raise_for_status()
        batch = r.json()
        if not batch:
            break
        rows.extend(batch)
        last_close = batch[-1][6]
        if last_close <= cursor:
            break
        cursor = last_close + 1
        if len(batch) < 1000:
            break
    cols = [
        "open_time", "open", "high", "low", "close", "volume",
        "close_time", "quote_volume", "trades",
        "taker_buy_base_volume", "taker_buy_quote_volume", "_ignore",
    ]
    df = pd.DataFrame(rows, columns=cols)
    df = df.drop(columns=["_ignore"])
    df["open_time"] = pd.to_datetime(df["open_time"], unit="ms", utc=True)
    df["close_time"] = pd.to_datetime(df["close_time"], unit="ms", utc=True)
    for c in ["open", "high", "low", "close", "volume", "quote_volume",
              "taker_buy_base_volume", "taker_buy_quote_volume"]:
        df[c] = df[c].astype(float)
    df["trades"] = df["trades"].astype("int64")
    return df


def fetch_binance_perp_funding(symbol: str, frm_ms: int, to_ms_: int) -> pd.DataFrame:
    """Page through Binance USDT-perp 8h funding rates. Limit 1000 per call."""
    rows: list[dict] = []
    cursor = frm_ms
    session = requests.Session()
    print(f"  binance perp {symbol} funding: {frm_ms} → {to_ms_}", file=sys.stderr)
    while cursor < to_ms_:
        params = {
            "symbol": symbol,
            "startTime": cursor,
            "endTime": to_ms_,
            "limit": 1000,
        }
        r = session.get(BINANCE_PERP, params=params, timeout=30)
        if r.status_code == 429:
            print("  429 — backing off 30s", file=sys.stderr)
            time.sleep(30)
            continue
        r.raise_for_status()
        batch = r.json()
        if not batch:
            break
        rows.extend(batch)
        last_ts = batch[-1]["fundingTime"]
        if last_ts <= cursor:
            break
        cursor = last_ts + 1
        if len(batch) < 1000:
            break
    df = pd.DataFrame(rows)
    if df.empty:
        return df
    df["fundingTime"] = pd.to_datetime(df["fundingTime"], unit="ms", utc=True)
    df["fundingRate"] = df["fundingRate"].astype(float)
    if "markPrice" in df.columns:
        df["markPrice"] = pd.to_numeric(df["markPrice"], errors="coerce")
    return df


def fetch_usd_borrow_proxy(frm_ms: int, to_ms_: int, spread_bps: float) -> pd.DataFrame:
    """Daily USD borrow rate proxy = FRED SOFR + spread (in bps).

    SOFR is published Mon-Fri (skips weekends/holidays), so we forward-fill
    over gaps to get a complete daily series across the window. SOFR values
    are reported as percent (e.g. 4.32 = 4.32% APR).
    """
    frm_dt = pd.Timestamp(frm_ms, unit="ms", tz="UTC").normalize()
    to_dt = pd.Timestamp(to_ms_, unit="ms", tz="UTC").normalize()
    print(f"  FRED SOFR + {spread_bps:.0f} bps spread", file=sys.stderr)
    r = requests.get(FRED_SOFR_CSV, timeout=60)
    r.raise_for_status()
    from io import StringIO
    df = pd.read_csv(StringIO(r.text))
    df.columns = [c.strip() for c in df.columns]
    df["observation_date"] = pd.to_datetime(df["observation_date"], utc=True)
    df["sofr_pct"] = pd.to_numeric(df["SOFR"], errors="coerce")
    df = df.drop(columns=["SOFR"])
    full_range = pd.date_range(frm_dt, to_dt, freq="D", tz="UTC", inclusive="left")
    out = pd.DataFrame({"date": full_range})
    out = out.merge(
        df.rename(columns={"observation_date": "date"}),
        on="date",
        how="left",
    )
    out["sofr_pct"] = out["sofr_pct"].ffill().bfill()
    out["borrow_apr_pct"] = out["sofr_pct"] + (spread_bps / 100.0)
    return out


def save_parquet(df: pd.DataFrame, path: Path, label: str) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    df.to_parquet(path, index=False)
    print(f"  wrote {label}: {len(df):,} rows → {path}", file=sys.stderr)


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__.split("\n")[0])
    ap.add_argument("--from", dest="frm", default=DEFAULT_FROM, help="YYYY-MM-DD inclusive (UTC)")
    ap.add_argument("--to", dest="to", default=DEFAULT_TO, help="YYYY-MM-DD exclusive (UTC)")
    ap.add_argument("--symbol", default=DEFAULT_SYMBOL)
    ap.add_argument("--out-dir", type=Path, default=DEFAULT_OUT)
    ap.add_argument("--force", action="store_true", help="refetch even if cached file exists")
    ap.add_argument(
        "--borrow-spread-bps",
        type=float,
        default=100.0,
        help="bps added to SOFR for the USD borrow rate proxy (default 100)",
    )
    ap.add_argument(
        "--only",
        choices=["spot", "funding", "borrow"],
        action="append",
        help="fetch only the named source(s); may repeat",
    )
    args = ap.parse_args()

    frm_ms = to_ms(args.frm)
    to_ms_ = to_ms(args.to)
    if to_ms_ <= frm_ms:
        ap.error("--to must be after --from")

    sources = set(args.only) if args.only else {"spot", "funding", "borrow"}
    print(
        f"vault-backtest fetcher: {args.frm} → {args.to} symbol={args.symbol} "
        f"sources={sorted(sources)}",
        file=sys.stderr,
    )

    failures: list[str] = []

    if "spot" in sources:
        path = cache_path(args.out_dir, "binance_spot_1m", args.frm, args.to, args.symbol)
        if path.exists() and not args.force:
            print(f"  skip {path.name} (exists; --force to override)", file=sys.stderr)
        else:
            try:
                df = fetch_binance_spot_klines(args.symbol, frm_ms, to_ms_)
                save_parquet(df, path, "binance spot 1m")
            except Exception as e:
                print(f"  FAIL spot: {e}", file=sys.stderr)
                failures.append("spot")

    if "funding" in sources:
        path = cache_path(args.out_dir, "binance_perp_funding", args.frm, args.to, args.symbol)
        if path.exists() and not args.force:
            print(f"  skip {path.name} (exists; --force to override)", file=sys.stderr)
        else:
            try:
                df = fetch_binance_perp_funding(args.symbol, frm_ms, to_ms_)
                save_parquet(df, path, "binance perp funding")
            except Exception as e:
                print(f"  FAIL funding: {e}", file=sys.stderr)
                failures.append("funding")

    if "borrow" in sources:
        path = cache_path(args.out_dir, "usd_borrow_daily", args.frm, args.to)
        if path.exists() and not args.force:
            print(f"  skip {path.name} (exists; --force to override)", file=sys.stderr)
        else:
            try:
                df = fetch_usd_borrow_proxy(frm_ms, to_ms_, args.borrow_spread_bps)
                save_parquet(df, path, "usd borrow daily (sofr + spread)")
            except Exception as e:
                print(f"  FAIL borrow: {e}", file=sys.stderr)
                failures.append("borrow")

    if failures:
        print(f"\nDONE with failures: {failures}", file=sys.stderr)
        return 1
    print("\nDONE — all sources fetched.", file=sys.stderr)
    return 0


if __name__ == "__main__":
    sys.exit(main())
