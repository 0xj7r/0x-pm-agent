# BTC 5m MM Infra Requirements

Status: implementation handoff  
Date: 2026-04-23  
Primary target: `polymarket-exec`  
Secondary target: control-plane helpers under `scripts/`

## 1. Purpose

This document defines the infrastructure required to:

- run the BTC 5m paper sleeve continuously
- collect enough live market context to calibrate the engine
- move quickly into tiny-live once the paper runtime is stable

This is the minimum complete stack.

## 2. Deployment Model

Separate the system into three planes.

### Trading plane

Owns:

- Polymarket market websocket
- Polymarket user websocket
- order submission / cancel / replace
- live strategy runtime
- pair ledger
- merge / redeem worker

This is latency-sensitive and restart-sensitive.

### Control plane

Owns:

- active BTC 5m market discovery
- runtime context export
- session scheduling
- feature flag / config promotion
- collector scheduling

This must not sit on the hot path.

### Research / observability plane

Owns:

- live collector outputs
- journals
- metrics
- strategy replay inputs
- PnL and rebate reconciliation

This can lag slightly. It must not block trading.

## 3. Region Guidance

### Paper mode

Paper can run:

- local
- current US host
- any stable host with good websocket reliability

Latency matters less here than clean data capture and stable runtime behavior.

### Tiny-live / live

For the international Polymarket venue, the live trading plane should move to:

- `eu-west-1` as the default target

Reason:

- it is a better operational fit than the current US setup
- it is the cleanest AWS region for a Europe-based trading plane
- it avoids building the live path around a region we already expect to change

The control plane and research plane do not need to move first.

## 4. Required External APIs

### Polymarket Gamma API

Use for:

- exact market lookup by slug
- active market metadata
- token IDs
- event start / end time
- fee metadata when available

Current practical use:

- `scripts/export_btc_5m_runtime.py` should query exact BTC 5m slugs for previous/current/next windows

### Polymarket market websocket

Use for:

- live order-book updates
- best bid / ask
- price changes
- trade prints

This is the primary market-data source for the runtime.

### Polymarket user websocket

Use for:

- our order acknowledgements
- fills
- cancels
- rejects
- open-order reconciliation

This is mandatory before tiny-live.

### Polymarket CLOB trading API / SDK

Use for:

- submit
- cancel
- sync open orders
- sync balances

The runtime should use the SDK / venue-native signing path rather than hand-rolled raw requests.

### Rebate endpoint

Use for:

- maker rebate verification
- daily economics reconciliation

This is not hot-path data, but it is essential for validating the MM sleeve.

### Polygon RPC + Etherscan/Polygonscan

Use for:

- approvals
- merge / redeem visibility
- funding / treasury reconciliation
- fill / settlement debugging

RPC is runtime-critical for tiny-live. Explorer APIs are not.

### External BTC spot feed

Use for:

- rolling realized vol
- trade count
- short-horizon returns
- shock detection

The runtime should treat BTC spot as a regime and risk feed, not as the primary directional alpha source.

## 5. Runtime Services

## 5.1 BTC 5m runtime

Required responsibilities:

- subscribe to both outcome books
- maintain local book state
- compute paired-market geometry
- enforce regime gate
- emit order intents
- manage fills, inventory, pair completion, and cleanup

## 5.2 Market context exporter

Required responsibilities:

- export previous/current/next BTC 5m windows
- populate:
  - `PM_BTC_5M_ASSET_IDS`
  - `PM_BTC_5M_INSTRUMENT_MARKETS`
  - `PM_BTC_5M_USER_MARKETS`
- write:
  - `rust_runtime.env`
  - `rust_market_context.json`

This now needs to be a rolling-window exporter, not a one-market exporter.

Current operator entrypoints:

- `scripts/export_btc_5m_runtime.py`
- `polymarket-exec/scripts/run_sleeve.sh`
- `polymarket-exec/scripts/run_unlawful_shear_paper.sh`

## 5.3 Merge / redeem worker

Required responsibilities:

- track merge-eligible pairs
- submit merge / redeem actions
- write outcomes into journal / accounting state

Paper mode can stub this. Tiny-live cannot.

## 5.4 Live collector

Required responsibilities:

- record unlawful-like market activity live
- capture books, trades, and market metadata around active windows
- produce machine-readable research packs for re-calibration

This should run continuously once the paper sleeve is active.

## 6. State and Storage

Minimum storage surfaces:

- durable order journal
- durable event journal
- runtime order store
- strategy snapshots
- collector outputs

Recommended v1 storage split:

- SQLite is acceptable for paper and local control-plane artifacts
- Postgres is preferred once tiny-live starts

Required persisted entities:

- market context snapshots
- order intents
- submit / cancel acks
- fills
- merge / redeem events
- balances
- runtime checkpoints

## 7. Secrets and Wallets

Required secret classes:

- Polymarket private key / signing key
- Polymarket signature type and optional funder address
- API credentials derived from wallet for user websocket and L2 CLOB auth
- Polygon RPC credentials
- optional market-data credentials

Operational rules:

- paper and live wallets must be separate
- direct MetaMask EOA is acceptable for smoke/tiny-live only when the funded
  signer address itself has CLOB balance/allowance
- live should use a Gnosis Safe / proxy wallet before scaling beyond smoke
- research scripts must not share the live wallet key
- approvals and balances must be audited before tiny-live

Current live-auth and ops env surface:

- `POLYMARKET_API_KEY`
- `POLYMARKET_API_SECRET`
- `POLYMARKET_API_PASSPHRASE`
- `POLYMARKET_PRIVATE_KEY`
- `METAMASK_PRIVATE_KEY` as a local fallback alias only
- `POLYMARKET_SIGNATURE_TYPE=eoa` with no `POLYMARKET_FUNDER_ADDRESS` for a
  directly funded MetaMask EOA
- `POLYMARKET_SIGNATURE_TYPE=gnosis_safe` plus `POLYMARKET_FUNDER_ADDRESS` only
  when the funded account is a Safe/proxy wallet
- `PM_BTC_5M_EXEC_SPOT_WS_URL`
- `PM_BTC_5M_EXEC_SPOT_SYMBOL`
- `PM_BTC_5M_DASHBOARD_WHALE_EVENTS_PATH`
- `PM_BTC_5M_DASHBOARD_REFRESH_MS`
- `PM_BTC_5M_DASHBOARD_EVENT_LIMIT`

## 9. Current checked-in operator surfaces

These now exist in-repo:

- generic sleeve launcher:
  - `polymarket-exec/scripts/run_sleeve.sh`
- base runtime bootstrap:
  - `polymarket-exec/scripts/run_unlawful_shear_paper.sh`
- sleeve env presets:
  - `polymarket-exec/env/*.env`
- systemd template:
  - `polymarket-exec/ops/systemd/polymarket-exec@.service`
- runbook:
  - `docs/architecture/2026-04-23-btc-5m-mm-paper-and-tiny-live-runbook.md`

## 8. Metrics and Alerting

Minimum runtime metrics:

- book staleness
- market websocket disconnect count
- user websocket disconnect count
- order submit latency
- cancel latency
- reject count by reason
- maker fill share
- pair completion rate
- merge latency
- stranded inventory
- realized net PnL
- rebate per day

Minimum alerts:

- stale market data
- stale user data
- reconciliation mismatch
- inventory limit breach
- missing merge progress
- repeated order reject loop

## 9. Kill Switches

The runtime must fail closed on:

- market websocket stale
- user websocket stale
- missing market context
- invalid token mapping
- repeated submit rejects
- inventory above hard cap
- cleanup failure beyond configured threshold

## 10. Paper-Only Requirements

Paper mode must still use:

- live market websocket
- live BTC spot feed
- rolling market context
- full strategy gate

Paper mode may stub:

- live order submission
- live merge execution
- live balances

But it must still record:

- intended submits
- intended cancels
- intended cleanup actions
- simulated fills and pair state

## 11. Tiny-Live Additions

Before tiny-live, add:

- user websocket auth path
- live execution adapter
- wallet approvals
- merge / redeem worker
- live reconciliation on startup
- daily rebate check

## 12. Recommended Immediate Follow-Ups

1. Keep the paper runtime on a rolling previous/current/next market slate.
2. Treat session hours as a soft preference, not a hard closure.
3. Add the unlawful live collector now, not after tiny-live.
4. Keep research/control-plane separate from the Rust hot path.
5. Move the live trading plane to `eu-west-1` before non-trivial real-money usage.
