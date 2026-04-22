#!/usr/bin/env bash
set -euo pipefail

REPO_ROOT="${REPO_ROOT:-$(cd "$(dirname "$0")/../../.." && pwd)}"
COMPOSE_FILE="$REPO_ROOT/scripts/deploy/whale-pair/docker-compose.monitoring.whale-pair.yml"

run_host_fix() {
  if command -v sudo >/dev/null 2>&1; then
    sudo "$@"
  else
    "$@"
  fi
}

mkdir -p \
  "$REPO_ROOT/data/monitoring/prometheus" \
  "$REPO_ROOT/data/monitoring/grafana" \
  "$REPO_ROOT/data/monitoring/node-exporter"

# Match container runtime UIDs so first boot works on a fresh host without
# manual permission repair.
run_host_fix chown -R 65534:65534 "$REPO_ROOT/data/monitoring/prometheus" || true
run_host_fix chown -R 472:472 "$REPO_ROOT/data/monitoring/grafana" || true
chmod 0755 "$REPO_ROOT/data/monitoring/node-exporter" || true

bash "$REPO_ROOT/scripts/deploy/whale-pair/refresh_monitoring_metrics.sh"
docker compose -f "$COMPOSE_FILE" up -d

echo ""
echo "Monitoring stack started."
echo "Prometheus: http://127.0.0.1:${WHALE_PAIR_PROMETHEUS_PORT:-9090}"
echo "Grafana:    http://127.0.0.1:${WHALE_PAIR_GRAFANA_PORT:-3000}"
