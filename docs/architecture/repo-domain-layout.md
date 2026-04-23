# Domain-oriented repository layout

This project is now grouped by domain: **execution** and **research**, with
compatibility links at legacy paths.

## Execution (live / paper / Rust implementation)

- Canonical:
  - `execution/clients`
  - `execution/core`
  - `execution/models`
  - `execution/config.py`
  - `execution/shared`
  - `execution/strategies`
  - `execution/rust`
  - `execution/bots`
  - `execution/whale_pair`
  - `execution/main.py`
  - `execution/strategy_config.json`
- Compatibility:
  - `clients`, `core`, `models`, `config.py`, `shared`, `strategies`,
    `rust`, `main.py`, `strategy_config.json`
    are symlinks to `execution/*`
  - `scripts/bots` and `scripts/whale_pair` remain compatibility wrappers
    and point into `execution/`

## Research & modeling

- Canonical:
  - `research/backtesting`
  - `research/collector`
  - `research/analysis`
  - `research/dataops`
  - `research/wallet_profiles`
  - `research/weather`
- Compatibility:
  - `backtesting`, `collector`, `scripts/analysis`, `scripts/dataops`
    are symlinks to `research/*`

## Operations

- `ops/` holds monitoring, infra scripts, and deploy manifests.
- `scripts/deploy/whale-pair` is a compatibility symlink to
  `ops/deploy/whale-pair`.
- `deploy.sh` is the compatibility entry for Hetzner with canonical source at
  `ops/deploy/deploy_hetzner.sh`.
- Runtime dashboards are canonical at `ops/monitoring/dashboard.html` with
  `dashboard.html` compatibility entry.

This split is intentional: keep execution-path changes isolated from analysis and
historical-modeling work so research and production execution can evolve
independently.


### Data organization

- Canonical research data:
  - `data/research/whale_analysis`
  - `data/research/wallet_research`
  - `data/research/archive`
  - `data/research/snapshots_bridge`
- Compatibility paths remain at:
  - `data/whale_analysis`
  - `data/wallet_research`
  - `data/archive`
  - `data/snapshots_bridge`
