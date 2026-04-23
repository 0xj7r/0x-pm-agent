# Operations workspace layout

This directory stores non-code operational assets used by runtime, deployment,
and monitoring.

- `ops/deploy/`: host- and environment-specific deployment launchers and glue.
- `ops/monitoring/`: dashboards and local monitoring artifacts served by the engine.
- `ops/grafana/` and `ops/prometheus/`: monitoring stack manifests and alert rules.

Keep operational runbooks and playbooks under `docs/deploy/` and reference the
canonical scripts under this directory.
