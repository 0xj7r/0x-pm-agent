#!/usr/bin/env bash
set -euo pipefail

REPO_ROOT="${REPO_ROOT:-$(cd "$(dirname "$0")/../../.." && pwd)}"
COMPOSE_FILE="$REPO_ROOT/scripts/deploy/whale-pair/docker-compose.monitoring.whale-pair.yml"

mkdir -p \
  "$REPO_ROOT/data/monitoring/prometheus" \
  "$REPO_ROOT/data/monitoring/grafana" \
  "$REPO_ROOT/data/monitoring/node-exporter"

bash "$REPO_ROOT/scripts/deploy/whale-pair/refresh_monitoring_metrics.sh"
docker compose -f "$COMPOSE_FILE" up -d

echo ""
echo "Monitoring stack started."
echo "Prometheus: http://127.0.0.1:${WHALE_PAIR_PROMETHEUS_PORT:-9090}"
echo "Grafana:    http://127.0.0.1:${WHALE_PAIR_GRAFANA_PORT:-3000}"
