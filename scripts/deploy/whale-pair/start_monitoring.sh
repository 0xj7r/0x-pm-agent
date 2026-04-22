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

clean_appledouble() {
  find "$1" -type f -name '._*' -delete 2>/dev/null || true
}

mkdir -p \
  "$REPO_ROOT/data/monitoring/alertmanager/data" \
  "$REPO_ROOT/data/monitoring/prometheus" \
  "$REPO_ROOT/data/monitoring/grafana" \
  "$REPO_ROOT/data/monitoring/node-exporter"

# macOS tar/scp flows can leave AppleDouble sidecar files behind, which break
# Prometheus/Grafana YAML loaders on Linux hosts.
clean_appledouble "$REPO_ROOT/ops/prometheus"
clean_appledouble "$REPO_ROOT/ops/grafana"

# Ensure the host user can rewrite the rendered config even if a previous run
# handed the directory to the container UID.
run_host_fix chown "$(id -u):$(id -g)" "$REPO_ROOT/data/monitoring/alertmanager" || true

bash "$REPO_ROOT/scripts/deploy/whale-pair/render_alertmanager_config.sh"

# Match container runtime UIDs so first boot works on a fresh host without
# manual permission repair. Keep the alertmanager root dir writable by the host
# user so config rendering can happen before compose starts; only the state dir
# and rendered config need container ownership.
run_host_fix chown -R 65534:65534 "$REPO_ROOT/data/monitoring/alertmanager/data" || true
run_host_fix chown 65534:65534 "$REPO_ROOT/data/monitoring/alertmanager/alertmanager.yml" || true
run_host_fix chown -R 65534:65534 "$REPO_ROOT/data/monitoring/prometheus" || true
run_host_fix chown -R 472:472 "$REPO_ROOT/data/monitoring/grafana" || true
chmod 0755 "$REPO_ROOT/data/monitoring/node-exporter" || true

bash "$REPO_ROOT/scripts/deploy/whale-pair/refresh_monitoring_metrics.sh"
docker compose -f "$COMPOSE_FILE" up -d

echo ""
echo "Monitoring stack started."
echo "Alertmanager: http://127.0.0.1:${WHALE_PAIR_ALERTMANAGER_PORT:-9093}"
echo "Prometheus: http://127.0.0.1:${WHALE_PAIR_PROMETHEUS_PORT:-9090}"
echo "Grafana:    http://127.0.0.1:${WHALE_PAIR_GRAFANA_PORT:-3000}"
