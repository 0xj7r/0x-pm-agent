#!/usr/bin/env bash
# Posts a single end-of-day rollup to Telegram.
set -uo pipefail

source ~/.config/polymarket-watchdog/telegram.env

SERVICE_UNIT="${SERVICE%.service}.service"
SLEEVE="${SERVICE_UNIT#polymarket-exec@}"
SLEEVE="${SLEEVE%.service}"
ORDER_STORE="${ORDER_STORE:-/home/ubuntu/go/polymarket-agent/data/runtime/${SLEEVE}/order-store.sqlite}"

LOG24=$(journalctl --user -u "$SERVICE_UNIT" --since "24 hours ago" --no-pager 2>/dev/null || true)
LATEST=$(echo "$LOG24" | grep "synced venue balance" | tail -1)
EARLIEST=$(echo "$LOG24" | grep "synced venue balance" | head -1)
cash_now=$(echo "$LATEST" | grep -oE "venue_cash_usd=[0-9.]+" | cut -d= -f2)
cash_then=$(echo "$EARLIEST" | grep -oE "venue_cash_usd=[0-9.]+" | cut -d= -f2)
positions=$(echo "$LATEST" | grep -oE "venue_position_count=[0-9]+" | cut -d= -f2)
cancel_races=$(echo "$LOG24" | grep -c "matched orders can.t be canceled")
regimes=$(echo "$LOG24" | grep -oE "regime=Some\([A-Za-z]+\)" | sort | uniq -c | sort -rn | head -3 | awk "{print \$2,\$1}" | tr "\n" " ")

fills=$(echo "$LOG24" | grep -c "to=Filled")
rejects=$(echo "$LOG24" | grep -c "to=Rejected")
filled_notional="0.00"
lane_mix=""
if command -v sqlite3 >/dev/null 2>&1 && [[ -f "$ORDER_STORE" ]]; then
  since_ms=$(($(date -u -d "24 hours ago" +%s) * 1000))
  fills=$(sqlite3 "$ORDER_STORE" "select count(*) from orders where status='Filled' and last_update_ms >= ${since_ms};" 2>/dev/null || echo "$fills")
  rejects=$(sqlite3 "$ORDER_STORE" "select count(*) from orders where status='Rejected' and last_update_ms >= ${since_ms};" 2>/dev/null || echo "$rejects")
  filled_notional=$(sqlite3 "$ORDER_STORE" "select printf('%.2f', coalesce(sum(filled_qty * limit_price), 0.0)) from orders where status='Filled' and last_update_ms >= ${since_ms};" 2>/dev/null || echo "0.00")
  lane_mix=$(sqlite3 -separator " " "$ORDER_STORE" "select accounting_lane || ':' || count(*) from orders where status='Filled' and last_update_ms >= ${since_ms} group by accounting_lane order by count(*) desc;" 2>/dev/null | tr "\n" " " || true)
fi

rescue_attempts=$(echo "$LOG24" | grep -c "rescue intent built\|mm-hedge-rescue.*PendingSubmit")
rescue_rejects=$(echo "$LOG24" | grep -c "mm-hedge-rescue.*to=Rejected")
rescue_skips=$(echo "$LOG24" | grep -c "rescue skipped")

delta="?"
if [[ -n "$cash_now" && -n "$cash_then" ]]; then
  delta=$(python3 -c "print(f\"{${cash_now}-${cash_then}:+.2f}\")")
fi

rescue_success_rate="n/a"
if (( rescue_attempts > 0 )); then
  rescue_success_rate=$((100 * (rescue_attempts - rescue_rejects) / rescue_attempts))%
fi

curl -s "https://api.telegram.org/bot${TG_TOKEN}/sendMessage" \
  -d chat_id="${TG_CHAT_ID}" \
  --data-urlencode "text=poly daily summary [${SLEEVE}] @ ${HOST_LABEL}
cash: \$${cash_now} (24h delta ${delta})
positions: ${positions}
fills: ${fills}  rejects: ${rejects}  filled notional: \$${filled_notional}
lane mix: ${lane_mix}
cancel-races: ${cancel_races}
rescue: ${rescue_attempts} attempts, ${rescue_rejects} rejected, ${rescue_skips} skipped (success rate: ${rescue_success_rate})
regime mix: ${regimes}" >/dev/null
