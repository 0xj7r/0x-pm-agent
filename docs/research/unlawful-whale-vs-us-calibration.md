# Unlawful Whale-vs-Us Calibration Export

This export compares the live `unlawful-shear` tape against our local unlawful
paper sleeves window-by-window.

It is designed for calibration, not final truth accounting. The live whale side
comes from `data-api.polymarket.com/activity`. The local side comes from:

- sleeve envs in `polymarket-exec/env/`
- current runtime journals in `polymarket-exec/data/execution/paper/`
- current runtime SQLite stores in `polymarket-exec/data/runtime/`
- signal snapshots from the same SQLite stores when the runtime has persisted
  them

The exporter prefers the crate-local runtime artifacts above because the sleeve
launcher runs from `polymarket-exec/`. If only older root-level `data/...`
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

The JSON also includes a top-level `qa_summary` for dashboards and reports. It
is the preferred high-level read surface; `windows` remains the drill-down
surface.

`qa_summary` includes:

- `overall_bucket_counts`: total `whale_only`, `us_only`, `both`, and `neither`
  windows across all sleeves
- `whale.buy_levels`: weighted average whale buy levels by outcome, by
  cheap/expensive role, and by early/mid/late phase
- `buckets.<bucket>.windows`: compact window refs for `whale_only`, `us_only`,
  `both`, and `neither`, with whale buy levels plus local sleeve metrics
- `buckets.<bucket>.windows[].microstructure_context`: averaged latest signal
  context across sleeves, including price gap, spreads, top-3 depth, BTC vol /
  returns, activity counts, clip scale, book freshness, and gate reasons
- `sleeves.<name>`: aggregate order, fill, cancel, status, rejection,
  suppression, and signal-gate metrics for each sleeve
- `sleeves.<name>.pnl_metrics`: currently marks PnL unavailable when the local
  `order-store.sqlite` only exposes `orders` and `signal_snapshots`

The CSV mirrors the main row-level fields and now includes per-sleeve
`filled_order_count`, `canceled_order_count`, `submitted_qty`, `filled_qty`, and
`status_counts` for quick spreadsheet triage.

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

## Dashboard / Report Read Path

Dashboard and report consumers should read the export in this order:

1. Use `qa_summary.overall_bucket_counts` for the headline miss/overtrade/match
   counts.
2. Use `qa_summary.whale.buy_levels.by_role` and `.by_outcome` for the whale
   reference buy levels. These are weighted by size, not simple window averages.
3. Use `qa_summary.buckets.whale_only.windows` as the miss queue. For each row,
   show the whale cheap/expensive legs next to `microstructure_context` and the
   sleeve suppression / gate reasons.
4. Use `qa_summary.buckets.us_only.windows` as the overtrade queue. Prioritize
   rows with fills, high submitted notional, stale books, or thin depth.
5. Use `qa_summary.buckets.both.windows` to check convergence: order count,
   filled notional, fill rate, cancel count, BTC vol, activity, and whether the
   latest signal mode/aggression matches the whale's observed buy geometry.
6. Use `qa_summary.sleeves.<name>` for sleeve-level health: fill rate by
   notional, cancel rate by order count, top status counts, rejection reasons,
   suppression reasons, and signal-gate reasons.

Do not infer PnL from submitted or filled notional. The current local stores do
not persist realized/unrealized PnL in the calibration export source schema, so
reports should display PnL as unavailable unless a future store adds explicit
PnL fields.

When `signal_snapshots` exist, the fastest calibration workflow is:

1. Filter `whale_only`.
2. Look at the latest signal mode and gate reasons for the missed window.
3. Compare those against the whale's actual participation.
4. Decide whether the miss was:
   - a hard invariant we should keep
   - an overly strict admission threshold
   - an overly timid aggression / clip-scale decision
