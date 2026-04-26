#!/usr/bin/env bash
# Live CTF redeem wrapper.
#
# Scans Polymarket positions for the configured wallet, identifies any
# that are redeemable (resolved markets), and issues redeem transactions
# via the relayer. Recovers stranded collateral.
#
# Usage:
#   ./scripts/live_redeem.sh              # DRY RUN (logs only, no submit)
#   ./scripts/live_redeem.sh --execute    # actually submit
#
# Required env (loaded from .env):
#   POLYMARKET_API_KEY, POLYMARKET_SECRET, POLYMARKET_PASSPHRASE
#   POLYMARKET_BOT_PRIVATE_KEY  (the signer)
#   POLYMARKET_FUNDER_ADDRESS   (proxy wallet for SIGNATURE_TYPE=1)
#   POLYMARKET_SIGNATURE_TYPE=1
#   RELAYER_API_KEY, RELAYER_API_KEY_ADDRESS

set -euo pipefail
cd "$(dirname "$0")/.."

if [[ ! -f .env ]]; then
  echo "ERROR: .env not found at $(pwd)/.env" >&2
  exit 1
fi

# Safely load .env: handles values containing spaces/commas/special
# chars without shell interpretation. Skip lines that aren't
# `KEY=value` and skip comments.
while IFS='=' read -r raw_key raw_value || [[ -n "$raw_key" ]]; do
  key="${raw_key%%[[:space:]]*}"
  [[ -z "$key" || "$key" =~ ^# ]] && continue
  [[ ! "$key" =~ ^[A-Za-z_][A-Za-z0-9_]*$ ]] && continue
  # Strip surrounding quotes if present
  value="${raw_value%\"}"
  value="${value#\"}"
  value="${value%\'}"
  value="${value#\'}"
  export "$key=$value"
done < .env

# Default to DRY RUN unless --execute passed
if [[ "${1:-}" == "--execute" ]]; then
  export WHALE_PAIR_LIVE_REDEEM_DRY_RUN=false
  echo ">>> EXECUTE MODE: redemptions will be SUBMITTED to the relayer <<<"
else
  export WHALE_PAIR_LIVE_REDEEM_DRY_RUN=true
  echo ">>> DRY RUN: planning only, no submission. Pass --execute to submit. <<<"
fi

export WHALE_PAIR_EXEC_MODE=live_redeem
export WHALE_PAIR_PAPER_MODE=false
# Redeem mode doesn't trade specific assets - it just queries positions
# from the Data API. Provide a placeholder so AppConfig::from_env passes
# its asset-list validation.
export WHALE_PAIR_ASSET_IDS="${WHALE_PAIR_ASSET_IDS:-1}"
export WHALE_PAIR_INSTRUMENT_MARKETS="${WHALE_PAIR_INSTRUMENT_MARKETS:-1:redeem-stub}"
export WHALE_PAIR_USER_MARKETS="${WHALE_PAIR_USER_MARKETS:-redeem-stub}"

# Use the main-tree binary (has redeem code from this session)
BIN=$(pwd)/target/release/polymarket-exec
if [[ ! -x "$BIN" ]]; then
  echo "ERROR: no built polymarket-exec binary at $BIN" >&2
  echo "Run: cargo build --release -p polymarket-exec" >&2
  exit 1
fi

echo "Using binary: $BIN"
exec "$BIN"
