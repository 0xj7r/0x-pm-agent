# Phase 1: Data Feasibility

Date: 2026-04-07
Branch: weather-market (worktree agent-a7277bb5)

## Question 1: Polymarket historical price-over-time per market

**Verdict: WORKING.** The CLOB `prices-history` endpoint returns trade-by-trade granularity for closed weather markets, no auth required.

Test market: `will-the-highest-temperature-in-nyc-be-48f-or-below-on-april-1`
- conditionId: `0x0f4eedfcbc5049d033ffe2449f98d96671bb6577848bf1cffcfc74e7d9cec677`
- gamma id: `531966`
- YES clobTokenId: `99182038211161711252276162175387732765816152711342244116896020076390095726142`
- Lifetime: 2025-03-28 to 2025-04-01 (~3.5 days)
- Volume: $12,488

Working call:
```
curl -sS "https://clob.polymarket.com/prices-history?market=99182038211161711252276162175387732765816152711342244116896020076390095726142&startTs=1743120000&endTs=1743548400&fidelity=1"
```

Response shape:
```json
{"history": [{"t": 1743187446, "p": 0.05}, {"t": 1743187505, "p": 0.05}, ...]}
```

Returned **5,259 price points** over the market lifetime. `t` = unix seconds, `p` = mid (or last-trade) price.

Notes:
- `interval=max` (no startTs/endTs) returned 0 points. You MUST pass `startTs`/`endTs` for closed markets.
- `fidelity=1` is a hint to the server; the server returns approximately one point per minute when there is trading activity, plus every individual trade. Effectively trade-by-trade.
- No rate limit hit at single-call scale; will need to throttle when batching across thousands of markets.
- Both YES and NO token IDs are available in the Gamma `clobTokenIds` field on each market.

## Question 2: Enumerating resolved weather markets

**Verdict: WORKING but slow.** Gamma `markets` endpoint with date filters paginates correctly. Weather markets are densely packed in the offset range 5000-25000 over a 12-month window.

Working call (paginated):
```
curl -sS "https://gamma-api.polymarket.com/markets?limit=500&offset=0&closed=true&active=false&end_date_min=2025-04-01&end_date_max=2026-04-07"
```

Filter trick that worked: `end_date_min`/`end_date_max` (snake_case). Camel-case `endDateMin` is silently ignored.

Important header: must send `User-Agent` (without it, Python urllib gets HTTP 403 from the Gamma edge).

### Inventory (12 months ending 2026-04-07)

Scanned 30,000 markets, filtered for `temperature` in question or `highest-temperature`/`lowest-temperature` in slug:

| Bucket | Count |
| --- | --- |
| Total weather markets found | **1,523** |
| London | 749 |
| New York / NYC | 489 + 250 = **739** |
| Dubai | 7 |
| Other | 28 |

City structure: each city, each day, has ~7 brackets (e.g. `48f or below`, `49-50f`, `51-52f`, `53-54f`, `55-56f`, `57-58f`, `59f or higher`). So 1,523 markets equals roughly **220 city-days** in 12 months. London and NYC almost daily; Dubai sporadic.

The strategy doc claims "493 active weather markets including 325+ daily temperature markets across 20+ cities." Real picture: in the **closed/resolved** archive that we can backtest, it is essentially **NYC + London** with a handful of Dubai. Miami and Buenos Aires (currently in `shared/constants.WEATHER_CITIES`) do NOT appear in the resolved archive at meaningful volume.

Per-market lifetime is short (the test market was only 3.5 days). Volumes are typically $5K-$15K. This caps our position size and means slippage matters a lot.

### Gating math

- **220 city-days * ~7 brackets = ~1,500 market observations.** That is the universe.
- Useful train/test split: 154 city-days for in-sample, 66 for out-of-sample.
- For the latency-arb / forecast-vs-price hypothesis the unit of edge measurement is one trade per market. After requiring confidence and edge filters, the actual trade count drops further.
- Conclusion: **the dataset is small but workable for Phase 3.** Anything that requires millions of observations is dead.

## Question 3: Open-Meteo historical forecast availability

**Verdict: PARTIAL.** We can pull a single forecast value per target date, but we **cannot** retrieve "what model X said at run-time T1 vs T2 about target date D."

Working call:
```
curl -sS "https://historical-forecast-api.open-meteo.com/v1/forecast?latitude=40.7128&longitude=-74.0060&start_date=2025-03-28&end_date=2025-04-01&daily=temperature_2m_max&models=gfs_seamless&temperature_unit=fahrenheit&timezone=America/New_York"
```

Returns one daily max per target date per model. Lookback verified back to 2025-03-28; Open-Meteo claims 1940 archive availability for ECMWF/GFS.

Ensemble API also works with `past_days`:
```
curl -sS "https://ensemble-api.open-meteo.com/v1/ensemble?latitude=40.7128&longitude=-74.0060&hourly=temperature_2m&models=gfs_seamless&past_days=10&forecast_days=1&temperature_unit=fahrenheit&timezone=America/New_York"
```

Returns 31 ensemble members, one curve per member, hourly. **But:** these are "the historical record of the run that was issued at the conventional lead time for that target date." There is no parameter to ask "give me the 12z run of GFS issued on 2025-03-29 forecasting 2025-04-01." Each target date gets one curated value/curve.

**Implications:**
- Hypothesis 1 (forecast vs price baseline): testable. Use the historical-forecast-api value as "ground-truth model forecast at last responsible lead time" and compare against price snapshots.
- Hypothesis 2 (forecast-update lag): **dead at the Open-Meteo data tier.** To actually time stamp "GFS run T published a new value, market repriced N hours later," we'd need raw GRIB archives from NOAA NOMADS (heavyweight) or a third-party that timestamps run issuance.
- Hypothesis 3 (ensemble disagreement → tail mispricing): testable using ensemble API spread per target date.
- Hypothesis 4 (climatology fade): testable using ERA5 archive for the trailing 7 days.
- Hypothesis 5 (cross-market consistency): testable from Polymarket alone, no forecast data needed.
- Hypothesis 6 (exact-bracket mispricing): testable, comparing each bracket's price to the implied probability from ensemble distribution at one snapshot.
- Hypothesis 7 (late-window pricing): partially testable. We can measure "what did the market do in the last 6 hours before close" against the **single available forecast value**. We cannot test "did the market repriceto an updated forecast" because we don't have the updates.

Open-Meteo also tags responses with `temperature_2m_max` in degC (or F) only at one decimal of precision, vs Polymarket settlement which is per-degree-F integer Wunderground LaGuardia. There will be a small unit-conversion mismatch worth flagging.

## Other notes worth keeping

- The Polymarket markets settle on **Wunderground LaGuardia (KLGA)** for NYC, not Central Park. This matters: KLGA can run 1-3°F off the canonical NYC temperature. Open-Meteo's lat/lon for NYC is downtown Manhattan; should switch to KLGA's coordinates (40.7773, -73.8726) for backfill or there will be a systematic offset that masquerades as edge.
- London: settlement station listed in market description should be checked too (likely Heathrow or St James's Park).
- 2% Polymarket fee is on **winnings only**. A YES bought at 0.10 winning at 1.00 = $0.90 raw, $0.882 net of fee. Losing trades pay no fee. This makes the long-tail bracket trades (low entry price, high payout) less attractive than they appear.
- Min order size: 5 shares ($5 notional at price 1.00). Tick size: 0.001. So very wide-tail brackets may be uninvestable in size.

## Decision

Phase 1 gate **PASSED** for hypotheses 1, 3, 5, 6 (and partial 7). Hypotheses 2 and 4-with-leadtime are **dead** because we cannot reconstruct historical forecast run snapshots from free APIs. Proceed to Phase 2 backfill targeting the testable hypotheses.
