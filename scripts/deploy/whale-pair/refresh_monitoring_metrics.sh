#!/usr/bin/env bash
set -euo pipefail

REPO_ROOT="${REPO_ROOT:-$(cd "$(dirname "$0")/../../.." && pwd)}"
APP_COMPOSE_FILE="${APP_COMPOSE_FILE:-$REPO_ROOT/scripts/deploy/whale-pair/docker-compose.whale-pair.yml}"
TEXTFILE_DIR="${TEXTFILE_DIR:-$REPO_ROOT/data/monitoring/node-exporter}"
METRICS_FILE="$TEXTFILE_DIR/whale_pair.prom"
TMP_FILE="$METRICS_FILE.$$"
LOG_WINDOW_SECONDS="${LOG_WINDOW_SECONDS:-120}"
RECENT_LOG_SECONDS="${RECENT_LOG_SECONDS:-60}"
SERVICE_NAMES="${WHALE_PAIR_SERVICE_NAMES:-whale-pair-live whale-pair-shadow}"

mkdir -p "$TEXTFILE_DIR"

escape_label() {
  printf '%s' "$1" | sed 's/\\/\\\\/g; s/"/\\"/g'
}

ledger_for_service() {
  case "$1" in
    *shadow*)
      printf '%s\n' "${WHALE_PAIR_SHADOW_LEDGER:-$REPO_ROOT/data/whale_pair_shadow.db}"
      ;;
    *live*)
      printf '%s\n' "${WHALE_PAIR_LIVE_LEDGER:-$REPO_ROOT/data/whale_pair_live.db}"
      ;;
    *)
      printf '%s\n' "${WHALE_PAIR_DEFAULT_LEDGER:-$REPO_ROOT/data/${1//-/_}.db}"
      ;;
  esac
}

echo "# HELP whale_pair_monitoring_last_run_timestamp_seconds Unix timestamp of the last monitoring textfile refresh." >"$TMP_FILE"
echo "# TYPE whale_pair_monitoring_last_run_timestamp_seconds gauge" >>"$TMP_FILE"
echo "whale_pair_monitoring_last_run_timestamp_seconds $(date +%s)" >>"$TMP_FILE"

echo "# HELP whale_pair_service_up Whether the whale-pair service container is running." >>"$TMP_FILE"
echo "# TYPE whale_pair_service_up gauge" >>"$TMP_FILE"
echo "# HELP whale_pair_service_restart_count Docker restart count for the whale-pair service." >>"$TMP_FILE"
echo "# TYPE whale_pair_service_restart_count gauge" >>"$TMP_FILE"
echo "# HELP whale_pair_service_recent_log_lines Number of recent log lines emitted by the whale-pair service." >>"$TMP_FILE"
echo "# TYPE whale_pair_service_recent_log_lines gauge" >>"$TMP_FILE"
echo "# HELP whale_pair_service_has_book_telemetry Whether recent logs contained book_age_ms telemetry." >>"$TMP_FILE"
echo "# TYPE whale_pair_service_has_book_telemetry gauge" >>"$TMP_FILE"
echo "# HELP whale_pair_service_book_age_avg_ms Average recent book_age_ms across parsed log samples." >>"$TMP_FILE"
echo "# TYPE whale_pair_service_book_age_avg_ms gauge" >>"$TMP_FILE"
echo "# HELP whale_pair_service_book_age_max_ms Max recent book_age_ms across parsed log samples." >>"$TMP_FILE"
echo "# TYPE whale_pair_service_book_age_max_ms gauge" >>"$TMP_FILE"
echo "# HELP whale_pair_service_book_age_samples Number of parsed book_age_ms samples." >>"$TMP_FILE"
echo "# TYPE whale_pair_service_book_age_samples gauge" >>"$TMP_FILE"
echo "# HELP whale_pair_service_ledger_present Whether the expected ledger file exists." >>"$TMP_FILE"
echo "# TYPE whale_pair_service_ledger_present gauge" >>"$TMP_FILE"
echo "# HELP whale_pair_service_ledger_age_seconds Seconds since the expected ledger file was modified." >>"$TMP_FILE"
echo "# TYPE whale_pair_service_ledger_age_seconds gauge" >>"$TMP_FILE"
echo "# HELP whale_pair_service_metrics_collection_success Whether monitoring collection succeeded for the service." >>"$TMP_FILE"
echo "# TYPE whale_pair_service_metrics_collection_success gauge" >>"$TMP_FILE"

for service in $SERVICE_NAMES; do
  label_service="$(escape_label "$service")"
  success=1
  container_id="$(docker compose -f "$APP_COMPOSE_FILE" ps -q "$service" 2>/dev/null || true)"
  running=0
  restart_count=0
  recent_lines=0
  has_book=0
  book_avg=0
  book_max=0
  book_samples=0

  if [ -n "$container_id" ]; then
    if docker inspect -f '{{.State.Running}}' "$container_id" 2>/dev/null | grep -q true; then
      running=1
    fi
    restart_count="$(docker inspect -f '{{.RestartCount}}' "$container_id" 2>/dev/null || echo 0)"
    logs_recent="$(docker compose -f "$APP_COMPOSE_FILE" logs --since "${RECENT_LOG_SECONDS}s" "$service" 2>/dev/null || true)"
    logs_window="$(docker compose -f "$APP_COMPOSE_FILE" logs --since "${LOG_WINDOW_SECONDS}s" "$service" 2>/dev/null || true)"
    if [ -n "$logs_recent" ]; then
      recent_lines="$(printf '%s\n' "$logs_recent" | wc -l | tr -d ' ')"
    fi
    book_ages="$(printf '%s\n' "$logs_window" | grep -oE '"book_age_ms":[ ]*[0-9.]+' | awk -F: '{print $2}' | tr -d ' ' || true)"
    if [ -n "$book_ages" ]; then
      has_book=1
      book_avg="$(printf '%s\n' "$book_ages" | awk 'BEGIN{s=0;n=0} {s+=$1;n+=1} END{if(n>0) printf "%.3f", s/n; else print "0"}')"
      book_max="$(printf '%s\n' "$book_ages" | awk 'BEGIN{m=0} {if($1>m) m=$1} END{printf "%.3f", m}')"
      book_samples="$(printf '%s\n' "$book_ages" | wc -l | tr -d ' ')"
    fi
  fi

  ledger="$(ledger_for_service "$service")"
  ledger_present=0
  ledger_age=-1
  if [ -f "$ledger" ]; then
    ledger_present=1
    if [ "$(uname)" = "Linux" ]; then
      ledger_mtime="$(stat -c '%Y' "$ledger")"
    else
      ledger_mtime="$(stat -f '%m' "$ledger")"
    fi
    ledger_age="$(($(date +%s) - ledger_mtime))"
  fi

  printf 'whale_pair_service_up{service="%s"} %s\n' "$label_service" "$running" >>"$TMP_FILE"
  printf 'whale_pair_service_restart_count{service="%s"} %s\n' "$label_service" "$restart_count" >>"$TMP_FILE"
  printf 'whale_pair_service_recent_log_lines{service="%s"} %s\n' "$label_service" "$recent_lines" >>"$TMP_FILE"
  printf 'whale_pair_service_has_book_telemetry{service="%s"} %s\n' "$label_service" "$has_book" >>"$TMP_FILE"
  printf 'whale_pair_service_book_age_avg_ms{service="%s"} %s\n' "$label_service" "$book_avg" >>"$TMP_FILE"
  printf 'whale_pair_service_book_age_max_ms{service="%s"} %s\n' "$label_service" "$book_max" >>"$TMP_FILE"
  printf 'whale_pair_service_book_age_samples{service="%s"} %s\n' "$label_service" "$book_samples" >>"$TMP_FILE"
  printf 'whale_pair_service_ledger_present{service="%s"} %s\n' "$label_service" "$ledger_present" >>"$TMP_FILE"
  printf 'whale_pair_service_ledger_age_seconds{service="%s"} %s\n' "$label_service" "$ledger_age" >>"$TMP_FILE"
  printf 'whale_pair_service_metrics_collection_success{service="%s"} %s\n' "$label_service" "$success" >>"$TMP_FILE"
done

mv "$TMP_FILE" "$METRICS_FILE"
