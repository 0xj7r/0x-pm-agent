# Domain-oriented repository layout

This project has two primary execution streams:

- **Execution / live trading**
  - `core/`, `strategies/`, `clients/`, `models/`, `shared/`
  - `scripts/whale_pair/` and `scripts/bots/` orchestration entrypoints
  - `rust/` for Rust execution implementations (paper/live parity)
  - `tests/` covers runtime and bot behavior

- **Research / analysis**
  - `backtesting/` strategy/market simulations
  - `scripts/analysis/` and `scripts/dataops/` data extraction and transform jobs
  - `research/` experimental studies and notebooks-like scripts
  - `data/` research exports (e.g., `wallet_research/`, `whale_analysis/`)

- **Operations**
  - `ops/` monitoring, deploy manifests, and local infra helpers
  - `deploy.sh` remains a compatibility entry for Hetzner; canonical source is
    `ops/deploy/deploy_hetzner.sh`
  - Runtime dashboards now live in `ops/monitoring/dashboard.html` with a
    compatibility root symlink

This split is intentional: keep execution-path changes isolated from analysis
pipelines so strategy experiments and trade validation can evolve independently.
