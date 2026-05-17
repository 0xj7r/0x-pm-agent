#!/usr/bin/env bash
# Safe restart wrapper for polymarket-exec service.
set -uo pipefail

source ~/.config/polymarket-watchdog/telegram.env 2>/dev/null || true

DEFAULT_SERVICE="${SERVICE:-polymarket-exec@bonereaper_tinylive.service}"
DEFAULT_SERVICE="${DEFAULT_SERVICE%.service}.service"
DEFAULT_SLEEVE="${DEFAULT_SERVICE#polymarket-exec@}"
DEFAULT_SLEEVE="${DEFAULT_SLEEVE%.service}"
SLEEVE="${1:-${DEFAULT_SLEEVE}}"
SERVICE="polymarket-exec@${SLEEVE}.service"
KILL_SWITCH="${HOME}/.config/polymarket-exec/live.kill"
DRAIN_TIMEOUT_S=30
DRAIN_QUIET_S=10

log() { echo "[poly-safe-restart] $(date -u +%H:%M:%S) $*"; }

tg_alert() {
  [[ -z "${TG_TOKEN:-}" ]] && return 0
  curl -s "https://api.telegram.org/bot${TG_TOKEN}/sendMessage" \
    -d chat_id="${TG_CHAT_ID}" --data-urlencode "text=poly-safe-restart [${SLEEVE}]
$1" >/dev/null
}

active=$(systemctl --user is-active "${SERVICE}" 2>/dev/null || echo unknown)
log "service state pre-restart: ${active}"

if [[ "${active}" != "active" ]]; then
  log "service not active; doing plain restart without drain"
  systemctl --user restart "${SERVICE}"
  exit 0
fi

log "engaging kill switch -> ${KILL_SWITCH}"
mkdir -p "$(dirname "${KILL_SWITCH}")"
touch "${KILL_SWITCH}"

log "waiting up to ${DRAIN_TIMEOUT_S}s for merge drain (need ${DRAIN_QUIET_S}s of quiet)"
deadline=$(($(date +%s) + DRAIN_TIMEOUT_S))
quiet_streak=0
last_merge_count=0
while (( $(date +%s) < deadline )); do
  recent=$(journalctl --user -u "${SERVICE}" --since "${DRAIN_QUIET_S} sec ago" --no-pager 2>/dev/null \
    | grep -c "merge command accepted\|relayer merge submitted")
  if (( recent == 0 )); then
    quiet_streak=$((quiet_streak + 1))
    if (( quiet_streak >= 3 )); then
      log "${DRAIN_QUIET_S}s quiet of merge activity -> drained"
      break
    fi
  else
    quiet_streak=0
  fi
  last_merge_count=${recent}
  sleep 1
done
if (( $(date +%s) >= deadline )); then
  log "drain timeout reached (last_recent_merges=${last_merge_count}); proceeding anyway"
fi

log "issuing systemctl restart ${SERVICE}"
systemctl --user restart "${SERVICE}"
sleep 4

new_active=$(systemctl --user is-active "${SERVICE}" 2>/dev/null || echo unknown)
log "service state post-restart: ${new_active}"
if [[ "${new_active}" == "active" ]]; then
  rm -f "${KILL_SWITCH}"
  log "kill switch removed; restart complete"
  tg_alert "safe restart OK; merge drain wait then service active again"
else
  log "service NOT active after restart (state=${new_active}); leaving kill switch ENGAGED"
  tg_alert "WARNING safe restart did not bring service active (state=${new_active}); kill switch still on, manual intervention needed"
  exit 1
fi
