#!/usr/bin/env bash
# Spot-feed staleness watchdog. Engages the operator kill switch when the active
# live strategy stops producing usable BTC volatility readings.
set -uo pipefail

source ~/.config/polymarket-watchdog/telegram.env 2>/dev/null || true

SERVICE_UNIT="${SERVICE%.service}.service"
SLEEVE="${SERVICE_UNIT#polymarket-exec@}"
SLEEVE="${SLEEVE%.service}"
KILL_SWITCH="${HOME}/.config/polymarket-exec/live.kill"
DEDUPE_DIR="${DEDUPE_DIR:-/tmp/poly-watchdog}"
STALENESS_FILE="${DEDUPE_DIR}/${SLEEVE}.spot_staleness.last"
DEDUPE_MIN="${DEDUPE_MIN:-60}"
NONE_THRESHOLD_PCT="${NONE_THRESHOLD_PCT:-80}"
mkdir -p "${DEDUPE_DIR}"

log() { echo "[poly-staleness] $(date -u +%H:%M:%S) $*"; }

tg_alert() {
  [[ -z "${TG_TOKEN:-}" ]] && return 0
  curl -s "https://api.telegram.org/bot${TG_TOKEN}/sendMessage" \
    -d chat_id="${TG_CHAT_ID}" --data-urlencode "text=poly-staleness [${SLEEVE}]
$1" >/dev/null
}

# Skip if service not active.
[[ "$(systemctl --user is-active "${SERVICE_UNIT}" 2>/dev/null)" != "active" ]] && exit 0

# Skip if kill switch already engaged.
if [[ -f "${KILL_SWITCH}" ]]; then
  exit 0
fi

LOG60=$(journalctl --user -u "${SERVICE_UNIT}" --since "60 sec ago" --no-pager 2>/dev/null || true)
LOG5MIN=$(journalctl --user -u "${SERVICE_UNIT}" --since "5 min ago" --no-pager 2>/dev/null || true)

total=$(printf "%s" "${LOG60}" | grep -Ec "(btc_)?vol_5m_bps=" 2>/dev/null) || total=0
none=$(printf "%s" "${LOG60}" | grep -Ec "(btc_)?vol_5m_bps=None" 2>/dev/null) || none=0
some_5min=$(printf "%s" "${LOG5MIN}" | grep -Ec "(btc_)?vol_5m_bps=Some\\(" 2>/dev/null) || some_5min=0
total=${total:-0}
none=${none:-0}
some_5min=${some_5min:-0}

# Need enough samples to evaluate.
(( total < 10 )) && exit 0

pct=$(( 100 * none / total ))
trigger=""
if (( pct >= NONE_THRESHOLD_PCT )) && (( total >= 30 )); then
  trigger="vol=None ${pct}% of last 60s (${none}/${total})"
elif (( some_5min == 0 )); then
  trigger="zero vol=Some readings in last 5 min"
fi
[[ -z "${trigger}" ]] && exit 0

now_epoch=$(date +%s)
if [[ -f "${STALENESS_FILE}" ]]; then
  last=$(cat "${STALENESS_FILE}")
  age=$((now_epoch - last))
  if (( age < DEDUPE_MIN * 60 )); then
    exit 0
  fi
fi

log "STALENESS DETECTED: ${trigger}"
log "engaging kill switch (no auto-recovery): ${KILL_SWITCH}"
echo "${now_epoch}" > "${STALENESS_FILE}"
touch "${KILL_SWITCH}"

tg_alert "CRITICAL spot-feed staleness detected (${trigger})

KILL SWITCH ENGAGED. Bot will not emit new entries until you investigate.

Manual recovery:
  1. ssh in, run: bash ~/.local/bin/poly-safe-restart.sh ${SLEEVE}
  2. Verify btc_vol_5m_bps=Some readings resume in journal
  3. Remove kill switch: rm ~/.config/polymarket-exec/live.kill"
