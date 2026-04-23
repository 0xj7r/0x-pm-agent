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

log_section() {
  echo "[unlawful-shear-paper] $1"
}

fail() {
  echo "[unlawful-shear-paper] ERROR: $1"
  exit 1
}

if ! command -v python3 >/dev/null 2>&1; then
  fail "python3 not found in PATH"
fi
if ! command -v cargo >/dev/null 2>&1; then
  fail "cargo not found in PATH"
fi

mkdir -p "$DATA_DIR"

EXPORT_CMD=(python3 "$ROOT_DIR/scripts/export_btc_5m_runtime.py" --source "$CONTEXT_SOURCE" --db "$FALLBACK_DB_PATH" --include-prev "$INCLUDE_PREV" --include-next "$INCLUDE_NEXT" --env-out "$RUNTIME_ENV_PATH" --context-out "$CONTEXT_PATH")
log_section "exporting runtime context source=$CONTEXT_SOURCE include_prev=$INCLUDE_PREV include_next=$INCLUDE_NEXT"
"${EXPORT_CMD[@]}"

if [[ ! -s "$RUNTIME_ENV_PATH" ]]; then
  fail "generated runtime env is empty: $RUNTIME_ENV_PATH"
fi
if [[ ! -s "$CONTEXT_PATH" ]]; then
  fail "market context artifact is empty: $CONTEXT_PATH"
fi

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

if [[ -n "$SLEEVE_ENV_PATH" ]]; then
  log_section "sleeve env: $SLEEVE_ENV_PATH"
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

if [[ -n "${WHALE_PAIR_ORDER_STORE_PATH:-}" ]]; then
  mkdir -p "$(dirname "$WHALE_PAIR_ORDER_STORE_PATH")"
fi
if [[ -n "${WHALE_PAIR_EXEC_JOURNAL_PATH:-}" ]]; then
  mkdir -p "$(dirname "$WHALE_PAIR_EXEC_JOURNAL_PATH")"
fi

log_section "launching crate with strategy=$WHALE_PAIR_STRATEGY paper_mode=$WHALE_PAIR_PAPER_MODE"
log_section "runtime env: $RUNTIME_ENV_PATH"
log_section "market context: $WHALE_PAIR_EXEC_MARKET_CONTEXT_PATH"
if [[ -n "${WHALE_PAIR_STRATEGY_PROFILE_PATH:-}" ]]; then
  log_section "strategy profile: $WHALE_PAIR_STRATEGY_PROFILE_PATH"
else
  log_section "strategy profile: <none> (runtime defaults + env overrides)"
fi
log_section "metrics bind: $WHALE_PAIR_EXEC_METRICS_BIND"

if [[ "$SKIP_CARGO_RUN" == "true" ]]; then
  log_section "WHALE_PAIR_SKIP_CARGO_RUN=true, stopping after runtime bootstrap"
  exit 0
fi

cd "$CRATE_DIR"
exec cargo run
