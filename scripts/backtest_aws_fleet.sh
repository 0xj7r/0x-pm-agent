#!/usr/bin/env bash
set -euo pipefail

usage() {
  cat >&2 <<'USAGE'
Usage:
  backtest_aws_fleet.sh START_DATE END_DATE --strategy-profile PROFILE_PATH [options]
  backtest_aws_fleet.sh --as-worker --day YYYY-MM-DD --strategy-profile PROFILE_PATH [options]

Mode:
  --mode aws-batch (default): one job per day via AWS Batch (no backtest runs on the host).
  --mode local: one-machine fan-out across day shards.

Common options:
  --strategy-profile PATH
  --profile PATH                  alias
  --workers N                     parallel workers for local mode (default: 2)
  --day-concurrency N             runner --concurrency for per-day run (default: 1)
  --market-filter NAME            market_type partition to run (default: btc_5m)
  --fill-config STRING            default: nominal
  --fill-quality STRING           default: base
  --journal-mode none|full        default: none
  --walk-forward-mode none|expanding|rolling
  --walk-forward-min-train-windows N
  --walk-forward-test-windows N
  --walk-forward-step-windows N
  --walk-forward-holdout-windows N
  --starting-cash-usd N           default: 1000
  --runner PATH                   default: target/release/backtest_runner
  --cache-root PATH               local cache root (local mode) default: .cache/backtest-events
  --s3-prefix PATH                default: s3://pm-research-data-prod/processed/v=1
  --s3-output-prefix PATH         optional s3:// destination for artifacts in aws-batch mode
  --run-id STRING                 default: date-timestamp
  --mode local|aws-batch          (default: aws-batch)
  --aws-batch-job-queue NAME
  --aws-batch-job-definition NAME
  --aws-workdir PATH              default: /work/polymarket-agent
  --seed N                        default: 0xC0FFEE
  --sync-only                     only sync inputs, no backtests
  --validate-cache                with --sync-only, dry-run Rust parser/preflight against prepared shard
  --dry-run                       print planned commands only
  --force-reload                   resync day shard even if manifest indicates ready
  --skip-sync                     skip syncing and use cached local shard
  --require-btc-tick/--no-require-btc-tick
  --require-market-meta/--no-require-market-meta
  --sync-retries N                default: 3
  --sync-retry-sleep N            default: 10
  --max-inflight-jobs N           default: 8
  --aws-profile NAME
  --aws-region NAME
  --help
USAGE
  exit 2
}

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
RUN_ROOT_DEFAULT="/tmp/backtest_fleet"
CACHE_ROOT_DEFAULT="$REPO_ROOT/.cache/backtest-events"
S3_PREFIX_DEFAULT="s3://pm-research-data-prod/processed/v=1"
RUNNER_DEFAULT="$REPO_ROOT/target/release/backtest_runner"
AWS_DEFAULT_WORKDIR="/work/polymarket-agent"

AS_WORKER=0
START=""
END=""
DAY=""
WORKERS=2
DAY_CONCURRENCY=1
MARKET_FILTER="btc_5m"
FILL_CONFIG="nominal"
FILL_QUALITY="base"
JOURNAL_MODE="none"
WALK_FORWARD_MODE="none"
WALK_FORWARD_MIN_TRAIN_WINDOWS=0
WALK_FORWARD_TEST_WINDOWS=0
WALK_FORWARD_STEP_WINDOWS=1
WALK_FORWARD_HOLDOUT_WINDOWS=0
PROFILE_PATH=""
RUNNER="$RUNNER_DEFAULT"
START_CASH=1000
RUN_ID="$(date -u +%Y%m%dT%H%M%SZ)"
CACHE_ROOT="$CACHE_ROOT_DEFAULT"
S3_PREFIX="$S3_PREFIX_DEFAULT"
S3_OUTPUT_PREFIX=""
MODE="aws-batch"
AWS_BATCH_QUEUE=""
AWS_BATCH_DEF=""
AWS_WORKDIR="$AWS_DEFAULT_WORKDIR"
SEED="0xC0FFEE"
SYNC_ONLY=0
VALIDATE_CACHE=0
DRY_RUN=0
FORCE_RELOAD=0
SKIP_SYNC=0
REQUIRE_BTC_TICK=0
REQUIRE_MARKET_META=1
SYNC_RETRIES=3
SYNC_RETRY_SLEEP=10
MAX_AWS_INFLIGHT=8
AWS_PROFILE_NAME="${AWS_PROFILE:-visumlabs}"
AWS_REGION="${AWS_DEFAULT_REGION:-us-east-1}"
AWS_POLL_SECONDS=15
WORKER_CACHE_ROOT="/tmp/backtest_fleet/worker_cache"
WORKER_OUTPUT_ROOT="/tmp/backtest_fleet"

if [ "$#" -eq 0 ]; then
  usage
fi

if [ "$1" = "--as-worker" ]; then
  AS_WORKER=1
  shift
else
  [ "$#" -lt 2 ] && usage
  START="$1"
  END="$2"
  shift 2
fi

while [ "$#" -gt 0 ]; do
  case "$1" in
    --help|-h)
      usage
      ;;
    --as-worker)
      AS_WORKER=1
      shift
      ;;
    --day)
      DAY="$2"
      shift 2
      ;;
    --start-date)
      START="$2"
      shift 2
      ;;
    --end-date)
      END="$2"
      shift 2
      ;;
    --workers)
      WORKERS="$2"
      shift 2
      ;;
    --day-concurrency)
      DAY_CONCURRENCY="$2"
      shift 2
      ;;
    --market-filter)
      MARKET_FILTER="$2"
      shift 2
      ;;
    --fill-config)
      FILL_CONFIG="$2"
      shift 2
      ;;
    --fill-quality)
      FILL_QUALITY="$2"
      shift 2
      ;;
    --journal-mode)
      JOURNAL_MODE="$2"
      shift 2
      ;;
    --walk-forward-mode)
      WALK_FORWARD_MODE="$2"
      shift 2
      ;;
    --walk-forward-min-train-windows)
      WALK_FORWARD_MIN_TRAIN_WINDOWS="$2"
      shift 2
      ;;
    --walk-forward-test-windows)
      WALK_FORWARD_TEST_WINDOWS="$2"
      shift 2
      ;;
    --walk-forward-step-windows)
      WALK_FORWARD_STEP_WINDOWS="$2"
      shift 2
      ;;
    --walk-forward-holdout-windows)
      WALK_FORWARD_HOLDOUT_WINDOWS="$2"
      shift 2
      ;;
    --starting-cash-usd|--starting-cash)
      START_CASH="$2"
      shift 2
      ;;
    --strategy-profile|--profile)
      PROFILE_PATH="$2"
      shift 2
      ;;
    --runner)
      RUNNER="$2"
      shift 2
      ;;
    --run-id)
      RUN_ID="$2"
      shift 2
      ;;
    --cache-root)
      CACHE_ROOT="$2"
      shift 2
      ;;
    --s3-prefix)
      S3_PREFIX="$2"
      shift 2
      ;;
    --s3-output-prefix)
      S3_OUTPUT_PREFIX="$2"
      shift 2
      ;;
    --mode)
      MODE="$2"
      shift 2
      ;;
    --aws-batch-job-queue)
      AWS_BATCH_QUEUE="$2"
      shift 2
      ;;
    --aws-batch-job-definition)
      AWS_BATCH_DEF="$2"
      shift 2
      ;;
    --aws-workdir)
      AWS_WORKDIR="$2"
      shift 2
      ;;
    --seed)
      SEED="$2"
      shift 2
      ;;
    --sync-only)
      SYNC_ONLY=1
      shift
      ;;
    --validate-cache)
      VALIDATE_CACHE=1
      shift
      ;;
    --dry-run)
      DRY_RUN=1
      shift
      ;;
    --force-reload)
      FORCE_RELOAD=1
      shift
      ;;
    --skip-sync)
      SKIP_SYNC=1
      shift
      ;;
    --require-btc-tick)
      REQUIRE_BTC_TICK=1
      shift
      ;;
    --no-require-btc-tick)
      REQUIRE_BTC_TICK=0
      shift
      ;;
    --require-market-meta)
      REQUIRE_MARKET_META=1
      shift
      ;;
    --no-require-market-meta)
      REQUIRE_MARKET_META=0
      shift
      ;;
    --sync-retries)
      SYNC_RETRIES="$2"
      shift 2
      ;;
    --sync-retry-sleep)
      SYNC_RETRY_SLEEP="$2"
      shift 2
      ;;
    --max-inflight-jobs)
      MAX_AWS_INFLIGHT="$2"
      shift 2
      ;;
    --aws-profile)
      AWS_PROFILE_NAME="$2"
      shift 2
      ;;
    --aws-region)
      AWS_REGION="$2"
      shift 2
      ;;
    *)
      echo "unknown arg: $1" >&2
      usage
      ;;
  esac
done

if [ "$#" -ne 0 ]; then
  echo "extra args: $*" >&2
  usage
fi

if [ -z "$PROFILE_PATH" ]; then
  echo "--strategy-profile is required" >&2
  usage
fi
if [ ! -f "$PROFILE_PATH" ]; then
  echo "profile not found: $PROFILE_PATH" >&2
  exit 2
fi
if [ "$AS_WORKER" -eq 1 ] && [ -z "$DAY" ]; then
  echo "--as-worker requires --day YYYY-MM-DD" >&2
  exit 2
fi
if [ "$AS_WORKER" -eq 0 ] && { [ -z "$START" ] || [ -z "$END" ]; }; then
  echo "START_DATE and END_DATE required when not running as worker" >&2
  usage
fi
if [ "$AS_WORKER" -eq 0 ]; then
  if ! python3 - "$START" "$END" <<'PY'
from datetime import date
import sys
start = date.fromisoformat(sys.argv[1])
end = date.fromisoformat(sys.argv[2])
if end < start:
    raise SystemExit(1)
PY
  then
    echo "date range invalid (must be YYYY-MM-DD and END >= START)" >&2
    exit 2
  fi
fi

if [ "$AS_WORKER" -eq 1 ] && [ "$MODE" = "aws-batch" ]; then
  fallback_runner="$AWS_WORKDIR/polymarket-exec/target/release/backtest_runner"
  if [ ! -x "$RUNNER" ] && [ -x "$fallback_runner" ]; then
    RUNNER="$fallback_runner"
  fi
fi
if ! command -v "$RUNNER" >/dev/null 2>&1 || [ ! -x "$RUNNER" ]; then
  echo "runner not found or not executable: $RUNNER" >&2
  exit 2
fi
if [ "$JOURNAL_MODE" != "none" ] && [ "$JOURNAL_MODE" != "full" ]; then
  echo "--journal-mode must be none|full" >&2
  exit 2
fi
if [ "$WALK_FORWARD_MODE" != "none" ] && [ "$WALK_FORWARD_MODE" != "expanding" ] && [ "$WALK_FORWARD_MODE" != "rolling" ]; then
  echo "--walk-forward-mode must be none|expanding|rolling" >&2
  exit 2
fi
if [ "$WALK_FORWARD_MODE" != "none" ]; then
  if [ "$WALK_FORWARD_MIN_TRAIN_WINDOWS" -lt 1 ] || [ "$WALK_FORWARD_TEST_WINDOWS" -lt 1 ] || [ "$WALK_FORWARD_STEP_WINDOWS" -lt 1 ] || [ "$WALK_FORWARD_HOLDOUT_WINDOWS" -lt 1 ]; then
    echo "walk-forward window counts must be >=1 when enabled" >&2
    exit 2
  fi
fi
if [ "$MODE" != "local" ] && [ "$MODE" != "aws-batch" ]; then
  echo "--mode must be local|aws-batch" >&2
  exit 2
fi
if [ "$DAY_CONCURRENCY" -lt 1 ]; then
  echo "--day-concurrency must be >=1" >&2
  exit 2
fi
if [ "$WORKERS" -lt 1 ]; then
  echo "--workers must be >=1" >&2
  exit 2
fi
if [ "$MODE" = "aws-batch" ] && ! command -v aws >/dev/null 2>&1; then
  echo "aws cli not found" >&2
  exit 2
fi

export AWS_PROFILE="$AWS_PROFILE_NAME"
export AWS_DEFAULT_REGION="$AWS_REGION"

if grep -Eiq 'bonereaper_mm|late_favorite_directional' "$PROFILE_PATH"; then
  REQUIRE_BTC_TICK=1
fi
if [ "$REQUIRE_BTC_TICK" -eq 1 ]; then
  REQUIRE_MARKET_META=1
fi

if [ "$AS_WORKER" -eq 1 ] || [ "$MODE" = "local" ]; then
  mkdir -p "$CACHE_ROOT"
fi
RUN_ROOT="${RUN_ROOT_DEFAULT}/run_${RUN_ID}"
OUTPUT_ROOT="$RUN_ROOT/outputs"
LOG_ROOT="$RUN_ROOT/logs"
STATUS_ROOT="$RUN_ROOT/status"
mkdir -p "$OUTPUT_ROOT" "$LOG_ROOT" "$STATUS_ROOT"

cleanup_worker_runtime() {
  if [ "$AS_WORKER" -eq 1 ] && [ "$MODE" = "aws-batch" ]; then
    rm -rf "${WORKER_CACHE_ROOT:?}" "${WORKER_OUTPUT_ROOT:?}/outputs"
  fi
}
trap cleanup_worker_runtime EXIT

log() {
  echo "[backtest-fleet] $*"
}

preflight_aws_batch() {
  if [ -z "$AWS_BATCH_QUEUE" ] || [ -z "$AWS_BATCH_DEF" ]; then
    echo "--mode aws-batch requires --aws-batch-job-queue and --aws-batch-job-definition" >&2
    return 2
  fi
  local queue_status
  local jobdef_status
  queue_status="$(aws batch describe-job-queues \
    --job-queues "$AWS_BATCH_QUEUE" --query 'jobQueues[0].status' --output text 2>/dev/null || true)"
  if [ -z "$queue_status" ] || [ "$queue_status" = "None" ]; then
    echo "AWS preflight failed: job queue not found or inaccessible: $AWS_BATCH_QUEUE" >&2
    return 2
  fi
  if [ "$queue_status" != "ENABLED" ]; then
    echo "AWS preflight warning: queue ${AWS_BATCH_QUEUE} status=${queue_status}" >&2
  fi

  jobdef_status="$(aws batch describe-job-definitions \
    --job-definition-name "$AWS_BATCH_DEF" --query 'jobDefinitions[0].status' --output text 2>/dev/null || true)"
  if [ -z "$jobdef_status" ] || [ "$jobdef_status" = "None" ]; then
    echo "AWS preflight failed: job definition not found or inaccessible: $AWS_BATCH_DEF" >&2
    return 2
  fi
  if [ "$jobdef_status" != "ACTIVE" ]; then
    echo "AWS preflight warning: job definition ${AWS_BATCH_DEF} status=${jobdef_status}" >&2
  fi
}

date_plus_one() {
  local d="$1"
  python3 - "$d" <<'PY'
from datetime import date, timedelta
import sys
print((date.fromisoformat(sys.argv[1]) + timedelta(days=1)).isoformat())
PY
}

build_dates() {
  local start="$1"
  local end="$2"
  python3 - "$start" "$end" <<'PY'
from datetime import date, timedelta
from sys import argv
s = date.fromisoformat(argv[1])
e = date.fromisoformat(argv[2])
d = s
while d <= e:
  print(d.isoformat())
  d += timedelta(days=1)
PY
}

event_type_count() {
  local dir="$1"
  local kind="$2"
  local scope="$3"
  find "$dir" -type f -path "*/market_type=${scope}/event_type=${kind}/*" -name "*.parquet" -size +0c | wc -l | tr -d ' '
}

event_has_type() {
  local dir="$1"
  local kind="$2"
  local scope="$3"
  find "$dir" -type f -path "*/market_type=${scope}/event_type=${kind}/*" -name "*.parquet" -size +0c -print -quit | grep -q .
}

required_types() {
  printf 'trade\nbook_delta\n'
  if [ "$REQUIRE_MARKET_META" -eq 1 ]; then
    printf 'market_meta\n'
  fi
  if [ "$REQUIRE_BTC_TICK" -eq 1 ]; then
    printf 'btc_tick\n'
  fi
}

required_scope() {
  local kind="$1"
  if [ "$kind" = "btc_tick" ]; then
    echo "btc_ref"
  else
    echo "$MARKET_FILTER"
  fi
}

cache_manifest_path() {
  local day_dir="$1"
  echo "$day_dir/.backtest_manifest.json"
}

cache_ready_for_day() {
  local day_dir="$1"
  local manifest="$2"
  local -a needed=()

  while IFS= read -r kind; do
    needed+=("$kind")
  done < <(required_types)

  if [ -f "$manifest" ]; then
    if python3 - "$manifest" "$MARKET_FILTER" "$S3_PREFIX" "$REQUIRE_BTC_TICK" "$REQUIRE_MARKET_META" "${needed[@]}" <<'PY'
import json
import sys

manifest_path = sys.argv[1]
market_filter = sys.argv[2]
s3_prefix = sys.argv[3]
require_btc_tick = bool(int(sys.argv[4]))
require_market_meta = bool(int(sys.argv[5]))
required = sys.argv[6:]
with open(manifest_path, "r", encoding="utf-8") as f:
    manifest = json.load(f)
if manifest.get("schema_version") != 2:
    raise SystemExit(1)
if manifest.get("market_filter") != market_filter:
    raise SystemExit(1)
if manifest.get("s3_prefix") != s3_prefix:
    raise SystemExit(1)
if bool(manifest.get("require_btc_tick")) != require_btc_tick:
    raise SystemExit(1)
if bool(manifest.get("require_market_meta")) != require_market_meta:
    raise SystemExit(1)
counts = manifest.get("event_counts", {})
for kind in required:
    if int(counts.get(kind, 0)) <= 0:
        raise SystemExit(1)
PY
    then
      return 0
    fi
  fi

  for kind in "${needed[@]}"; do
    scope="$(required_scope "$kind")"
    if ! event_has_type "$day_dir" "$kind" "$scope"; then
      return 1
    fi
  done
  return 0
}

write_manifest() {
  local day="$1"
  local day_dir="$2"
  local manifest="$3"
  local -a needed=()

  while IFS= read -r kind; do
    needed+=("$kind")
  done < <(required_types)

  mkdir -p "$day_dir"
  python3 - "$day" "$day_dir" "$manifest" "$MARKET_FILTER" "$S3_PREFIX" "$REQUIRE_BTC_TICK" "$REQUIRE_MARKET_META" "${needed[@]}" <<'PY'
from datetime import datetime, timezone
import json
import os
import sys

day, day_dir, manifest_path = sys.argv[1:4]
market_filter, s3_prefix = sys.argv[4:6]
require_btc_tick = bool(int(sys.argv[6]))
require_market_meta = bool(int(sys.argv[7]))
required = sys.argv[8:]
kinds = ["trade", "book_delta", "book_snapshot", "market_meta", "btc_tick"]

def scope_for(kind: str) -> str:
    return "btc_ref" if kind == "btc_tick" else market_filter

counts = {kind: 0 for kind in kinds}
bytes_by_kind = {kind: 0 for kind in kinds}
latest_mtime = None
for root, _, files in os.walk(day_dir):
    parts = set(root.split(os.sep))
    for kind in kinds:
        if f"market_type={scope_for(kind)}" not in parts:
            continue
        if f"event_type={kind}" not in parts:
            continue
        for name in files:
            if not name.endswith(".parquet"):
                continue
            path = os.path.join(root, name)
            try:
                stat = os.stat(path)
            except FileNotFoundError:
                continue
            if stat.st_size <= 0:
                continue
            counts[kind] += 1
            bytes_by_kind[kind] += stat.st_size
            latest_mtime = max(latest_mtime or stat.st_mtime, stat.st_mtime)

manifest = {
    "schema_version": 2,
    "date": day,
    "market_filter": market_filter,
    "s3_prefix": s3_prefix,
    "require_btc_tick": require_btc_tick,
    "require_market_meta": require_market_meta,
    "required_event_types": required,
    "event_counts": counts,
    "event_bytes": bytes_by_kind,
    "ready": all(counts.get(kind, 0) > 0 for kind in required),
    "latest_input_mtime": latest_mtime,
    "generated_at": datetime.now(timezone.utc).isoformat(),
}
tmp_path = f"{manifest_path}.tmp"
with open(tmp_path, "w", encoding="utf-8") as f:
    json.dump(manifest, f, indent=2, sort_keys=True)
    f.write("\n")
os.replace(tmp_path, manifest_path)
PY
}

sync_day_input() {
  local day="$1"
  local day_dir="$CACHE_ROOT/dt=$day"
  local manifest
  local attempts=1
  local filters=(
    --exclude "*"
    --only-show-errors
  )

  manifest="$(cache_manifest_path "$day_dir")"
  if [ "$SKIP_SYNC" -eq 1 ]; then
    if cache_ready_for_day "$day_dir" "$manifest"; then
      write_manifest "$day" "$day_dir" "$manifest"
      return 0
    fi
    log "cache miss for $day (skip-sync enabled)"
    return 1
  fi

  if [ "$FORCE_RELOAD" -eq 0 ] && cache_ready_for_day "$day_dir" "$manifest"; then
    write_manifest "$day" "$day_dir" "$manifest"
    log "using cached data for $day"
    return 0
  fi

  mkdir -p "$day_dir"

  while IFS= read -r kind; do
    scope="$(required_scope "$kind")"
    filters+=(--include "*/market_type=${scope}/event_type=${kind}/*")
  done < <(required_types)

  while [ "$attempts" -le "$SYNC_RETRIES" ]; do
    if aws s3 sync \
      "$S3_PREFIX/dt=$day/" \
      "$day_dir/" \
      "${filters[@]}"; then
      write_manifest "$day" "$day_dir" "$manifest"
      log "synced cache for $day -> $day_dir"
      return 0
    fi
    log "sync attempt $attempts/$SYNC_RETRIES failed for $day"
    attempts=$((attempts + 1))
    if [ "$attempts" -le "$SYNC_RETRIES" ]; then
      sleep "$SYNC_RETRY_SLEEP"
    fi
  done

  log "sync failed for $day after $SYNC_RETRIES attempts"
  return 1
}

runner_day_window() {
  local day="$1"
  date_plus_one "$day"
}

build_runner_args() {
  local day="$1"
  local day_end="$2"
  local run_id="$3"
  local force_dry_run="${4:-0}"
  local input_root="$CACHE_ROOT"
  local output_prefix_arg="$OUTPUT_ROOT"
  if [ "$AS_WORKER" -eq 1 ] && [ "$MODE" = "aws-batch" ]; then
    input_root="$WORKER_CACHE_ROOT"
    output_prefix_arg="$WORKER_OUTPUT_ROOT/outputs"
  fi
  local -a args=(
    "$RUNNER"
    --window-start "${day}T00:00:00Z"
    --window-end "${day_end}T00:00:00Z"
    --strategy-profile "$PROFILE_PATH"
    --market-filter "$MARKET_FILTER"
    --input-prefix "$input_root/dt=$day"
    --metadata-prefix "$input_root/dt=$day"
    --output-prefix "$output_prefix_arg"
    --input-format rust-event
    --fill-config "$FILL_CONFIG"
    --fill-quality "$FILL_QUALITY"
    --journal-mode "$JOURNAL_MODE"
    --starting-cash-usd "$START_CASH"
    --git-rev "fleet"
    --seed "$SEED"
    --run-id "$run_id"
  )

  if [ "$DAY_CONCURRENCY" -gt 1 ]; then
    args+=(--concurrency "$DAY_CONCURRENCY")
  fi
  if [ "$WALK_FORWARD_MODE" != "none" ]; then
    args+=(
      --walk-forward-mode "$WALK_FORWARD_MODE"
      --walk-forward-min-train-windows "$WALK_FORWARD_MIN_TRAIN_WINDOWS"
      --walk-forward-test-windows "$WALK_FORWARD_TEST_WINDOWS"
      --walk-forward-step-windows "$WALK_FORWARD_STEP_WINDOWS"
      --walk-forward-holdout-windows "$WALK_FORWARD_HOLDOUT_WINDOWS"
    )
  fi
  if [ "$DRY_RUN" -eq 1 ] || [ "$force_dry_run" -eq 1 ]; then
    args+=(--dry-run)
  fi
  printf '%s\n' "${args[@]}"
}

validate_prepared_day() {
  local day="$1"
  local day_end="$2"
  local run_id="$3"
  local log_file="$4"
  local -a args=()

  while IFS= read -r arg; do
    args+=("$arg")
  done < <(build_runner_args "$day" "$day_end" "${run_id}_validate" 1)

  {
    echo "[validate-cache] ${args[*]}"
    "${args[@]}"
  } >> "$log_file" 2>&1
}

publish_run_outputs() {
  local day_id="$1"
  local run_output_root="$WORKER_OUTPUT_ROOT/outputs"
  local src="$run_output_root/runs/run_id=${day_id}"
  local dst="${S3_OUTPUT_PREFIX%/}/run_id=${day_id}/"

  if [ -z "$S3_OUTPUT_PREFIX" ]; then
    return 0
  fi
  if [ "$MODE" != "aws-batch" ] || [ "$AS_WORKER" -eq 0 ]; then
    return 0
  fi
  if ! command -v aws >/dev/null 2>&1; then
    echo "aws cli missing; cannot upload run_id=${day_id}" >&2
    return 1
  fi
  if ! [ -d "$src" ]; then
    echo "missing output dir for upload: $src" >&2
    return 1
  fi
  aws s3 sync "$src/" "$dst" --only-show-errors
}

expected_run_id() {
  local day="$1"
  echo "${RUN_ID}_dt=${day}"
}

has_uploaded_outputs() {
  local day="$1"
  local run_id
  local target_prefix
  local out
  local attempts=1
  local max_attempts=5
  local delay=2
  local max_delay=30

  if [ -z "$S3_OUTPUT_PREFIX" ]; then
    return 0
  fi
  if [ "$MODE" != "aws-batch" ] || [ "$AS_WORKER" -eq 1 ]; then
    return 0
  fi
  run_id="$(expected_run_id "$day")"
  target_prefix="${S3_OUTPUT_PREFIX%/}/run_id=${run_id}/runs/run_id=${run_id}/"
  while [ "$attempts" -le "$max_attempts" ]; do
    if out="$(aws s3 ls "${target_prefix}" 2>/dev/null || true)"; then
      if [ -n "$out" ]; then
        return 0
      fi
    fi
    if [ "$attempts" -eq "$max_attempts" ]; then
      break
    fi
    sleep "$delay"
    delay=$((delay * 2))
    if [ "$delay" -gt "$max_delay" ]; then
      delay="$max_delay"
    fi
    attempts=$((attempts + 1))
  done
  return 1
}

read_s3_metrics() {
  local day="$1"
  local run_id
  local metrics_prefix
  local tmp_json
  local pnl
  local fills
  local sharpe

  if [ -z "$S3_OUTPUT_PREFIX" ] || [ "$AS_WORKER" -eq 1 ]; then
    echo "n/a n/a n/a"
    return 0
  fi
  run_id="$(expected_run_id "$day")"
  metrics_prefix="${S3_OUTPUT_PREFIX%/}/run_id=${run_id}/runs/run_id=${run_id}/metrics_summary.json"
  tmp_json="$(mktemp)"
  if ! aws s3 cp "$metrics_prefix" "$tmp_json" >/dev/null 2>&1; then
    rm -f "$tmp_json"
    echo "n/a n/a n/a"
    return 0
  fi
  read -r pnl fills sharpe <<<"$(python3 - "$tmp_json" <<'PY'
import json,sys
m=json.load(open(sys.argv[1]))
pnl=m.get("total_pnl_usd", 0)
fills=m.get("accepted_fills", "n/a")
sharpe=m.get("sharpe", "n/a")
if sharpe is None:
  sharpe="n/a"
print(pnl, fills, sharpe)
PY
)"
  rm -f "$tmp_json"
  echo "${pnl} ${fills} ${sharpe}"
}

run_one_day() {
  local day="$1"
  local day_end
  local day_id="${RUN_ID}_dt=${day}"
  local status="$STATUS_ROOT/${day}.status"
  local log_file="$LOG_ROOT/${day}.log"
  local run_output_root="$OUTPUT_ROOT"

  if [ "$AS_WORKER" -eq 1 ] && [ "$MODE" = "aws-batch" ]; then
    run_output_root="$WORKER_OUTPUT_ROOT/outputs"
    mkdir -p "$run_output_root" "$WORKER_CACHE_ROOT"
  fi
  mkdir -p "$run_output_root"

  day_end="$(runner_day_window "$day")"

  if [ "$DRY_RUN" -eq 1 ] || [ "$SYNC_ONLY" -eq 1 ]; then
    : > "$status"
  fi

  if [ "$SYNC_ONLY" -eq 0 ]; then
    if ! sync_day_input "$day"; then
      echo "FAIL cache_missing" > "$status"
      return 1
    fi

    if [ "$DRY_RUN" -eq 1 ]; then
      local -a args=()
      while IFS= read -r arg; do
        args+=("$arg")
      done < <(build_runner_args "$day" "$day_end" "$day_id")
      {
        echo "[run] ${args[*]}"
      } > "$log_file"
      echo "SKIP dry_run" > "$status"
      return 0
    fi
  else
    if ! sync_day_input "$day"; then
      echo "FAIL sync_only_missing" > "$status"
      return 1
    fi
    if [ "$VALIDATE_CACHE" -eq 1 ]; then
      if ! validate_prepared_day "$day" "$day_end" "$day_id" "$log_file"; then
        echo "FAIL sync_only_validation_failed" > "$status"
        return 1
      fi
    fi
    echo "OK sync_only" > "$status"
    return 0
  fi

  local -a runner_args=()
  while IFS= read -r arg; do
    runner_args+=("$arg")
  done < <(build_runner_args "$day" "$day_end" "$day_id")

  if "${runner_args[@]}"; then
    local rc=$?
    local metrics_file="$run_output_root/runs/run_id=$day_id/metrics_summary.json"
    local pnl="n/a"
    local fills="n/a"
    local sharpe="n/a"
    if [ -f "$metrics_file" ]; then
      read -r pnl fills sharpe <<<"$(python3 - "$metrics_file" <<'PY'
import json,sys
m=json.load(open(sys.argv[1]))
pnl=m.get("total_pnl_usd",0)
fills=m.get("accepted_fills",0)
sharpe=m.get("sharpe",None)
if sharpe is None:
  sharpe=float('nan')
print(pnl, fills, sharpe)
PY
)"
    fi
    if [ "$MODE" = "aws-batch" ] && [ "$AS_WORKER" -eq 1 ]; then
      if ! publish_run_outputs "$day_id"; then
        echo "FAIL upload_failed rc=${rc}" > "$status"
        return 1
      fi
    fi
    echo "OK pnl=${pnl} fills=${fills} sharpe=${sharpe} rc=${rc}" > "$status"
    return 0
  fi

  local rc=$?
  echo "FAIL runner_exit=${rc}" > "$status"
  return 1
}

submit_batch_job() {
  local day="$1"
  local run_id="${RUN_ID}_dt=${day}"
  local wrapped_cmd

  wrapped_cmd="$AWS_WORKDIR/scripts/backtest_aws_fleet.sh --as-worker --day \"$day\" --strategy-profile \"$PROFILE_PATH\" --workers \"$WORKERS\" --day-concurrency \"$DAY_CONCURRENCY\" --market-filter \"$MARKET_FILTER\" --fill-config \"$FILL_CONFIG\" --fill-quality \"$FILL_QUALITY\" --journal-mode \"$JOURNAL_MODE\" --walk-forward-mode \"$WALK_FORWARD_MODE\" --walk-forward-min-train-windows \"$WALK_FORWARD_MIN_TRAIN_WINDOWS\" --walk-forward-test-windows \"$WALK_FORWARD_TEST_WINDOWS\" --walk-forward-step-windows \"$WALK_FORWARD_STEP_WINDOWS\" --walk-forward-holdout-windows \"$WALK_FORWARD_HOLDOUT_WINDOWS\" --starting-cash-usd \"$START_CASH\" --runner \"$RUNNER\" --cache-root \"$WORKER_CACHE_ROOT\" --s3-prefix \"$S3_PREFIX\" --s3-output-prefix \"$S3_OUTPUT_PREFIX\" --run-id \"$RUN_ID\" --mode \"$MODE\" --seed \"$SEED\""
  if [ "$VALIDATE_CACHE" -eq 1 ]; then
    wrapped_cmd="$wrapped_cmd --validate-cache"
  fi

  if [ "$SYNC_ONLY" -eq 1 ] || [ "$DRY_RUN" -eq 1 ]; then
    echo "would submit: $wrapped_cmd"
    return 0
  fi

  local job_id
  job_id="$(aws batch submit-job \
    --job-name "btf-${run_id}" \
    --job-queue "$AWS_BATCH_QUEUE" \
    --job-definition "$AWS_BATCH_DEF" \
    --container-overrides "{\"command\":[\"/bin/bash\",\"-lc\",\"$wrapped_cmd\"]}" \
    --query 'jobId' --output text)"
  if [ -z "$job_id" ] || [ "$job_id" = "None" ]; then
    echo "aws submit failed for $day" >&2
    return 1
  fi
  echo "$job_id" > "$STATUS_ROOT/batch.$day.jobid"
  log "submitted day=${day} job=${job_id}"
}

run_local_days() {
  local days=("$@")
  local day
  for day in "${days[@]}"; do
    while [ "$(jobs -pr | wc -l | tr -d ' ')" -ge "$WORKERS" ]; do
      sleep 1
    done
    run_one_day "$day" > "$RUN_ROOT/logs/${day}.worker.log" 2>&1 &
  done
  wait
}

collect_summary() {
  local status_dir="$STATUS_ROOT"
  local failed=0
  local done_days=()
  local fail_days=()
  local done_count=0
  local fail_count=0

  shopt -s nullglob
  for status_file in "$status_dir"/*.status; do
    day="$(basename "$status_file" .status)"
    status="$(cat "$status_file" 2>/dev/null | tr -d '\n' || true)"
    done_count=$((done_count + 1))
    if printf '%s' "$status" | grep -Eq '^(OK|SKIP)'; then
      done_days+=("$day")
    else
      failed=1
      fail_count=$((fail_count + 1))
      fail_days+=("$day")
    fi
  done
  shopt -u nullglob

  log "completed=${done_count} failed=${fail_count}"
  if [ "${#fail_days[@]}" -gt 0 ]; then
    log "failed_days=${fail_days[*]}"
  fi
  if [ "$failed" -ne 0 ]; then
    return 1
  fi
  return 0
}

write_status_report() {
  local report_path="$OUTPUT_ROOT/day_summary.tsv"
  local status_dir="$STATUS_ROOT"
  local line
  local status
  local pnl
  local fills
  local sharpe

  echo "day,status,pnl,accepted_fills,sharpe" > "$report_path"
  shopt -s nullglob
  for status_file in "$status_dir"/*.status; do
    day="$(basename "$status_file" .status)"
    status="$(cat "$status_file" 2>/dev/null | tr -d '\n' || true)"
    read -r pnl fills sharpe <<<"$(read_s3_metrics "$day")"
    line="${day},${status},${pnl},${fills},${sharpe}"
    echo "$line" >> "$report_path"
  done
  shopt -u nullglob
  log "wrote day summary: $report_path"
}

wait_for_batch_jobs() {
  while true; do
    local inflight=0
    local failed=0

    shopt -s nullglob
    for job_file in "$STATUS_ROOT"/batch.*.jobid; do
      local day
      local job_id
      local status
      local exit_code
      local status_reason

      day="${job_file##*/}"
      day="${day#batch.}"
      day="${day%.jobid}"
      job_id="$(cat "$job_file" 2>/dev/null | tr -d '[:space:]' || true)"
      if [ -z "$job_id" ]; then
        echo "FAIL status_missing_job_id" > "$STATUS_ROOT/${day}.status"
        failed=1
        continue
      fi

      status="$(aws batch describe-jobs --jobs "$job_id" --query 'jobs[0].status' --output text 2>/dev/null || true)"
      if [ -z "$status" ] || [ "$status" = "None" ]; then
        status="UNKNOWN"
      fi

      case "$status" in
        SUBMITTED|PENDING|RUNNABLE|STARTING|RUNNING|RETRYING)
          inflight=$((inflight + 1))
          echo "RUNNING batch_status=${status} job_id=${job_id}" > "$STATUS_ROOT/${day}.status"
          ;;
        SUCCEEDED)
          exit_code="$(aws batch describe-jobs --jobs "$job_id" --query 'jobs[0].container.exitCode' --output text 2>/dev/null || true)"
          if [ -z "$exit_code" ] || [ "$exit_code" = "None" ]; then
            exit_code="0"
          fi
          if [ "$exit_code" != "0" ]; then
            failed=1
            echo "FAIL batch_status=${status} job_id=${job_id} rc=${exit_code}" > "$STATUS_ROOT/${day}.status"
            continue
          fi
          if ! has_uploaded_outputs "$day"; then
            failed=1
            echo "FAIL batch_status=${status} job_id=${job_id} rc=${exit_code} upload_missing" > "$STATUS_ROOT/${day}.status"
          else
            echo "OK batch_status=${status} job_id=${job_id} rc=${exit_code}" > "$STATUS_ROOT/${day}.status"
          fi
          ;;
        FAILED|CANCELED|TERMINATED)
          failed=1
          status_reason="$(aws batch describe-jobs --jobs "$job_id" --query 'jobs[0].statusReason' --output text 2>/dev/null || true)"
          if [ -z "$status_reason" ] || [ "$status_reason" = "None" ]; then
            status_reason="UNKNOWN"
          fi
          echo "FAIL batch_status=${status} job_id=${job_id} reason=${status_reason}" > "$STATUS_ROOT/${day}.status"
          ;;
        *)
          inflight=$((inflight + 1))
          echo "RUNNING batch_status=${status} job_id=${job_id}" > "$STATUS_ROOT/${day}.status"
          ;;
      esac
    done
    shopt -u nullglob

    if [ "$inflight" -eq 0 ]; then
      [ "$failed" -ne 0 ] && return 1
      return 0
    fi

    log "batch jobs in flight: ${inflight}"
    sleep "$AWS_POLL_SECONDS"
  done
}

DAYS=()
if [ "$AS_WORKER" -eq 1 ]; then
  DAYS=("$DAY")
else
  while IFS= read -r d; do
    DAYS+=("$d")
  done < <(build_dates "$START" "$END")
fi

GIT_REV="$(git -C "$REPO_ROOT" rev-parse --short HEAD 2>/dev/null || echo unknown)"
log "run_id=$RUN_ID mode=$MODE workers=$WORKERS day-concurrency=$DAY_CONCURRENCY"
log "days=${DAYS[*]}"

if [ "$MODE" = "aws-batch" ] && [ "$AS_WORKER" -eq 0 ]; then
  preflight_aws_batch
fi

if [ "$AS_WORKER" -eq 1 ]; then
  run_one_day "$DAY"
  exit $?
fi

if [ "$MODE" = "aws-batch" ]; then
  inflight=0
  for day in "${DAYS[@]}"; do
    while [ "$inflight" -ge "$MAX_AWS_INFLIGHT" ]; do
      sleep 1
      inflight="$(ls -1 "$STATUS_ROOT"/batch.*.jobid 2>/dev/null | wc -l | tr -d ' ')"
    done
    submit_batch_job "$day"
    if [ "$DRY_RUN" -eq 0 ] && [ "$SYNC_ONLY" -eq 0 ]; then
      inflight=$((inflight + 1))
    fi
  done
  log "batch dispatch complete"
  if [ "$DRY_RUN" -eq 1 ] || [ "$SYNC_ONLY" -eq 1 ]; then
    exit 0
  fi
  write_status_report
  if ! wait_for_batch_jobs; then
    log "batch jobs failed"
    if collect_summary; then
      log "all days complete"
    else
      log "some days failed"
    fi
    exit 1
  fi
  if collect_summary; then
    write_status_report
    log "all days complete"
    exit 0
  fi
  log "some days failed"
  exit 1
fi

run_local_days "${DAYS[@]}"
if collect_summary; then
  log "all days complete"
  exit 0
fi
log "some days failed"
exit 1
