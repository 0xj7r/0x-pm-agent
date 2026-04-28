#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT_DIR"

export WHALE_PAIR_CONTEXT_SOURCE="${WHALE_PAIR_CONTEXT_SOURCE:-live}"
export WHALE_PAIR_INCLUDE_PREV="${WHALE_PAIR_INCLUDE_PREV:-0}"
export WHALE_PAIR_INCLUDE_NEXT="${WHALE_PAIR_INCLUDE_NEXT:-0}"
export WHALE_PAIR_CONTEXT_REFRESH_INTERVAL_SEC="${WHALE_PAIR_CONTEXT_REFRESH_INTERVAL_SEC:-0}"
export WHALE_PAIR_CARGO_BIN="${WHALE_PAIR_CARGO_BIN:-cargo}"

exec polymarket-exec/scripts/run_sleeve.sh btc_5m_mm_tinylive
