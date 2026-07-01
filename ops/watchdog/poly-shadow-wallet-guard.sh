#!/usr/bin/env bash
# Wallet floor circuit breaker for shadow_exec_tail live trading.
# Trips the operator kill switch when CLOB tradable cash falls below floor.
#
# Disabled unless WALLET_GUARD_ENABLED=1 (tight $1K floor trips on normal
# variance and amputates recoveries — see autoloop ledger F10 / daily CB REJECT).
# To arm: WALLET_GUARD_ENABLED=1 WALLET_FLOOR_USD=500 crontab ...
set -uo pipefail

if [[ "${WALLET_GUARD_ENABLED:-0}" != "1" ]]; then
  exit 0
fi

source ~/.config/polymarket-watchdog/telegram.env 2>/dev/null || true

WALLET_ENV="${WALLET_ENV:-$HOME/.config/polymarket-exec/wallet.env}"
SHADOW_ENV="${SHADOW_ENV:-$HOME/.config/polymarket-exec/shadow_exec_tail.env}"
KILL_SWITCH="${KILL_SWITCH:-$HOME/fade.kill}"
BALANCE_BIN="${BALANCE_BIN:-$HOME/deploy-main/polymarket-agent/target/release/balance_once}"
WALLET_FLOOR_USD="${WALLET_FLOOR_USD:-500}"
DEDUPE_DIR="${DEDUPE_DIR:-/tmp/poly-watchdog}"
STATE_FILE="${DEDUPE_DIR}/shadow.wallet_guard.last"
DEDUPE_MIN="${DEDUPE_MIN:-30}"

mkdir -p "${DEDUPE_DIR}"

log() { echo "[shadow-wallet-guard] $(date -u +%H:%M:%S) $*"; }

tg_alert() {
  [[ -z "${TG_TOKEN:-}" ]] && return 0
  curl -s "https://api.telegram.org/bot${TG_TOKEN}/sendMessage" \
    -d chat_id="${TG_CHAT_ID}" --data-urlencode "text=shadow-wallet-guard
$1" >/dev/null
}

if [[ -f "${KILL_SWITCH}" ]]; then
  exit 0
fi

if ! pgrep -f "shadow_exec_tail" >/dev/null 2>&1; then
  exit 0
fi

if [[ ! -x "${BALANCE_BIN}" ]]; then
  log "balance_once missing at ${BALANCE_BIN}"
  exit 1
fi

if [[ ! -f "${WALLET_ENV}" ]]; then
  log "wallet env missing: ${WALLET_ENV}"
  exit 1
fi

set -a
# shellcheck disable=SC1090
source "${WALLET_ENV}"
if [[ -f "${SHADOW_ENV}" ]]; then
  # shellcheck disable=SC1090
  source "${SHADOW_ENV}"
fi
set +a

cash_raw=$("${BALANCE_BIN}" 2>/dev/null || true)
if [[ -z "${cash_raw}" ]] || ! [[ "${cash_raw}" =~ ^[0-9]+(\.[0-9]+)?$ ]]; then
  log "balance probe failed (raw='${cash_raw}')"
  exit 1
fi

cash_int=$(printf "%.0f" "${cash_raw}")
log "venue_cash=\$${cash_raw} floor=\$${WALLET_FLOOR_USD}"

if (( cash_int >= WALLET_FLOOR_USD )); then
  exit 0
fi

now_epoch=$(date +%s)
if [[ -f "${STATE_FILE}" ]]; then
  last=$(cat "${STATE_FILE}")
  age=$((now_epoch - last))
  if (( age < DEDUPE_MIN * 60 )); then
    exit 0
  fi
fi

log "CIRCUIT BREAKER: venue_cash=\$${cash_raw} < floor \$${WALLET_FLOOR_USD}"
echo "${now_epoch}" > "${STATE_FILE}"
touch "${KILL_SWITCH}"

tg_alert "CRITICAL wallet floor breached

venue_cash=\$${cash_raw}
floor=\$${WALLET_FLOOR_USD}

KILL SWITCH ENGAGED (${KILL_SWITCH})
New LIVE ENTER legs blocked until you investigate.

Recovery:
  1. ssh in, inspect ~/data/pm-alpha/shadow_exec_tail.log
  2. Redeem any open positions if needed (redeem_once)
  3. Debug, then: rm ${KILL_SWITCH}"