# Backtest Pipeline README

This is the canonical workflow for BTC 5m historical backtests in
`polymarket-agent`. The pipeline is deliberately split into two contracts:

1. prepared data must be reusable and validated before any strategy claims;
2. replay/backtest runs must be deterministic and artifact-driven.

Do not tune strategy parameters from a run until the data-preparation and
replay gates below pass.

## Architecture

The backtest path has four layers:

| Layer | File | Responsibility |
|---|---|---|
| Fleet orchestration | `scripts/backtest_aws_fleet.sh` | Date fan-out, S3 sync, prepared-cache manifesting, AWS Batch submission, local smoke runs |
| Rust replay runner | `polymarket-exec/src/bin/backtest_runner.rs` | Parses prepared Parquet, builds windows, runs deterministic replay, writes metrics/manifests |
| Data-quality checks | `polymarket-exec/src/replay/data_quality.rs` | Rejects or warns on timestamp drift, book gaps, invalid prices/sizes, missing books, crossed books, duplicate sequences |
| Walk-forward reducer | `polymarket-exec/scripts/cross_day_walk_forward.py` | Consumes completed run artifacts only; builds train/test/holdout diagnostics without reprocessing market data |

Reuse:

- Reuse the prepared shard cache and `.backtest_manifest.json` for reruns.
- Reuse `backtest_runner` for replay/accounting/fill simulation.
- Reuse `cross_day_walk_forward.py` for cross-day validation from immutable
  artifacts.

Do not replace this with ad hoc local data munging. If a data shape is wrong,
fix the preparation/validation contract before running strategies.

## Prepared Data Contract

Prepared inputs are day shards under:

```text
s3://pm-research-data-prod/processed/v=1/market_type=btc_5m/dt=YYYY-MM-DD/
```

The local cache default is:

```text
.cache/backtest-events/market_type=btc_5m/dt=YYYY-MM-DD/
```

Each prepared day shard is tracked by:

```text
.backtest_manifest.json
```

The manifest is a v2 cache contract. It records:

- `schema_version`
- `date`
- `market_filter`
- `s3_prefix`
- `require_btc_tick`
- `require_market_meta`
- `required_event_types`
- `event_counts`
- `event_bytes`
- `latest_input_mtime`
- `ready`
- `generated_at`

A shard is reusable only when the manifest matches the current source prefix,
market filter, and required-data flags. If the manifest is missing or stale but
the files are already present, the script refreshes the manifest instead of
blindly downloading/reprocessing everything.

## Data Validation Gates

Run cache validation before PnL/fill runs:

```bash
cargo build -p polymarket-exec --release --bin backtest_runner

scripts/backtest_aws_fleet.sh 2026-04-13 2026-04-19 \
  --strategy-profile polymarket-exec/config/strategies/whale_bonereaper_strategy.live.yaml \
  --market-filter btc_5m \
  --mode local \
  --sync-only \
  --validate-cache \
  --cache-root .cache/backtest-events \
  --run-id data_prep_validate_20260413_20260419
```

`--sync-only` prepares or reuses cached shards and exits before strategy replay.
`--validate-cache` additionally invokes `backtest_runner --dry-run` for each
prepared shard. This validates parser/schema compatibility, required event
types, metadata joins, and replay data-quality checks without producing PnL.

Hard stop:

- If validation exits non-zero, do not tune strategy knobs.
- If any replay manifest contains data-quality `reject` rows, do not present
  that segment as live-relevant performance.
- If required BTC tick or market metadata is missing, either repair the
  prepared shard or explicitly disable the requirement for a smoke run only.

Default data-quality thresholds in `DataQualityConfig`:

| Check | Warn | Reject |
|---|---:|---:|
| Event timestamp drift | `> 2s` | `> 5m` |
| Book-event gap | `> 10s` | `> 60s` |
| Crossed-book samples | n/a | `> 100` samples |

Additional reject surfaces include empty input, no book events, no trade events,
missing expected asset books, invalid prices/sizes, duplicate sequence keys, and
non-monotonic received timestamps.

## Replay Semantics

`backtest_runner` replays prepared events chronologically. The current model is:

- Event clock: event `received_ns` ordering from prepared Parquet.
- Window clock: deterministic replay windows from the runner manifest.
- Book model: in-memory reconstructed per-asset bid/ask state from snapshots
  and deltas.
- Fill model: `--fill-config` controls latency preset; `--fill-quality`
  controls queue/fill realism.
- Accounting: replay portfolio state is written to `metrics_summary.json` and
  `manifest.json`; cash-compounded runs must stay sequential.
- Resolution handling: uses available market metadata/resolution events in the
  prepared shard; missing metadata is a validation failure by default.

Fill-quality policy:

| `--fill-quality` | Meaning | Claim status |
|---|---|---|
| `optimistic` | Useful for debugging upper-bound behavior | Exploratory only |
| `base` | Default realistic queue regime | Comparable, not final |
| `conservative` | Haircut queue fills after book depth is consumed | Preferred for live-relevant claims |

If a result depends on `optimistic`, label it exploratory.

## Proper Run Order

### 1. Validate prepared data

Use the command in [Data Validation Gates](#data-validation-gates). This should
be the first command for any new date range.

### 2. Dry-run AWS Batch submission

```bash
scripts/backtest_aws_fleet.sh 2026-04-13 2026-04-19 \
  --strategy-profile polymarket-exec/config/strategies/whale_bonereaper_strategy.live.yaml \
  --market-filter btc_5m \
  --fill-config nominal \
  --fill-quality base \
  --journal-mode none \
  --mode aws-batch \
  --aws-batch-job-queue <queue-name> \
  --aws-batch-job-definition <job-def-name> \
  --workers 8 \
  --max-inflight-jobs 8 \
  --s3-output-prefix s3://pm-research-fleet/results/bonereaper \
  --run-id bonereaper_20260413_20260419_base \
  --dry-run
```

Check that each date maps to one job and that the runner/profile paths are
correct in the submitted command.

### 3. Run base fill-quality batch

```bash
scripts/backtest_aws_fleet.sh 2026-04-13 2026-04-19 \
  --strategy-profile polymarket-exec/config/strategies/whale_bonereaper_strategy.live.yaml \
  --market-filter btc_5m \
  --fill-config nominal \
  --fill-quality base \
  --journal-mode none \
  --mode aws-batch \
  --aws-batch-job-queue <queue-name> \
  --aws-batch-job-definition <job-def-name> \
  --workers 8 \
  --max-inflight-jobs 8 \
  --s3-output-prefix s3://pm-research-fleet/results/bonereaper \
  --run-id bonereaper_20260413_20260419_base
```

Use this for throughput, sanity, and comparison.

### 4. Run conservative fill-quality batch

```bash
scripts/backtest_aws_fleet.sh 2026-04-13 2026-04-19 \
  --strategy-profile polymarket-exec/config/strategies/whale_bonereaper_strategy.live.yaml \
  --market-filter btc_5m \
  --fill-config conservative \
  --fill-quality conservative \
  --journal-mode none \
  --mode aws-batch \
  --aws-batch-job-queue <queue-name> \
  --aws-batch-job-definition <job-def-name> \
  --workers 8 \
  --max-inflight-jobs 8 \
  --s3-output-prefix s3://pm-research-fleet/results/bonereaper \
  --run-id bonereaper_20260413_20260419_conservative
```

Use this as the first live-relevance screen. It is still a backtest, not a
deployment recommendation.

### 5. Reduce artifacts into walk-forward diagnostics

The fleet runs one portfolio replay per day. Cross-day walk-forward validation
therefore happens after replay, from completed artifacts:

```bash
python3 polymarket-exec/scripts/cross_day_walk_forward.py \
  --output-root /tmp/backtest_fleet/run_bonereaper_20260413_20260419_conservative/outputs \
  --min-train-days 3 \
  --test-days 1 \
  --step-days 1 \
  --holdout-days 1
```

Outputs:

```text
/tmp/backtest_fleet/run_<RUN_ID>/outputs/walk_forward_report.json
/tmp/backtest_fleet/run_<RUN_ID>/outputs/walk_forward_folds.csv
```

The reducer marks a fold or holdout as non-claimable if it used optimistic fill
assumptions or includes rejected data-quality windows.

## Outputs

Fleet run directory:

```text
/tmp/backtest_fleet/run_<RUN_ID>/
```

Important files:

| Path | Meaning |
|---|---|
| `logs/` | Per-day worker logs |
| `status/` | Per-day status markers |
| `outputs/day_summary.tsv` | Day-level status, PnL, accepted fills, Sharpe |
| `outputs/runs/run_id=*/manifest.json` | Replay config, windows, data-quality summaries |
| `outputs/runs/run_id=*/metrics_summary.json` | Strategy/accounting metrics |
| `outputs/runs/run_id=*/journal.parquet` | Full replay journal when `--journal-mode full` |
| `outputs/walk_forward_report.json` | Cross-day train/test/holdout report |
| `outputs/walk_forward_folds.csv` | Tabular fold diagnostics |

When `--s3-output-prefix` is set, each AWS worker uploads its run artifacts to
that prefix.

## Performance Design

Fast runs come from avoiding repeated data work:

- Prepared day shards are cached by date and market type.
- The v2 cache manifest makes reuse explicit and deterministic.
- AWS Batch shards by day, so workers read only the data they need.
- `--journal-mode none` avoids writing the full event journal for first-pass
  runs.
- `--max-inflight-jobs` controls fleet parallelism.
- Cross-day walk-forward reads only `manifest.json` and `metrics_summary.json`;
  it does not reopen Parquet or replay events.

Correctness guard:

- If speed work changes replay outputs for the same input/seed/config, stop and
  explain the discrepancy before using the faster path.
- Keep `--seed` fixed for comparable runs.
- Do not use `--day-concurrency > 1` for cash-compounded/live-style replay
  unless the runner explicitly supports the requested independence mode.

## Historical Validation Policy

Use chronological splits only:

- Train/calibration days come first.
- Test days follow train days.
- Holdout days are last and must remain unseen until the final readout.

Recommended first pass for seven days:

```text
min_train_days = 3
test_days = 1
step_days = 1
holdout_days = 1
```

No-lookahead rules:

- Do not change knobs after reading holdout performance.
- Do not calibrate fill assumptions on the same period being claimed.
- Compare strategy behavior to whale research before tuning knobs:
  `docs/research/whale-research-index.md`,
  `docs/research/unlawful-shear-reconstruction-thread.md`,
  `docs/research/unlawful-whale-vs-us-calibration.md`, and
  `docs/research/bonereaper-research-thread.md`.
- Compare live/paper traces against replay using fill counts, quote age,
  side imbalance, inventory, rejected intents, and settlement outcomes.

## Diagnostics Checklist

Before reading PnL:

```bash
cat /tmp/backtest_fleet/run_<RUN_ID>/outputs/day_summary.tsv
```

For each failed day:

```bash
cat /tmp/backtest_fleet/run_<RUN_ID>/status/YYYY-MM-DD.status
sed -n '1,200p' /tmp/backtest_fleet/run_<RUN_ID>/logs/YYYY-MM-DD.log
```

For data-quality rejects:

```bash
jq '.data_quality[] | select(.status=="reject")' \
  /tmp/backtest_fleet/run_<RUN_ID>/outputs/runs/run_id=*/manifest.json
```

For exploratory fill assumptions:

```bash
jq '{run_id, fill_config, exploratory: (.fill_config | contains("optimistic"))}' \
  /tmp/backtest_fleet/run_<RUN_ID>/outputs/runs/run_id=*/metrics_summary.json
```

For walk-forward claimability:

```bash
jq '{folds: [.folds[] | {fold_index, test_days, claimable, data_quality_rejects, exploratory_fill_assumption}], holdout}' \
  /tmp/backtest_fleet/run_<RUN_ID>/outputs/walk_forward_report.json
```

## Stop Rules

- Historical data incomplete or inconsistent: stop before tuning.
- Data-quality `reject`: stop for that segment; repair or exclude it.
- Optimistic fills: label exploratory and do not present as expected live
  performance.
- Non-deterministic replay/accounting: fix determinism before adding strategy
  logic.
- Speed optimization changes outputs: stop and explain the discrepancy.
- Strategy knob needs tuning: inspect whale research first.
- Live-capital implication: do not recommend deployment from backtest results
  alone.
