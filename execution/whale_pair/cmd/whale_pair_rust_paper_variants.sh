#!/usr/bin/env bash
# Start multiple Rust whale-pair paper strategies in parallel.
#
# Usage:
#   scripts/whale_pair_rust_paper_variants.sh start [variant]
#   scripts/whale_pair_rust_paper_variants.sh stop [variant]
#   scripts/whale_pair_rust_paper_variants.sh status [variant]
#
# If no variant is provided, all variants are started/stopped/checked.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../../.." && pwd)"
CRATE_DIR="$REPO_ROOT/execution/rust/whale-pair-exec"
WORK_ROOT="${WHALE_PAIR_PAPER_WORK_ROOT:-$REPO_ROOT/data/whale_pair_rust_paper}"
ENV_FILE="${WHALE_PAIR_PAPER_ENV_FILE:-$REPO_ROOT/.env}"
METRICS_PORT_BASE="${WHALE_PAIR_PAPER_METRICS_PORT_BASE:-9108}"
CARGO_BIN="${WHALE_PAIR_CARGO_BIN:-cargo}"
PYTHON_BIN="${WHALE_PAIR_PYTHON_BIN:-python3}"

if [ -f "$ENV_FILE" ]; then
    set -a
    # shellcheck disable=SC1090
    source "$ENV_FILE"
    set +a
fi

if [ ! -d "$CRATE_DIR" ]; then
    echo "ERROR: rust crate not found: $CRATE_DIR" >&2
    exit 2
fi

if [ ! -x "$CRATE_DIR/src/main.rs" ] && [ ! -f "$CRATE_DIR/src/main.rs" ]; then
    echo "ERROR: missing Rust entrypoint: $CRATE_DIR/src/main.rs" >&2
    exit 3
fi

if ! command -v "$CARGO_BIN" >/dev/null 2>&1; then
    echo "ERROR: cargo not found in PATH (set WHALE_PAIR_CARGO_BIN)." >&2
    exit 4
fi

if [ -z "${WHALE_PAIR_ASSET_IDS:-}" ]; then
    echo "ERROR: WHALE_PAIR_ASSET_IDS is required (export it or set in $ENV_FILE)." >&2
    exit 5
fi

if [ -z "${WHALE_PAIR_INSTRUMENT_MARKETS:-}" ]; then
    echo "ERROR: WHALE_PAIR_INSTRUMENT_MARKETS is required for paired execution." >&2
    exit 6
fi

mkdir -p "$WORK_ROOT"

SERVICE_NAME_PREFIX="${WHALE_PAIR_SERVICE_NAME_PREFIX:-whale-pair-paper}"
BASE_LOOP_MS="${WHALE_PAIR_EXEC_LOOP_INTERVAL_MS:-1000}"

VARIANTS=()

load_variants_from_python() {
    if ! command -v "$PYTHON_BIN" >/dev/null 2>&1; then
        echo "ERROR: python runtime not found (set WHALE_PAIR_PYTHON_BIN)." >&2
        return 9
    fi

    # TODO: align these preset values with live queue/latency calibrations after
    # order-book replay validation.
    mapfile -t VARIANTS < <(
        PYTHONPATH="$REPO_ROOT" "$PYTHON_BIN" - <<'PY'
from execution.whale_pair.strategy_presets import rust_variant_env_lines

for line in rust_variant_env_lines():
    print(line)
PY
    )

    if [ ${#VARIANTS[@]} -eq 0 ]; then
        echo "ERROR: failed to load Rust variants from execution/whale_pair/strategy_presets.py" >&2
        return 10
    fi
}

if ! load_variants_from_python; then
    echo "ERROR: could not initialize Rust variant matrix." >&2
    exit 10
fi

MODE="${1:-}"
if [ -z "$MODE" ]; then
    echo "Usage: $0 start|stop|status [variant]" >&2
    exit 64
fi

TARGET="${2:-}"

is_running() {
    local pid_file=$1
    if [ ! -f "$pid_file" ]; then
        return 1
    fi
    local pid
    pid="$(cat "$pid_file")"
    if [ -z "$pid" ] || ! [[ "$pid" =~ ^[0-9]+$ ]]; then
        return 1
    fi
    if kill -0 "$pid" >/dev/null 2>&1; then
        return 0
    fi
    return 1
}

stop_one() {
    local name=$1
    local dir="$WORK_ROOT/$name"
    local pid_file="$dir/pid"
    local log_file="$dir/run.log"

    if ! is_running "$pid_file"; then
        echo "[$name] not running"
        return
    fi
    local pid
    pid="$(cat "$pid_file")"
    kill "$pid" >/dev/null 2>&1 || true
    for _ in {1..30}; do
        if ! is_running "$pid_file"; then
            break
        fi
        sleep 1
    done
    if is_running "$pid_file"; then
        kill -9 "$pid" >/dev/null 2>&1 || true
        sleep 1
    fi
    rm -f "$pid_file"
    echo "[$name] stopped (pid ${pid})"
}

status_one() {
    local name=$1
    local dir="$WORK_ROOT/$name"
    local pid_file="$dir/pid"
    local log_file="$dir/run.log"
    if is_running "$pid_file"; then
        local pid
        pid="$(cat "$pid_file")"
        echo "[$name] running pid=$pid log=$log_file"
    else
        echo "[$name] not running (log=$log_file)"
    fi
}

run_one() {
    local raw=$1
    local idx=$2
    local -a fields
    local -a override_env
    IFS='|' read -ra fields <<< "$raw"
    local name="${fields[0]}"
    if [ -n "$TARGET" ] && [ "$name" != "$TARGET" ]; then
        return 0
    fi
    local dir="$WORK_ROOT/$name"
    local pid_file="$dir/pid"
    local log_file="$dir/run.log"
    mkdir -p "$dir"

    if is_running "$pid_file"; then
        local existing
        existing="$(cat "$pid_file")"
        echo "[$name] already running (pid $existing), skipping"
        return 0
    fi

    rm -f "$pid_file"
    local metric_port=$((METRICS_PORT_BASE + idx))
    local journal="$dir/journal.jsonl"
    local service_name="${SERVICE_NAME_PREFIX}-${name}"

    (
        cd "$REPO_ROOT"
        export RUST_LOG="${WHALE_PAIR_RUST_LOG:-info}"
        export WHALE_PAIR_PAPER_MODE="${WHALE_PAIR_PAPER_MODE:-true}"
        export WHALE_PAIR_EXEC_SERVICE_NAME="$service_name"
        export WHALE_PAIR_EXEC_METRICS_BIND="0.0.0.0:${metric_port}"
        export WHALE_PAIR_EXEC_JOURNAL_PATH="$journal"
        export WHALE_PAIR_EXEC_LOOP_INTERVAL_MS="$BASE_LOOP_MS"
        export WHALE_PAIR_EXEC_BOOK_STALE_MS="${WHALE_PAIR_EXEC_BOOK_STALE_MS:-2000}"
        export WHALE_PAIR_EXEC_PING_INTERVAL_MS="${WHALE_PAIR_EXEC_PING_INTERVAL_MS:-10000}"
        export WHALE_PAIR_EXEC_SUMMARY_INTERVAL_MS="${WHALE_PAIR_EXEC_SUMMARY_INTERVAL_MS:-10000}"
        export WHALE_PAIR_EXEC_STARTING_CASH_USD="${WHALE_PAIR_EXEC_STARTING_CASH_USD:-0.0}"
        export WHALE_PAIR_EXEC_MAX_OPEN_ORDERS_TOTAL="${WHALE_PAIR_EXEC_MAX_OPEN_ORDERS_TOTAL:-32}"
        export WHALE_PAIR_EXEC_MAX_OPEN_ORDERS_PER_MARKET="${WHALE_PAIR_EXEC_MAX_OPEN_ORDERS_PER_MARKET:-8}"
        override_env=("${fields[@]:1}")
        for entry in "${override_env[@]}"; do
            if [ -n "$entry" ]; then
                export "$entry"
            fi
        done
        nohup "$CARGO_BIN" run --manifest-path "$CRATE_DIR/Cargo.toml" \
            > "$log_file" 2>&1 < /dev/null &
        echo "$!" > "$pid_file"
    )
    sleep 1
    if is_running "$pid_file"; then
        local pid
        pid="$(cat "$pid_file")"
        echo "[$name] started pid=$pid metrics=http://127.0.0.1:${metric_port}/metrics log=$log_file"
    else
        echo "[$name] failed to start; see $log_file" >&2
    fi
}

case "$MODE" in
    start)
        for i in "${!VARIANTS[@]}"; do
            run_one "${VARIANTS[$i]}" "$i"
        done
        ;;
    stop)
        for spec in "${VARIANTS[@]}"; do
            IFS='|' read -ra fields <<< "$spec"
            name="${fields[0]}"
            if [ -n "$TARGET" ] && [ "$name" != "$TARGET" ]; then
                continue
            fi
            stop_one "$name"
        done
        ;;
    status)
        for spec in "${VARIANTS[@]}"; do
            IFS='|' read -ra fields <<< "$spec"
            name="${fields[0]}"
            if [ -n "$TARGET" ] && [ "$name" != "$TARGET" ]; then
                continue
            fi
            status_one "$name"
        done
        ;;
    *)
        echo "Usage: $0 start|stop|status [variant]" >&2
        exit 64
        ;;
esac
