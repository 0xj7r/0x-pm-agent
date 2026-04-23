#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/../../../.." && pwd)"
CRATE_DIR="$ROOT_DIR/execution/rust/whale-pair-exec"
CONTEXT_PATH="$ROOT_DIR/data/research/wallet_research/unlawful-shear/rust_market_context.json"
RUNTIME_ENV_PATH="$ROOT_DIR/data/research/wallet_research/unlawful-shear/rust_runtime.env"
BACKUP_CONTEXT_PATH="$CRATE_DIR/.unlawful_shear_context.json"
BACKUP_RUNTIME_ENV_PATH="$CRATE_DIR/.unlawful_shear_runtime.env"
SKIP_LIVE_EXPORT="${WHALE_PAIR_SKIP_LIVE_EXPORT:-false}"

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

mkdir -p "$(dirname "$RUNTIME_ENV_PATH")"

if [[ "$SKIP_LIVE_EXPORT" == "true" ]]; then
  log_section "WHALE_PAIR_SKIP_LIVE_EXPORT=true, loading fallback context from local research DB"
  python3 "$ROOT_DIR/research/dataops/export_rust_market_context.py" \
    --db "$ROOT_DIR/data/research/wallet_research/unlawful-shear/wallet_research.db" \
    --out "$CONTEXT_PATH"
  python3 "$ROOT_DIR/research/dataops/export_rust_runtime_env.py" \
    --db "$ROOT_DIR/data/research/wallet_research/unlawful-shear/wallet_research.db" \
    --out "$RUNTIME_ENV_PATH"
  cp "$CONTEXT_PATH" "$BACKUP_CONTEXT_PATH"
  cp "$RUNTIME_ENV_PATH" "$BACKUP_RUNTIME_ENV_PATH"
  log_section "using historical context fallback outputs"
else
  LIVE_EXPORT_ARGS=()
  if [[ -n "${WHALE_PAIR_LIVE_SLUG:-}" ]]; then
    LIVE_EXPORT_ARGS+=(--slug "$WHALE_PAIR_LIVE_SLUG")
  fi

  log_section "resolving runtime context from export scripts"
  if [[ "${#LIVE_EXPORT_ARGS[@]}" -gt 0 ]]; then
    LIVE_EXPORT_CMD=(python3 "$ROOT_DIR/research/dataops/export_live_gamma_runtime.py" "${LIVE_EXPORT_ARGS[@]}" --env-out "$RUNTIME_ENV_PATH" --context-out "$CONTEXT_PATH")
  else
    LIVE_EXPORT_CMD=(python3 "$ROOT_DIR/research/dataops/export_live_gamma_runtime.py" --env-out "$RUNTIME_ENV_PATH" --context-out "$CONTEXT_PATH")
  fi

  if ! "${LIVE_EXPORT_CMD[@]}"; then
    python3 "$ROOT_DIR/research/dataops/export_rust_market_context.py" \
      --db "$ROOT_DIR/data/research/wallet_research/unlawful-shear/wallet_research.db" \
      --out "$CONTEXT_PATH"
    python3 "$ROOT_DIR/research/dataops/export_rust_runtime_env.py" \
      --db "$ROOT_DIR/data/research/wallet_research/unlawful-shear/wallet_research.db" \
      --out "$RUNTIME_ENV_PATH"
    cp "$CONTEXT_PATH" "$BACKUP_CONTEXT_PATH"
    cp "$RUNTIME_ENV_PATH" "$BACKUP_RUNTIME_ENV_PATH"
    log_section "using historical context fallback outputs"
  else
    log_section "live export succeeded"
  fi
fi

if [[ ! -f "$BACKUP_CONTEXT_PATH" ]]; then
  cp "$CONTEXT_PATH" "$BACKUP_CONTEXT_PATH" 2>/dev/null || true
fi
if [[ ! -f "$BACKUP_RUNTIME_ENV_PATH" ]]; then
  cp "$RUNTIME_ENV_PATH" "$BACKUP_RUNTIME_ENV_PATH" 2>/dev/null || true
fi

if [[ ! -s "$RUNTIME_ENV_PATH" ]]; then
  fail "generated runtime env is empty: $RUNTIME_ENV_PATH"
fi
if [[ ! -s "$CONTEXT_PATH" ]]; then
  fail "market context artifact is empty: $CONTEXT_PATH"
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
log_section "launching crate with strategy=$WHALE_PAIR_STRATEGY paper_mode=$WHALE_PAIR_PAPER_MODE"
log_section "runtime env: $RUNTIME_ENV_PATH"
log_section "market context: $WHALE_PAIR_EXEC_MARKET_CONTEXT_PATH"
log_section "metrics bind: $WHALE_PAIR_EXEC_METRICS_BIND"


if [[ -f "$CRATE_DIR/.env" ]]; then
  set -a
  source "$CRATE_DIR/.env"
  set +a
fi

set -a
source "$RUNTIME_ENV_PATH"
set +a

cd "$CRATE_DIR"
exec cargo run
