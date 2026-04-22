# Whale-Pair Monitoring Stack

This is a self-hosted Prometheus + Grafana + Alertmanager stack for the Dublin whale-pair deployment. It is intentionally separate from the repo-root `docker-compose.yml` and does not require changes to `scripts/whale_pair_live_bot.py`.

## What it covers

- Host health via `node-exporter`
- Container CPU / memory / restart telemetry via `cAdvisor`
- Whale-pair-specific health via a host-side Prometheus textfile bridge:
  - service up/down
  - recent log activity
  - recent `book_age_ms` samples
  - ledger presence and ledger freshness
- Alert delivery via Alertmanager with env-driven outbound transports:
  - Discord webhook
  - Telegram bot delivery
  - optional generic webhook receiver for a second hop or incident router

The stack is designed to monitor both:

- `whale-pair-live`
- `whale-pair-shadow`

If one of those services is not deployed, its `service_up` metric simply stays `0`.

## Files

- Compose: [scripts/deploy/whale-pair/docker-compose.monitoring.whale-pair.yml](/Users/jackreid/go/polymarket-agent/scripts/deploy/whale-pair/docker-compose.monitoring.whale-pair.yml)
- Prometheus config: [ops/prometheus/prometheus.whale-pair.yml](/Users/jackreid/go/polymarket-agent/ops/prometheus/prometheus.whale-pair.yml)
- Alert rules: [ops/prometheus/rules/whale-pair-alerts.yml](/Users/jackreid/go/polymarket-agent/ops/prometheus/rules/whale-pair-alerts.yml)
- Alertmanager render helper: [scripts/deploy/whale-pair/render_alertmanager_config.sh](/Users/jackreid/go/polymarket-agent/scripts/deploy/whale-pair/render_alertmanager_config.sh)
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
  - [scripts/deploy/whale-pair/send_test_alert.sh](/Users/jackreid/go/polymarket-agent/scripts/deploy/whale-pair/send_test_alert.sh)

## Required env vars for alert transport

At least one outbound transport is needed if you want alerts to leave the host:

- `WHALE_PAIR_DISCORD_WEBHOOK_URL`
- `WHALE_PAIR_TELEGRAM_BOT_TOKEN`
- `WHALE_PAIR_TELEGRAM_CHAT_ID`
- `WHALE_PAIR_ALERT_WEBHOOK_URL`

Optional alert transport tuning:

- `WHALE_PAIR_ALERT_WEBHOOK_BEARER_TOKEN`
- `WHALE_PAIR_ALERT_SEND_RESOLVED` default `true`
- `WHALE_PAIR_ALERT_GROUP_BY` default `alertname,service,severity`
- `WHALE_PAIR_ALERT_GROUP_WAIT` default `30s`
- `WHALE_PAIR_ALERT_GROUP_INTERVAL` default `5m`
- `WHALE_PAIR_ALERT_REPEAT_INTERVAL` default `4h`
- `WHALE_PAIR_ALERT_RESOLVE_TIMEOUT` default `5m`

If none of the outbound transport vars are set, Alertmanager still runs, but all alerts are routed to a local blackhole receiver. That is useful for dry runs, not for unattended live trading.

## Operator steps

1. On the Dublin host, set the transport env vars in the same environment used to launch monitoring, for example:

```bash
export WHALE_PAIR_DISCORD_WEBHOOK_URL="https://discord.com/api/webhooks/..."
export WHALE_PAIR_TELEGRAM_BOT_TOKEN="123456:abc..."
export WHALE_PAIR_TELEGRAM_CHAT_ID="-1001234567890"
```

2. From `/opt/polymarket-agent`, start the stack:

```bash
bash scripts/deploy/whale-pair/start_monitoring.sh
```

This renders `data/monitoring/alertmanager/alertmanager.yml` from env before the containers start.

3. Install the host-side metric refresh cron:

```bash
bash scripts/deploy/whale-pair/install_monitoring_cron.sh
```

4. Verify stack health:

```bash
bash scripts/deploy/whale-pair/monitoring_health.sh
```

5. Send a synthetic alert through Alertmanager to validate transport end to end:

```bash
bash scripts/deploy/whale-pair/send_test_alert.sh --severity critical --service whale-pair-live
```

6. Open Grafana or Alertmanager locally or through an SSH tunnel:

```bash
ssh -L 3000:127.0.0.1:3000 -L 9090:127.0.0.1:9090 -L 9093:127.0.0.1:9093 <host>
```

Then browse to `http://127.0.0.1:3000`.
Alertmanager is available at `http://127.0.0.1:9093`.

Default credentials are controlled by env vars in the monitoring compose file:

- user: `admin`
- password: `admin`

Override them before first unattended use.

## Ports

By default the monitoring services bind only to loopback:

- Alertmanager: `127.0.0.1:9093`
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

These rules are examples and are safe to start with. Alertmanager is now wired to deliver them when one or more transport env vars are configured.

## Changing transport config

Alertmanager config is rendered from env, not edited in place.

1. Update the env vars.
2. Re-render the config:

```bash
bash scripts/deploy/whale-pair/render_alertmanager_config.sh
```

`start_monitoring.sh` handles the ownership fix automatically on a fresh boot. If you render manually on a running host, keep the rendered file readable by the Alertmanager container before restart.

3. Restart or reload Alertmanager:

```bash
docker compose -f scripts/deploy/whale-pair/docker-compose.monitoring.whale-pair.yml restart alertmanager
```

## Notes

- This stack does not edit or instrument the whale-pair runtime.
- The Prometheus bridge script reads Docker state and recent container logs on the host.
- If only `whale-pair-live` exists, the dashboard still works; select that service in the Grafana variable.
- Alertmanager supports both native Discord webhook delivery and native Telegram delivery. The optional generic webhook receiver is there for teams that want a second hop into another incident system.
