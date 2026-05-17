#!/usr/bin/env bash
# Polls live service health, cash, and order-store activity; alerts on Telegram.
set -uo pipefail

source ~/.config/polymarket-watchdog/telegram.env

SERVICE_UNIT="${SERVICE%.service}.service"
SLEEVE="${SERVICE_UNIT#polymarket-exec@}"
SLEEVE="${SLEEVE%.service}"
ORDER_STORE="${ORDER_STORE:-/home/ubuntu/go/polymarket-agent/data/runtime/${SLEEVE}/order-store.sqlite}"
ACTIVITY_ALERTS="${ACTIVITY_ALERTS:-1}"
ACTIVITY_MAX_ROWS="${ACTIVITY_MAX_ROWS:-12}"

mkdir -p "$DEDUPE_DIR"

now_epoch=$(date +%s)
LOG=$(journalctl --user -u "$SERVICE_UNIT" --since "5 min ago" --no-pager 2>/dev/null || true)
LOG60=$(journalctl --user -u "$SERVICE_UNIT" --since "60 min ago" --no-pager 2>/dev/null || true)

send_telegram() {
  local text="$1"
  curl -s "https://api.telegram.org/bot${TG_TOKEN}/sendMessage" \
    -d chat_id="${TG_CHAT_ID}" \
    --data-urlencode "text=${text}" >/dev/null
}

send_alert() {
  local trigger="$1"
  local body="$2"
  local last_file="$DEDUPE_DIR/${SLEEVE}.${trigger}.last"
  if [[ -f "$last_file" ]]; then
    local last
    last=$(cat "$last_file")
    local age=$((now_epoch - last))
    if (( age < DEDUPE_MIN * 60 )); then
      return 0
    fi
  fi
  send_telegram "ALERT [${trigger}] @ ${HOST_LABEL}
${body}"
  echo "$now_epoch" > "$last_file"
}

alert_activity() {
  [[ "$ACTIVITY_ALERTS" != "1" ]] && return 0
  command -v sqlite3 >/dev/null 2>&1 || return 0
  [[ -f "$ORDER_STORE" ]] || return 0

  local state_file="$DEDUPE_DIR/${SLEEVE}.activity_last_ms"
  local max_seen
  max_seen=$(sqlite3 "$ORDER_STORE" "select coalesce(max(last_update_ms), 0) from orders;" 2>/dev/null || echo 0)
  max_seen="${max_seen:-0}"

  # First run only arms the cursor. This avoids flooding Telegram with old fills.
  if [[ ! -f "$state_file" ]]; then
    echo "$max_seen" > "$state_file"
    return 0
  fi

  local last_seen
  last_seen=$(cat "$state_file" 2>/dev/null || echo 0)
  last_seen="${last_seen:-0}"
  [[ "$max_seen" =~ ^[0-9]+$ ]] || return 0
  [[ "$last_seen" =~ ^[0-9]+$ ]] || last_seen=0
  (( max_seen > last_seen )) || return 0

  local rows
  rows=$(sqlite3 -separator $'\t' "$ORDER_STORE" "
    select
      last_update_ms,
      status,
      market_id,
      side,
      printf('%.4f', limit_price),
      printf('%.2f', filled_qty),
      printf('%.2f', original_qty),
      accounting_lane,
      replace(substr(coalesce(reason, ''), 1, 180), char(10), ' ')
    from orders
    where last_update_ms > ${last_seen}
      and status in ('Filled', 'Rejected')
    order by last_update_ms asc
    limit ${ACTIVITY_MAX_ROWS};
  " 2>/dev/null || true)

  echo "$max_seen" > "$state_file"
  [[ -n "$rows" ]] || return 0

  local body=""
  local row ts_ms status market_id side px filled original lane reason leg asset ts
  while IFS=$'\t' read -r ts_ms status market_id side px filled original lane reason; do
    [[ -n "${ts_ms:-}" ]] || continue
    leg=$(printf "%s" "$reason" | grep -oE "leg=(Yes|No)" | head -1 | cut -d= -f2)
    case "$leg" in
      Yes) asset="UP" ;;
      No) asset="DOWN" ;;
      *) asset="?" ;;
    esac
    ts=$(date -u -d "@$((ts_ms / 1000))" "+%H:%M:%S" 2>/dev/null || echo "${ts_ms}")
    body+="${ts} ${status} ${lane} ${asset} ${side} ${filled}/${original} @ ${px} market=${market_id}"$'\n'
  done <<< "$rows"

  [[ -n "$body" ]] || return 0
  send_telegram "poly activity [${SLEEVE}] @ ${HOST_LABEL}
${body}"
}

# 1. service health
state=$(systemctl --user is-active "$SERVICE_UNIT" 2>/dev/null || echo unknown)
if [[ "$state" != "active" ]]; then
  enter_ts=$(systemctl --user show -p ActiveEnterTimestamp --value "$SERVICE_UNIT" 2>/dev/null || echo "")
  send_alert "service-down" "service=${SERVICE_UNIT} state=${state} (last active=${enter_ts})"
fi

# 2. venue_cash floor
last_cash_line=$(echo "$LOG" | grep "synced venue balance" | tail -1)
cash=$(echo "$last_cash_line" | grep -oE "venue_cash_usd=[0-9.]+" | cut -d= -f2)
if [[ -n "$cash" ]]; then
  cash_int=$(printf "%.0f" "$cash")
  if (( cash_int < CASH_FLOOR_USD )); then
    send_alert "cash-low" "venue_cash=\$${cash} below floor \$${CASH_FLOOR_USD}"
  fi
fi

# 3. Bonereaper order activity.
alert_activity

# 4. Legacy rescue rejection rate. Harmless when the active strategy has no rescue lane.
attempts=$(echo "$LOG60" | grep -c "rescue intent built\|mm-hedge-rescue.*PendingSubmit")
rejects=$(echo "$LOG60" | grep -c "mm-hedge-rescue.*to=Rejected")
if (( attempts >= RESCUE_MIN_ATTEMPTS_FOR_RATE && attempts > 0 )); then
  rate=$(( 100 * rejects / attempts ))
  if (( rate > RESCUE_REJECT_RATE_MAX )); then
    send_alert "rescue-failing" "rescue reject rate ${rate}% (${rejects}/${attempts}) over last 60min"
  fi
fi
