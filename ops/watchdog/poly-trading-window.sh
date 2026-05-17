#!/usr/bin/env bash
# Trading window scheduler: engages/clears the operator kill switch.
set -uo pipefail

source ~/.config/polymarket-watchdog/telegram.env 2>/dev/null || true
DEFAULT_SERVICE="${SERVICE:-polymarket-exec@bonereaper_tinylive.service}"
DEFAULT_SERVICE="${DEFAULT_SERVICE%.service}.service"
SLEEVE="${DEFAULT_SERVICE#polymarket-exec@}"
SLEEVE="${SLEEVE%.service}"
KILL_SWITCH="${HOME}/.config/polymarket-exec/live.kill"

action="${1:-}"
case "${action}" in
  on)
    if [[ -f "${KILL_SWITCH}" ]]; then
      echo "kill switch already engaged"
      exit 0
    fi
    touch "${KILL_SWITCH}"
    msg="trading window CLOSED [${SLEEVE}]
kill switch engaged at $(date -u +%H:%M)Z (= $(TZ=Europe/London date +%H:%M) London)
bot will not emit new entries during high-vol session.
manual override: rm ${KILL_SWITCH}"
    ;;
  off)
    if [[ ! -f "${KILL_SWITCH}" ]]; then
      echo "kill switch already cleared"
      exit 0
    fi
    rm -f "${KILL_SWITCH}"
    msg="trading window OPEN [${SLEEVE}]
kill switch cleared at $(date -u +%H:%M)Z (= $(TZ=Europe/London date +%H:%M) London)
bot resuming new entries for configured trading window.
manual override: touch ${KILL_SWITCH}"
    ;;
  *)
    echo "usage: $0 on|off"
    exit 1
    ;;
esac

echo "${msg}"
if [[ -n "${TG_TOKEN:-}" ]]; then
  curl -s "https://api.telegram.org/bot${TG_TOKEN}/sendMessage" \
    -d chat_id="${TG_CHAT_ID}" --data-urlencode "text=${msg}" >/dev/null
fi
