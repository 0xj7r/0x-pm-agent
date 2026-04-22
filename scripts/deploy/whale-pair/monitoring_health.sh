#!/usr/bin/env bash
set -euo pipefail

REPO_ROOT="${REPO_ROOT:-$(cd "$(dirname "$0")/../../.." && pwd)}"
COMPOSE_FILE="$REPO_ROOT/scripts/deploy/whale-pair/docker-compose.monitoring.whale-pair.yml"
PROM_URL="${PROM_URL:-http://127.0.0.1:${WHALE_PAIR_PROMETHEUS_PORT:-9090}}"
GRAFANA_URL="${GRAFANA_URL:-http://127.0.0.1:${WHALE_PAIR_GRAFANA_PORT:-3000}}"
METRICS_FILE="${METRICS_FILE:-$REPO_ROOT/data/monitoring/node-exporter/whale_pair.prom}"

fail() {
  echo "UNHEALTHY: $1" >&2
  exit 1
}

for service in prometheus grafana node-exporter cadvisor; do
  if ! docker compose -f "$COMPOSE_FILE" ps --status running 2>/dev/null | grep -q "$service"; then
    fail "$service is not running"
  fi
done

curl -fsS "$PROM_URL/-/ready" >/dev/null || fail "prometheus not ready at $PROM_URL"
curl -fsS "$GRAFANA_URL/api/health" >/dev/null || fail "grafana not healthy at $GRAFANA_URL"

[ -f "$METRICS_FILE" ] || fail "textfile metrics missing: $METRICS_FILE"

if [ "$(uname)" = "Linux" ]; then
  MTIME=$(stat -c '%Y' "$METRICS_FILE")
else
  MTIME=$(stat -f '%m' "$METRICS_FILE")
fi
NOW=$(date +%s)
AGE=$((NOW - MTIME))
if [ "$AGE" -gt 120 ]; then
  fail "textfile metrics are stale (${AGE}s old)"
fi

echo "HEALTHY: monitoring stack ready and textfile metrics fresh"
