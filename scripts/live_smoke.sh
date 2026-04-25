#!/usr/bin/env bash
# Live smoke test wrapper.
# Submits a single tiny ($1 notional, $0.01 price) post-only order
# against the venue, verifies it's accepted + visible in open-orders,
# then cancels it. Validates auth/signing end-to-end without risking
# meaningful capital.
set -eo pipefail
cd "$(dirname "$0")/.."

if [[ ! -f .env ]]; then
  echo "ERROR: .env not found" >&2
  exit 1
fi

# Safe .env loader (no shell interpretation)
while IFS='=' read -r raw_key raw_value || [[ -n "$raw_key" ]]; do
  key="${raw_key%%[[:space:]]*}"
  [[ -z "$key" || "$key" =~ ^# ]] && continue
  [[ ! "$key" =~ ^[A-Za-z_][A-Za-z0-9_]*$ ]] && continue
  value="${raw_value%\"}"
  value="${value#\"}"
  value="${value%\'}"
  value="${value#\'}"
  export "$key=$value"
done < .env

# Get a current asset to smoke against
python3 scripts/export_btc_5m_runtime.py \
  --context-out /tmp/live_smoke_ctx.json \
  --env-out /tmp/live_smoke.env > /dev/null

# Pull first asset id from runtime env
ASSET_ID=$(grep WHALE_PAIR_ASSET_IDS /tmp/live_smoke.env | head -1 | sed 's/WHALE_PAIR_ASSET_IDS=//' | cut -d, -f1)
MARKET_ID=$(grep WHALE_PAIR_USER_MARKETS /tmp/live_smoke.env | head -1 | sed 's/WHALE_PAIR_USER_MARKETS=//' | cut -d, -f1)

echo ">>> SMOKE TEST: asset=$ASSET_ID market=$MARKET_ID"
echo ">>> Will submit: BUY $0.01 × 100 shares ($1 notional, post-only) then immediately cancel"

export WHALE_PAIR_EXEC_MODE=live_smoke
export WHALE_PAIR_PAPER_MODE=false
export WHALE_PAIR_LIVE_SMOKE_ASSET_ID=$ASSET_ID
export WHALE_PAIR_LIVE_SMOKE_MARKET_ID=$MARKET_ID
export WHALE_PAIR_LIVE_SMOKE_PRICE=0.01
export WHALE_PAIR_LIVE_SMOKE_NOTIONAL_USD=1
# Also need standard config knobs
source /tmp/live_smoke.env
export WHALE_PAIR_ASSET_IDS WHALE_PAIR_INSTRUMENT_MARKETS WHALE_PAIR_USER_MARKETS
# Match tinylive risk caps
export WHALE_PAIR_STRATEGY=btc_5m_mm
export WHALE_PAIR_EXEC_STARTING_CASH_USD=100
export WHALE_PAIR_EXEC_MAX_GROSS_NOTIONAL_USD=80
export WHALE_PAIR_EXEC_MIN_FREE_CASH_USD=10
export WHALE_PAIR_EXEC_MAX_OPEN_ORDERS_TOTAL=4

BIN=$(pwd)/target/release/polymarket-exec
exec "$BIN"
