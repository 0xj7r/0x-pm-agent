# Canonical backtest fleet workflow

This runner is the standard path for BTC 5m backtests.

- AWS Batch fan-out by default (one day per job, host does not run backtests).
- `--mode local` is retained for local smoke/debug only.
- S3 sync + manifest checks for each day shard.

## Script

```bash
scripts/backtest_aws_fleet.sh START_DATE END_DATE --strategy-profile PATH [options]
```

## Recommended run (AWS Batch)

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
  --run-id bonereaper_2026_04_13_19
```

Notes:
- `--journal-mode none` is fastest for first-pass PnL/fill throughput.
- Each AWS job writes to ephemeral worker output directories and uploads artifacts to `--s3-output-prefix` when set.
- Use `--dry-run` to validate submission commands before first launch.

## AWS Batch-only behavior

- Workers sync their own day shard at runtime.
- Control host no longer needs local 5m shard cache for the main batch run.
- The script waits for all AWS jobs and returns non-zero if any day fails.
- A run summary is written to `outputs/day_summary.tsv` with
  `day,status,pnl,accepted_fills,sharpe` and attempts to read S3 metrics when
  `--s3-output-prefix` is set.

## Local mode (debug only)

```bash
scripts/backtest_aws_fleet.sh 2026-04-13 2026-04-19 \
  --strategy-profile polymarket-exec/config/strategies/whale_bonereaper_strategy.live.yaml \
  --market-filter btc_5m \
  --mode local \
  --cache-root .cache/backtest-events
```

- `--workers` controls local max concurrent day jobs in debug mode.
- `--skip-sync`/`--force-reload` control local cache behavior.
- `--sync-only` performs cache warm-up locally and exits.
- Local mode is intentionally slower and is optional.

## Command flags

- `--workers` controls local max concurrent day jobs (local mode only).
- `--day-concurrency` passes `--concurrency` to the runner.
- `--cache-root` controls local cache path.
- `--s3-prefix` controls shard source in S3.
- `--s3-output-prefix` uploads per-run artifacts from AWS workers.
- `--skip-sync` avoids S3 work when manifest is present (local mode behavior).
- `--force-reload` ignores manifest and always resyncs day shard from S3.
- `--require-btc-tick` and `--require-market-meta` tune manifest checks.
- `--dry-run` prints what would be executed and exits.
- `--sync-retries` and `--sync-retry-sleep` control S3 retry behavior.

## Notes on robustness

- Batch workers run with their own ephemeral cache/output directories, so long host-side disk use is minimized.
- Jobs are polled until all in-flight jobs finish; any non-zero worker exit code marks the day failed.
## Output locations

- run logs: `/tmp/backtest_fleet/run_<RUN_ID>/logs`
- status markers: `/tmp/backtest_fleet/run_<RUN_ID>/status`
- run summaries: `<output_prefix>/runs/run_id=<run_id>/`
- per-run metrics: `metrics_summary.json`
- runner journal: `journal.parquet` only when `--journal-mode full`
