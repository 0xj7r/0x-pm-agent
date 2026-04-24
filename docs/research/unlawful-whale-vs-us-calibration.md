# Unlawful Whale-vs-Us Calibration Export

This export compares the live `unlawful-shear` tape against our local unlawful
paper sleeves window-by-window.

It is designed for calibration, not final truth accounting. The live whale side
comes from `data-api.polymarket.com/activity`. The local side comes from:

- sleeve envs in `whale-pair-exec/env/`
- current runtime journals in `whale-pair-exec/data/execution/paper/`
- current runtime SQLite stores in `whale-pair-exec/data/runtime/`
- signal snapshots from the same SQLite stores when the runtime has persisted
  them

The exporter prefers the crate-local runtime artifacts above because the sleeve
launcher runs from `whale-pair-exec/`. If only older root-level `data/...`
artifacts exist, the env-based path resolution still falls back cleanly.

If a local journal or order store is missing or empty, the exporter treats that
as "no local evidence" instead of failing.

The primary local truth surface is now:

1. current SQLite order stores
2. current SQLite signal snapshots
3. journals for extra suppression context

That order matters because the current live paper journals are large. By
default, journal reads are bounded automatically while the SQLite stores are
read in full.

## Run

From the repo root:

```bash
python3 scripts/export_unlawful_whale_vs_us.py
```

Default outputs:

- `reports/unlawful_calibration/unlawful_whale_vs_us.json`
- `reports/unlawful_calibration/unlawful_whale_vs_us.csv`

Optional range clamp:

```bash
python3 scripts/export_unlawful_whale_vs_us.py \
  --start-ms 1777010400000 \
  --end-ms 1777014000000
```

Optional offline whale input:

```bash
curl -sS 'https://data-api.polymarket.com/activity?user=0xb27bc932bf8110d8f78e55da7d5f0497a18b5b82&limit=500&offset=0' \
  > /tmp/unlawful-activity.json

python3 scripts/export_unlawful_whale_vs_us.py \
  --activity-json /tmp/unlawful-activity.json
```

Bound journal reads explicitly:

```bash
python3 scripts/export_unlawful_whale_vs_us.py \
  --journal-mode tail \
  --journal-tail-lines 20000
```

Or skip journals entirely and rely on order stores plus signal snapshots:

```bash
python3 scripts/export_unlawful_whale_vs_us.py \
  --journal-mode skip
```

## What The Export Includes

Each 5-minute window includes:

- `market_slug`
- `condition_id`
- `window_start_ms` / `window_start_iso`
- whale participation summary:
  - first timestamp
  - trade count
  - merge count
  - redeem count
  - rough trade notional from `usdcSize`
- per sleeve:
  - any orders submitted
  - any fills
  - submitted notional
  - filled notional
  - market ids touched
  - status counts
  - suppression reason summary from journal events
  - rejection reason summary from order-store reasons
  - signal snapshot summary if present:
    - mode
    - aggression tier
    - clip scale
    - price gap
    - BTC vol / trade-count snapshot
    - activity counts
    - gate reason summary

The top-level sleeve metadata also records:

- which current artifact paths were used
- whether journals were read in `full`, `tail:<n>`, `skip`, or `missing` mode
- how many signal snapshot rows were available

## Bucket Semantics

Per sleeve bucket:

- `both`: whale participated and the sleeve submitted at least one order
- `whale_only`: whale participated and the sleeve submitted nothing
- `us_only`: sleeve submitted at least one order and the whale did not
- `neither`: neither side participated in that window

There is also an `overall_bucket`:

- compares whale participation against "any of our sleeves participated"

## Practical Use

The main loop is:

1. Run paper sleeves.
2. Export comparison windows.
3. Sort/filter the CSV for `whale_only`.
4. Inspect which sleeve suppression reasons dominate those misses.
5. Tighten invariants only if they are clearly wrong.
6. Tune signal thresholds and aggression on the loose sleeve first.

The highest-signal rows are usually:

- `whale_only`: we missed a window the whale traded
- `us_only`: we traded something the whale did not

`both` is useful for checking whether our local notional, order count, and fill
behavior are converging toward the whale's tape.

When `signal_snapshots` exist, the fastest calibration workflow is:

1. Filter `whale_only`.
2. Look at the latest signal mode and gate reasons for the missed window.
3. Compare those against the whale's actual participation.
4. Decide whether the miss was:
   - a hard invariant we should keep
   - an overly strict admission threshold
   - an overly timid aggression / clip-scale decision
