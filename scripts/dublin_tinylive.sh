#!/usr/bin/env bash
# Live tinylive deploy on V1 endpoint with USDC.e collateral.
# - EOA wallet 0x97fBC6Bc... ($102 USDC.e + 105 MATIC available)
# - btc_5m_mm strategy with tuned knobs (edge=25, age=2s, safety=1)
# - $50 starting cash, $25 max gross notional (TIGHT caps)
# - 2 max open orders total (very small position)
# - Kill switch: touch ~/.config/polymarket-exec/live.kill
set -uo pipefail
cd ~/go/polymarket-agent

# Safe .env loader
while IFS='=' read -r raw_key raw_value || [[ -n "$raw_key" ]]; do
  key="${raw_key%%[[:space:]]*}"
  [[ -z "$key" || "$key" =~ ^# ]] && continue
  [[ ! "$key" =~ ^[A-Za-z_][A-Za-z0-9_]*$ ]] && continue
  value="${raw_value%\"}"; value="${value#\"}"
  value="${value%\'}"; value="${value#\'}"
  export "$key=$value"
done < .env

# EOA mode (proxy not funded; whale uses proxy but we need to wrap pUSD for V2 first)
export POLYMARKET_SIGNATURE_TYPE=eoa
unset POLYMARKET_FUNDER_ADDRESS POLYMARKET_FUNDER
export POLYMARKET_FUNDER_ADDRESS='' POLYMARKET_FUNDER=''

# V1 endpoint — works today with USDC.e until Apr 28 cutover
export POLYMARKET_CLOB_API_URL=https://clob.polymarket.com
export POLYMARKET_CLOB_VERSION=v1
export POLYMARKET_COLLATERAL_TOKEN_ADDRESS=0x2791Bca1f2de4661ED88A30C99A7a9449Aa84174

# Required for EOA-mode CTF merge/redeem (signs + submits direct to Polygon).
export POLYGON_RPC_URL=${POLYGON_RPC_URL:-https://polygon-bor-rpc.publicnode.com}

# Strategy + tuning
export WHALE_PAIR_STRATEGY=btc_5m_mm
export WHALE_PAIR_QUOTE_MIN_ORDER_AGE_MS=8000
export WHALE_PAIR_BTC_5M_MM_MIN_EDGE_BPS=25
export WHALE_PAIR_BTC_5M_MM_MAKER_SAFETY_TICKS=1
export WHALE_PAIR_BTC_5M_MM_BASE_CLIP_USD=1.10
export WHALE_PAIR_BTC_5M_MM_MIN_CLIP_USD=0.25
export WHALE_PAIR_BTC_5M_MM_MAX_CLIP_USD=8.0
export WHALE_PAIR_BTC_5M_MM_MIN_EDGE_BPS=25
export WHALE_PAIR_BTC_5M_MM_LIQUIDITY_CLIP_FRACTION=0.02

# Anti-churn: longer cooldown + more aggressive hedge rescue.
# Without these the bot reposts every ~1s and chases the trending leg
# (e.g. 4 UP fills as BTC ticked up while DOWN bid kept getting cancelled).
export WHALE_PAIR_BTC_5M_MM_COOLDOWN_MS=2000
export WHALE_PAIR_BTC_5M_MM_HEDGE_RESCUE_CLIP_USD=5.00
export WHALE_PAIR_BTC_5M_MM_HEDGE_RESCUE_EDGE_BPS=10

# TIGHT risk caps
export WHALE_PAIR_EXEC_STARTING_CASH_USD=50
export WHALE_PAIR_EXEC_MAX_GROSS_NOTIONAL_USD=25
export WHALE_PAIR_EXEC_MAX_NET_NOTIONAL_PER_MARKET_USD=10
export WHALE_PAIR_EXEC_MAX_POSITION_QUANTITY_PER_INSTRUMENT=60
export WHALE_PAIR_EXEC_MIN_FREE_CASH_USD=5
export WHALE_PAIR_EXEC_MAX_OPEN_ORDERS_TOTAL=2
export WHALE_PAIR_EXEC_MAX_OPEN_ORDERS_PER_MARKET=2
export WHALE_PAIR_LIVE_MAX_CANCEL_ERRORS=3
# Default WHALE_PAIR_LIVE_MAX_SUBMIT_ERRORS=1 freezes the bot on a single
# startup race. 10 absorbs the unavoidable rejections during book chase.
export WHALE_PAIR_LIVE_MAX_SUBMIT_ERRORS=10
# Faster reconcile so first-merge-per-market warmup drops from 15s to 3s.
# Each tick is one cheap data-api positions call. After warmup, merges are
# fill-driven (~3-6s end-to-end including chain).
export WHALE_PAIR_ORDER_RECONCILE_INTERVAL_MS=3000

# Kill switch path
export WHALE_PAIR_LIVE_KILL_SWITCH_PATH=$HOME/.config/polymarket-exec/live.kill
mkdir -p ~/.config/polymarket-exec

# Live mode
export WHALE_PAIR_PAPER_MODE=false
unset WHALE_PAIR_EXEC_MODE

# Refresh active markets
python3 scripts/export_btc_5m_runtime.py \
  --context-out /tmp/tinylive_ctx.json \
  --env-out /tmp/tinylive_runtime.env > /dev/null
source /tmp/tinylive_runtime.env
export WHALE_PAIR_ASSET_IDS WHALE_PAIR_INSTRUMENT_MARKETS WHALE_PAIR_USER_MARKETS
export WHALE_PAIR_EXEC_MARKET_CONTEXT_PATH=/tmp/tinylive_ctx.json

# Persistent journal/order store paths
mkdir -p ~/.local/share/polymarket-exec/tinylive
export WHALE_PAIR_JOURNAL_PATH=~/.local/share/polymarket-exec/tinylive/journal-$(date +%Y%m%d-%H%M%S).jsonl
export WHALE_PAIR_ORDER_STORE_PATH=~/.local/share/polymarket-exec/tinylive/orders.sqlite

export RUST_LOG=info,paper_fill_gate=warn

echo ">>> LIVE TINYLIVE LAUNCH at $(date -u +%H:%M:%S)"
echo ">>> wallet: 0x97fBC6Bc... | endpoint: V1 | collateral: USDC.e"
echo ">>> strategy: btc_5m_mm | edge=25bps | quote_age=2s | safety=1tick"
echo ">>> caps: cash=\$50 max_gross=\$25 max_orders=2"
echo ">>> kill: touch ~/.config/polymarket-exec/live.kill"
exec target/release/polymarket-exec
