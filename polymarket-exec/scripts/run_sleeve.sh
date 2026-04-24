#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
CRATE_DIR="$ROOT_DIR/polymarket-exec"
BASE_LAUNCHER="$CRATE_DIR/scripts/run_unlawful_shear_paper.sh"
SLEEVE_INPUT="${1:-${WHALE_PAIR_SLEEVE:-}}"

log() {
  echo "[polymarket-exec-sleeve] $1"
}

fail() {
  echo "[polymarket-exec-sleeve] ERROR: $1"
  exit 1
}

resolve_sleeve_env() {
  local raw="$1"
  if [[ -z "$raw" ]]; then
    return 0
  fi
  if [[ -f "$raw" ]]; then
    printf '%s\n' "$raw"
    return 0
  fi
  if [[ -f "$CRATE_DIR/env/$raw.env" ]]; then
    printf '%s\n' "$CRATE_DIR/env/$raw.env"
    return 0
  fi
  if [[ -f "$CRATE_DIR/env/$raw" ]]; then
    printf '%s\n' "$CRATE_DIR/env/$raw"
    return 0
  fi
  return 1
}

if [[ ! -x "$BASE_LAUNCHER" ]]; then
  fail "base launcher is missing or not executable: $BASE_LAUNCHER"
fi

if [[ -n "$SLEEVE_INPUT" ]]; then
  if ! SLEEVE_ENV_PATH="$(resolve_sleeve_env "$SLEEVE_INPUT")"; then
    fail "could not resolve sleeve env from '$SLEEVE_INPUT' (expected polymarket-exec/env/<name>.env or an explicit path)"
  fi
  export WHALE_PAIR_SLEEVE_ENV_PATH="$SLEEVE_ENV_PATH"
  log "resolved sleeve env: $WHALE_PAIR_SLEEVE_ENV_PATH"
else
  log "no sleeve env supplied; relying on .env + exported overrides only"
fi

exec "$BASE_LAUNCHER"
