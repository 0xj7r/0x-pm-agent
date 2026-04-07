# Weather strategy edge research

Throwaway research scripts. Not production code. Used to validate whether
Polymarket weather markets have an exploitable edge before investing in
production strategy work.

## Headline finding

The current `strategies/weather.py` (per-bucket ensemble vote) is the wrong
abstraction. The right unit of analysis is the `(city, target_date)` partition
of ~7 mutually exclusive temperature buckets, with fair value computed via
calibrated Normal CDF integration over each bucket.

OOS backtest on n=205 partitions (2025-04-01..2025-07-16, NYC + London):

* NYC: 60.9% win rate at thr=0.05, +27.6% ROI on deployed capital
* London: 37.7% win rate, +3.3% ROI at 12h before close, +29% ROI at 48h
* Calibrated CDF crushes ensemble vote on log-loss (1.58 vs 5.24)
* Edge robust 6h to 72h before close, catastrophic loss in last 3h (stale-quote
  settlement artifact)

## Pipeline

Run in this order:

1. `weather_edge.py`: builds the partition universe, fetches OpenMeteo, computes
   the calibrated CDF and ensemble-vote baselines, scores both with log loss
   and hit rate. Caches OM data to `weather_om.json`.
2. `weather_pnl.py`: pulls CLOB price history for all ~1500 markets in the
   partition universe (caches to `/tmp/weather_clob.json`, ~112MB), runs an
   OOS PnL backtest at a fixed snapshot time and threshold.
3. `weather_sensitivity.py`: sweeps snapshot time (1h to 72h before close) and
   threshold (0.03 to 0.12), reports per-cell ROI. Use this to pick the
   operating point.
4. `weather_montecarlo.py`: bootstraps from the empirical OOS partition returns
   and projects forward 1/3/6/12 months for various starting bankrolls. Two
   scenarios: idealized (no liquidity cap) and capped at $500/partition.

## Inputs / caches

* `weather_partitions.json`: pre-grouped market partitions from Gamma. Built by
  the inline pagination loop in `weather_pnl.py`. Committed for reproducibility.
* `weather_om.json`: OpenMeteo historical forecast cache. Committed.
* `/tmp/weather_clob.json`: 112MB CLOB price history cache. NOT committed
  (gitignore size). Rebuilt by `weather_pnl.py` on first run (5-10 min).

## What is NOT here yet

The de-risking work that should happen before this becomes a production
strategy:

* Walk-forward CV (current is single 70/30 time split)
* Cross-city generalization test (train NYC, test London, vice versa)
* Larger sample (full 1500-market universe vs current 205)
* Real book depth measurement via CLOB `/book` endpoint
* Realistic slippage measurement from tick-to-tick price moves
* Multi-snapshot trading scheme comparison
* Stressed Monte Carlo (block bootstrap, catastrophic event injection,
  shrinkage prior on win rate)

See conversation history with the assistant for the full plan.

## Important details that bit us during development

* Polymarket Gamma API: filter with `closed=true` only. Do NOT add
  `active=false`. Closed weather markets have `active=true, closed=true`.
* NYC settles on Wunderground LaGuardia (KLGA), not Manhattan. Use coordinates
  40.7773, -73.8726, not Manhattan downtown. The 1-3F offset masquerades as
  edge.
* Slug bands have a 4-digit form: `between-5253f` means 52-53F. The hyphenated
  form (`between-52-53f`) only appears for 3-digit numbers. Both must parse.
* CLOB `/prices-history` requires explicit `startTs` and `endTs` for closed
  markets. `interval=max` returns 0 points.
* Last 6h before market close: avoid. Last-trade prices become stale during
  settlement and produce -100% ROI artifacts in any backtest snapshot.
