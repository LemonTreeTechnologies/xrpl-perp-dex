# Vault revenue sources measured over the trailing twelve months

**Scope.** Tier A of `docs/vault-backtest-plan.md`. A closed-form pass over
historical market data measuring what each vault strategy's revenue source paid
between **2025-09-23 and 2026-09-23**.

**Status.** This is a measurement, not a backtest of deployed infrastructure, and
not a forecast. It reports what the data supports, what it contradicts, and what
it cannot settle either way.

---

## 1. The question

`docs/vault-design-spec.md` defines three vault strategies. Each monetises a
different revenue source, and this document measures **what that source actually
paid** over the window:

| vault | revenue source measured here |
|---|---|
| Market-making (MM) | spread captured on taker flow crossing the quoted band |
| Delta-Neutral (DN) | that spread, plus funding accruing on the resulting inventory |
| Delta One | funding carry on a levered position, less borrow on the levered part |

Yield bands of 12–18 / 15–25 / 20–35 % circulate on the product's UI cards. They
are not stated anywhere in this repository, and this document does not establish
their origin or treat them as a specification. They appear below only as
comparison points, because a measurement of a revenue source is easier to read
next to a number someone expects.

## 2. Data

| series | source | rows | notes |
|---|---|---|---|
| XRPUSDT spot, 1-minute OHLCV | Binance `api/v3/klines` | 525,601 | includes `taker_buy_base_volume`, so taker flow splits by side |
| XRPUSDT perpetual funding | Binance `fapi/v1/fundingRate` | 1,096 | 8-hour periods; 365 × 3 = 1,095, plus boundary |
| USD borrow proxy | FRED SOFR + 100 bps | 365 | daily; a reference rate, not a quote available to us |

Fetched by `tools/vault_backtest/fetch.py` (merged in PR #7).

Reference magnitudes over the window, for calibration:

- Total XRPUSDT spot volume: **$74.6 bn** (daily average $205 M)
- Taker-buy share of volume: 48.4 %
- Funding: mean **+0.017 bp** per 8h, median **+0.08 bp**, q05 **−1.37 bp**,
  q95 **+1.00 bp**, 54.3 % of periods positive, extremes ±0.44 bp… ±4.4 bp
- Borrow proxy: **4.74 %** APR mean

Binance is used as the proxy for both flow and funding because our own venue has
no comparable history. §5 covers what that substitution does and does not permit.

## 3. Method

### 3.1 Delta One — closed form, no fill model

Delta One earns funding on a levered position and pays borrow on the levered
part. Both terms are directly measurable, so no assumption about our order book
enters:

```
APY = Σ(funding_rate) × leverage × capture_efficiency
      − borrow_apr × (leverage − 1)
```

`Σ(funding_rate)` is the arithmetic sum of all 1,096 realised 8-hour rates, which
is the carry an always-on position would have collected. `capture_efficiency`
(0.85 headline) discounts slippage on entering and rolling. Leverage is swept at
1.5 / 2.0 / 3.0.

Inverted, the same formula gives the funding level a given target requires:

```
funding_needed = (target + borrow_apr × (leverage − 1)) / (leverage × capture_efficiency)
```

### 3.2 MM and DN — why no headline number is produced

A market-making yield is `filled_volume × mark × edge`. `filled_volume` requires
a model of what fraction of taker flow a resting quote captures. **Tier A does
not have one, and this document does not invent one.** The reason is measured:

At a 10 bp half-spread, price crosses the quoted band in 14 % of minutes, and
the median taker flow in such a minute is ~65,000 XRP. A vault quoting three
levels against a 50 XRP inventory cap shows ~150 XRP. The quote is therefore
size-capped in **100 %** of the minutes where it would trade. Any revenue figure
computed this way is `quoted_size × edge × count(minutes)` — an identity in the
assumed size, not a property of the market.

What the data *can* settle is the inverse. Define **addressable flow** at
half-spread `h` as the taker volume in minutes where price actually crossed the
band:

```
addressable(h) = Σ over minutes [ taker_buy  where high > open × (1 + h) ]
               + Σ over minutes [ taker_sell where low  < open × (1 − h) ]
               taker_sell = volume − taker_buy_base_volume
```

The gross spread available to a maker capturing all of it is
`addressable(h) × h`, and the share a $100k vault must win for a target APY is
`target × 100,000 / (addressable(h) × h)`.

This replaces an unanswerable question with a requirement that can be judged.

### 3.3 Not modelled, deliberately

- **Adverse selection.** No cost is charged for being filled on the wrong side of
  a move. Every MM/DN figure here is therefore an **upper bound**.
- **Inventory mark-to-market.** Not carried, for the same reason the MM headline
  is omitted: without a fill model there is no inventory path to mark.
- **Maker rebate.** Held at **0 bp** throughout, which is what the venue has
  committed (taker 5 bp, maker not rebated). A 1 bp rebate changes required
  capture by roughly a tenth of itself and alters no conclusion.
- **Our venue's funding clamp** (±5 bp per 8h in `compute_funding_rate`) is not
  applied, because **0.0 %** of periods in this window would have reached it. The
  clamp is not what suppressed the carry.

## 4. Results

### 4.1 Delta One

Realised carry over the window: **+0.19 %**. Borrow proxy: **4.74 %** APR.

| leverage | realised APY | funding required for 20 % | required, per 8h |
|---|---|---|---|
| 1.5× | **−2.13 %** | 17.5 % | 1.60 bp |
| 2.0× | **−4.42 %** | 14.6 % | 1.33 bp |
| 3.0× | **−9.01 %** | 11.6 % | 1.06 bp |

Measured funding for comparison: median **0.08 bp** per 8h, 95th percentile
**1.00 bp**.

The outcome is negative at every leverage tested, and becomes more negative as
leverage rises, because borrow scales with `leverage − 1` while the carry did
not. Reaching the band's lower bound would have required funding to hold above
its own 95th percentile in essentially every period for twelve months.

### 4.2 Delta-Neutral

DN's revenue over MM is funding accruing on inventory left by market-making —
the same series that summed to 0.19 %. Under this method DN and MM differ by
less than 0.1 % of notional over the window. A DN premium over MM of several
points has no measurable source in this data.

### 4.3 Market-making

| half-spread | addressable flow | gross spread if all captured | share needed for 12 % | for 18 % |
|---|---|---|---|---|
| 10 bp | $30.4 bn | $30.4 M | 0.039 % | 0.059 % |
| 15 bp | $21.2 bn | $31.7 M | 0.038 % | 0.057 % |
| 25 bp | $11.4 bn | $28.4 M | 0.042 % | 0.063 % |
| 40 bp | $5.5 bn | $21.9 M | 0.055 % | 0.082 % |
| 60 bp | $2.7 bn | $16.0 M | 0.075 % | 0.112 % |

The required share is 4–11 parts per ten thousand of the flow that crosses the
band. **This neither confirms nor refutes the MM band.** It establishes that the
spread available in the market is not the binding constraint at $100k notional.
The binding quantity is the flow reaching *our* order book, which this data
cannot speak to.

## 5. What this method cannot establish

- **Binance flow is not our flow.** §4.3 measures an external venue. A required
  capture share of 0.04 % of Binance's book says nothing about volume on ours.
- **Binance funding is a proxy for ours.** Our venue derives funding from its own
  book. A thinner book gives noisier funding, not systematically larger funding,
  and the clamp bounds it at ±5 bp — but this is an argument, not a measurement.
- **One window, one regime.** Funding was flat and near-symmetric across these
  twelve months. Sustained-basis regimes occur and would change §4.1 materially.
  The result is that the Delta One carry was absent in this window, not that it
  is absent in every window.
- **Delta One models absent infrastructure.** Spot RLUSD/XRP and lending are not
  built. The borrow series is a reference rate, not a quote obtainable against
  XRP collateral on XRPL today.
- **MM/DN figures are upper bounds**, per §3.3.

## 6. Method note — a discarded first model

The first implementation did produce a headline: **383 % APY at 0 % drawdown**.
It is recorded here because the way it failed is part of the method.

Three signals identified it as wrong rather than favourable: revenue scaled
exactly linearly with the assumed quote size (×4 and ×10 for inventory caps of
200 and 500 against 50); DN matched MM to four significant figures; and maximum
drawdown was 0.0 % in every parameter set — impossible for a strategy carrying
inventory.

The causes were the size cap binding on 100 % of crossed minutes (§3.2) and the
absence of inventory mark-to-market, which left the equity curve monotonically
increasing. That model is not part of the published tool.

## 7. Reproduction

```bash
python3 tools/vault_backtest/fetch.py \
    --from 2025-09-23 --to 2026-09-23 --out-dir <data-dir>
python3 tools/vault_backtest/tier_a.py --data-dir <data-dir>
```

Requires `pandas` and `requests`. The fetcher writes parquet and is idempotent;
`tier_a.py` reads the newest matching files and prints every table in §4.
