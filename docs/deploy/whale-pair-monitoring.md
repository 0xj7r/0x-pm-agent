# Whale-Pair Monitoring Stack

This is a self-hosted Prometheus + Grafana stack for the Dublin whale-pair deployment. It is intentionally separate from the repo-root `docker-compose.yml` and does not require changes to `scripts/whale_pair_live_bot.py`.

## What it covers

- Host health via `node-exporter`
- Container CPU / memory / restart telemetry via `cAdvisor`
- Whale-pair-specific health via a host-side Prometheus textfile bridge:
  - service up/down
  - recent log activity
  - recent `book_age_ms` samples
  - ledger presence and ledger freshness

The stack is designed to monitor both:

- `whale-pair-live`
- `whale-pair-shadow`

If one of those services is not deployed, its `service_up` metric simply stays `0`.

## Files

- Compose: [scripts/deploy/whale-pair/docker-compose.monitoring.whale-pair.yml](/Users/jackreid/go/polymarket-agent/scripts/deploy/whale-pair/docker-compose.monitoring.whale-pair.yml)
- Prometheus config: [ops/prometheus/prometheus.whale-pair.yml](/Users/jackreid/go/polymarket-agent/ops/prometheus/prometheus.whale-pair.yml)
- Alert rules: [ops/prometheus/rules/whale-pair-alerts.yml](/Users/jackreid/go/polymarket-agent/ops/prometheus/rules/whale-pair-alerts.yml)
- Grafana provisioning:
  - [ops/grafana/provisioning/datasources/prometheus.yml](/Users/jackreid/go/polymarket-agent/ops/grafana/provisioning/datasources/prometheus.yml)
  - [ops/grafana/provisioning/dashboards/dashboards.yml](/Users/jackreid/go/polymarket-agent/ops/grafana/provisioning/dashboards/dashboards.yml)
  - [ops/grafana/dashboards/whale-pair-overview.json](/Users/jackreid/go/polymarket-agent/ops/grafana/dashboards/whale-pair-overview.json)
- Host-side scripts:
  - [scripts/deploy/whale-pair/start_monitoring.sh](/Users/jackreid/go/polymarket-agent/scripts/deploy/whale-pair/start_monitoring.sh)
  - [scripts/deploy/whale-pair/stop_monitoring.sh](/Users/jackreid/go/polymarket-agent/scripts/deploy/whale-pair/stop_monitoring.sh)
  - [scripts/deploy/whale-pair/refresh_monitoring_metrics.sh](/Users/jackreid/go/polymarket-agent/scripts/deploy/whale-pair/refresh_monitoring_metrics.sh)
  - [scripts/deploy/whale-pair/install_monitoring_cron.sh](/Users/jackreid/go/polymarket-agent/scripts/deploy/whale-pair/install_monitoring_cron.sh)
  - [scripts/deploy/whale-pair/monitoring_health.sh](/Users/jackreid/go/polymarket-agent/scripts/deploy/whale-pair/monitoring_health.sh)

## Operator steps

1. On the Dublin host, from `/opt/polymarket-agent`, start the stack:

```bash
bash scripts/deploy/whale-pair/start_monitoring.sh
```

2. Install the host-side metric refresh cron:

```bash
bash scripts/deploy/whale-pair/install_monitoring_cron.sh
```

3. Verify stack health:

```bash
bash scripts/deploy/whale-pair/monitoring_health.sh
```

4. Open Grafana locally or through an SSH tunnel:

```bash
ssh -L 3000:127.0.0.1:3000 <host>
```

Then browse to `http://127.0.0.1:3000`.

Default credentials are controlled by env vars in the monitoring compose file:

- user: `admin`
- password: `admin`

Override them before first unattended use.

## Ports

By default the monitoring services bind only to loopback:

- Grafana: `127.0.0.1:3000`
- Prometheus: `127.0.0.1:9090`

Use SSH tunneling or an external reverse proxy if remote access is needed.

## What the refresh script exports

Per service label:

- `whale_pair_service_up`
- `whale_pair_service_restart_count`
- `whale_pair_service_recent_log_lines`
- `whale_pair_service_has_book_telemetry`
- `whale_pair_service_book_age_avg_ms`
- `whale_pair_service_book_age_max_ms`
- `whale_pair_service_book_age_samples`
- `whale_pair_service_ledger_present`
- `whale_pair_service_ledger_age_seconds`
- `whale_pair_service_metrics_collection_success`

Global:

- `whale_pair_monitoring_last_run_timestamp_seconds`

## Alert examples

Prometheus rules in this stack include:

- whale-pair service down
- no recent whale-pair logs
- stale `book_age_ms`
- stale ledger file
- missing monitoring targets (`node-exporter`, `cadvisor`)

These rules are examples and are safe to start with. Pager routing is still an operator integration step.

## Notes

- This stack does not edit or instrument the whale-pair runtime.
- The Prometheus bridge script reads Docker state and recent container logs on the host.
- If only `whale-pair-live` exists, the dashboard still works; select that service in the Grafana variable.
