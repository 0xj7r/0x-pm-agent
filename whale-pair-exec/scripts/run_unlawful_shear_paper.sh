#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
CRATE_DIR="$ROOT_DIR/whale-pair-exec"
DATA_DIR="$ROOT_DIR/data/research/wallet_research/unlawful-shear"
CONTEXT_PATH="$DATA_DIR/rust_market_context.json"
RUNTIME_ENV_PATH="$DATA_DIR/rust_runtime.env"
FALLBACK_DB_PATH="$DATA_DIR/wallet_research.db"
CONTEXT_SOURCE="${WHALE_PAIR_CONTEXT_SOURCE:-live}"
INCLUDE_PREV="${WHALE_PAIR_INCLUDE_PREV:-1}"
INCLUDE_NEXT="${WHALE_PAIR_INCLUDE_NEXT:-1}"
SLEEVE_ENV_PATH="${WHALE_PAIR_SLEEVE_ENV_PATH:-}"
SKIP_CARGO_RUN="${WHALE_PAIR_SKIP_CARGO_RUN:-false}"
REFRESH_INTERVAL_SEC="${WHALE_PAIR_CONTEXT_REFRESH_INTERVAL_SEC:-45}"
PYTHON_BIN="${WHALE_PAIR_PYTHON_BIN:-python3}"
CARGO_BIN="${WHALE_PAIR_CARGO_BIN:-cargo}"
CHILD_PID=""

log_section() {
  echo "[unlawful-shear-paper] $1"
}

fail() {
  echo "[unlawful-shear-paper] ERROR: $1"
  exit 1
}

export_runtime_context() {
  local context_out="$1"
  local env_out="$2"
  local phase="${3:-refresh}"
  local export_cmd=(
    "$PYTHON_BIN" "$ROOT_DIR/scripts/export_btc_5m_runtime.py"
    --source "$CONTEXT_SOURCE"
    --db "$FALLBACK_DB_PATH"
    --include-prev "$INCLUDE_PREV"
    --include-next "$INCLUDE_NEXT"
    --env-out "$env_out"
    --context-out "$context_out"
  )
  log_section "exporting runtime context phase=$phase source=$CONTEXT_SOURCE include_prev=$INCLUDE_PREV include_next=$INCLUDE_NEXT"
  "${export_cmd[@]}"
}

load_runtime_env() {
  if [[ -f "$CRATE_DIR/.env" ]]; then
    set -a
    source "$CRATE_DIR/.env"
    set +a
  fi

  set -a
  source "$RUNTIME_ENV_PATH"
  set +a

  if [[ -n "$SLEEVE_ENV_PATH" ]]; then
    if [[ ! -f "$SLEEVE_ENV_PATH" ]]; then
      fail "sleeve env file not found: $SLEEVE_ENV_PATH"
    fi
    set -a
    source "$SLEEVE_ENV_PATH"
    set +a
  fi

  export WHALE_PAIR_STRATEGY="${WHALE_PAIR_STRATEGY:-unlawful_shear}"
  if [[ "${WHALE_PAIR_PAPER_MODE:-}" != "true" ]]; then
    log_section "WHALE_PAIR_PAPER_MODE is not explicitly true; defaulting to paper via script"
  fi
  export WHALE_PAIR_PAPER_MODE="${WHALE_PAIR_PAPER_MODE:-true}"
  if [[ "$WHALE_PAIR_PAPER_MODE" != "true" && "$WHALE_PAIR_PAPER_MODE" != "false" ]]; then
    fail "WHALE_PAIR_PAPER_MODE must be true/false, got: $WHALE_PAIR_PAPER_MODE"
  fi
  export WHALE_PAIR_EXEC_MARKET_CONTEXT_PATH="${WHALE_PAIR_EXEC_MARKET_CONTEXT_PATH:-$CONTEXT_PATH}"
  export WHALE_PAIR_EXEC_METRICS_BIND="${WHALE_PAIR_EXEC_METRICS_BIND:-127.0.0.1:9108}"
  mkdir -p "$(dirname "$WHALE_PAIR_EXEC_MARKET_CONTEXT_PATH")"

  if [[ -n "${WHALE_PAIR_ORDER_STORE_PATH:-}" ]]; then
    mkdir -p "$(dirname "$WHALE_PAIR_ORDER_STORE_PATH")"
  fi
  if [[ -n "${WHALE_PAIR_EXEC_JOURNAL_PATH:-}" ]]; then
    mkdir -p "$(dirname "$WHALE_PAIR_EXEC_JOURNAL_PATH")"
  fi
  if [[ -n "${WHALE_PAIR_DASHBOARD_WHALE_EVENTS_PATH:-}" ]]; then
    mkdir -p "$(dirname "$WHALE_PAIR_DASHBOARD_WHALE_EVENTS_PATH")"
  fi
}

launch_child() {
  log_section "launching crate with strategy=$WHALE_PAIR_STRATEGY paper_mode=$WHALE_PAIR_PAPER_MODE"
  log_section "runtime env: $RUNTIME_ENV_PATH"
  log_section "market context: $WHALE_PAIR_EXEC_MARKET_CONTEXT_PATH"
  if [[ -n "${WHALE_PAIR_ORDER_STORE_PATH:-}" ]]; then
    log_section "order store: $WHALE_PAIR_ORDER_STORE_PATH"
  fi
  if [[ -n "${WHALE_PAIR_EXEC_JOURNAL_PATH:-}" ]]; then
    log_section "journal: $WHALE_PAIR_EXEC_JOURNAL_PATH"
  fi
  log_section "unlawful gate config: allow_extreme_offhour_override=${WHALE_PAIR_UNLAWFUL_SHEAR_ALLOW_EXTREME_OFFHOUR_OVERRIDE:-<unset>} primary_btc=(${WHALE_PAIR_UNLAWFUL_SHEAR_PRIMARY_MIN_BTC_REALIZED_VOL_5M_BPS:-<unset>},${WHALE_PAIR_UNLAWFUL_SHEAR_PRIMARY_MIN_BTC_REALIZED_VOL_15M_BPS:-<unset>},${WHALE_PAIR_UNLAWFUL_SHEAR_PRIMARY_MIN_BTC_TRADE_COUNT_5M:-<unset>}) secondary_btc=(${WHALE_PAIR_UNLAWFUL_SHEAR_SECONDARY_MIN_BTC_REALIZED_VOL_5M_BPS:-<unset>},${WHALE_PAIR_UNLAWFUL_SHEAR_SECONDARY_MIN_BTC_REALIZED_VOL_15M_BPS:-<unset>},${WHALE_PAIR_UNLAWFUL_SHEAR_SECONDARY_MIN_BTC_TRADE_COUNT_5M:-<unset>}) override_btc=(${WHALE_PAIR_UNLAWFUL_SHEAR_OVERRIDE_MIN_BTC_REALIZED_VOL_5M_BPS:-<unset>},${WHALE_PAIR_UNLAWFUL_SHEAR_OVERRIDE_MIN_BTC_REALIZED_VOL_15M_BPS:-<unset>},${WHALE_PAIR_UNLAWFUL_SHEAR_OVERRIDE_MIN_BTC_TRADE_COUNT_5M:-<unset>})"
  if [[ -n "${WHALE_PAIR_STRATEGY_PROFILE_PATH:-}" ]]; then
    log_section "strategy profile: $WHALE_PAIR_STRATEGY_PROFILE_PATH"
  else
    log_section "strategy profile: <none> (runtime defaults + env overrides)"
  fi
  log_section "metrics bind: $WHALE_PAIR_EXEC_METRICS_BIND"
  (
    cd "$CRATE_DIR"
    "$CARGO_BIN" run
  ) &
  CHILD_PID=$!
  log_section "spawned cargo run pid=$CHILD_PID"
}

stop_child() {
  local reason="${1:-shutdown}"
  if [[ -z "$CHILD_PID" ]]; then
    return 0
  fi
  if kill -0 "$CHILD_PID" >/dev/null 2>&1; then
    log_section "stopping cargo run pid=$CHILD_PID reason=$reason"
    kill "$CHILD_PID" >/dev/null 2>&1 || true
    wait "$CHILD_PID" || true
  fi
  CHILD_PID=""
}

runtime_health_url() {
  printf 'http://%s/healthz' "$WHALE_PAIR_EXEC_METRICS_BIND"
}

runtime_ready_for_refresh() {
  curl -fsS --max-time 1 "$(runtime_health_url)" >/dev/null 2>&1
}

cleanup() {
  stop_child "script-exit"
}

refresh_artifacts_if_needed() {
  local temp_context_path
  local temp_runtime_env_path
  temp_context_path="$(mktemp "$DATA_DIR/.rust_market_context.next.XXXXXX")"
  temp_runtime_env_path="$(mktemp "$DATA_DIR/.rust_runtime.next.XXXXXX")"

  if [[ -z "$temp_context_path" || -z "$temp_runtime_env_path" ]]; then
    rm -f "${temp_context_path:-}" "${temp_runtime_env_path:-}"
    fail "failed to allocate temporary runtime artifacts"
  fi

  export_runtime_context "$temp_context_path" "$temp_runtime_env_path" "refresh"

  if [[ ! -s "$temp_runtime_env_path" ]]; then
    rm -f "$temp_context_path" "$temp_runtime_env_path"
    fail "generated runtime env is empty: $temp_runtime_env_path"
  fi
  if [[ ! -s "$temp_context_path" ]]; then
    rm -f "$temp_context_path" "$temp_runtime_env_path"
    fail "market context artifact is empty: $temp_context_path"
  fi

  if cmp -s "$temp_runtime_env_path" "$RUNTIME_ENV_PATH" && cmp -s "$temp_context_path" "$CONTEXT_PATH"; then
    rm -f "$temp_runtime_env_path" "$temp_context_path"
    return 1
  fi

  mv "$temp_runtime_env_path" "$RUNTIME_ENV_PATH"
  mv "$temp_context_path" "$CONTEXT_PATH"
  return 0
}

if ! command -v "$PYTHON_BIN" >/dev/null 2>&1; then
  fail "python binary not found in PATH: $PYTHON_BIN"
fi
if ! command -v "$CARGO_BIN" >/dev/null 2>&1; then
  fail "cargo binary not found in PATH: $CARGO_BIN"
fi

mkdir -p "$DATA_DIR"

export_runtime_context "$CONTEXT_PATH" "$RUNTIME_ENV_PATH" "initial"

if [[ ! -s "$RUNTIME_ENV_PATH" ]]; then
  fail "generated runtime env is empty: $RUNTIME_ENV_PATH"
fi
if [[ ! -s "$CONTEXT_PATH" ]]; then
  fail "market context artifact is empty: $CONTEXT_PATH"
fi

load_runtime_env

if [[ -n "$SLEEVE_ENV_PATH" ]]; then
  log_section "sleeve env: $SLEEVE_ENV_PATH"
fi

if [[ "$SKIP_CARGO_RUN" == "true" ]]; then
  log_section "WHALE_PAIR_SKIP_CARGO_RUN=true, stopping after runtime bootstrap"
  exit 0
fi

if ! [[ "$REFRESH_INTERVAL_SEC" =~ ^[0-9]+$ ]]; then
  fail "WHALE_PAIR_CONTEXT_REFRESH_INTERVAL_SEC must be an integer, got: $REFRESH_INTERVAL_SEC"
fi

trap cleanup EXIT INT TERM

if [[ "$REFRESH_INTERVAL_SEC" -eq 0 ]]; then
  cd "$CRATE_DIR"
  exec "$CARGO_BIN" run
fi

log_section "context refresh supervision enabled interval_sec=$REFRESH_INTERVAL_SEC"
launch_child

while true; do
  sleep "$REFRESH_INTERVAL_SEC"

  if ! kill -0 "$CHILD_PID" >/dev/null 2>&1; then
    wait "$CHILD_PID" || true
    log_section "cargo run exited unexpectedly; relaunching"
    load_runtime_env
    launch_child
    continue
  fi

  if refresh_artifacts_if_needed; then
    if runtime_ready_for_refresh; then
      log_section "detected rolling market context change; restarting sleeve onto fresh slate"
      stop_child "market-context-refresh"
      load_runtime_env
      launch_child
    else
      log_section "detected rolling market context change but runtime is not healthy yet; deferring restart"
    fi
  fi
done
