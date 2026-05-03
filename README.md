# Rust Workspace

This workspace contains the Rust execution scaffold for whale-pair.

## Current shape

- `polymarket-exec` is now a real library + binary crate.
- The binary owns websocket intake, metrics, and a minimal runtime loop.
- The hot path now feeds top-of-book updates into the deterministic execution runtime instead of only logging book summaries.

## Operational runbooks

- Paper/tiny-live runbook: `docs/architecture/2026-04-23-btc-5m-mm-paper-and-tiny-live-runbook.md`
- Infra requirements: `docs/architecture/2026-04-23-btc-5m-mm-infra-requirements.md`
- Paper storage/archive plan: `docs/architecture/2026-04-24-paper-storage-archive-plan.md`
- Hetzner paper deploy helper: `ops/deploy/deploy_paper_hetzner.sh`
- Systemd env template: `polymarket-exec/ops/systemd/common.env.example`

Live mode must use the signed Polymarket CLOB API/SDK path for submit, cancel,
open-order sync, and balance sync before any capital is deployed. Do not treat
paper-mode simulation or unsigned/raw HTTP as tiny-live ready.

## Key env vars

- `PM_BTC_5M_ASSET_IDS`
- `PM_BTC_5M_INSTRUMENT_MARKETS` as `asset_id:market_id[,asset_id:market_id...]`
- `PM_BTC_5M_EXEC_STARTING_CASH_USD`
- `PM_BTC_5M_EXEC_EVENT_LOG_CAPACITY`
- `PM_BTC_5M_EXEC_MAX_ORDER_NOTIONAL_USD`
- `PM_BTC_5M_EXEC_MAX_GROSS_NOTIONAL_USD`
- `PM_BTC_5M_EXEC_MAX_NET_NOTIONAL_PER_MARKET_USD`
- `PM_BTC_5M_EXEC_MAX_POSITION_QTY_PER_INSTRUMENT`
- `PM_BTC_5M_EXEC_MIN_FREE_CASH_USD`
- `PM_BTC_5M_EXEC_MAX_OPEN_ORDERS_TOTAL`
- `PM_BTC_5M_EXEC_MAX_OPEN_ORDERS_PER_MARKET`
- `PM_BTC_5M_PAPER_MODE` (`true|false`, default `true`)
- `PM_BTC_5M_INSTRUMENT_MARKETS` as `asset_id:market_id[,asset_id:market_id...]`
- `PM_BTC_5M_ACCUMULATE_PRICE_MAX`
- `PM_BTC_5M_AGGRESSIVE_PRICE_MAX`
- `PM_BTC_5M_BASE_CLIP_USD`
- `PM_BTC_5M_AGGRESSIVE_CLIP_USD`
- `PM_BTC_5M_MAX_GROSS_COST_USD`
- `PM_BTC_5M_COMPLETION_MIN_PNL_PER_SHARE`
- `PM_BTC_5M_MAX_IMBALANCE_RATIO`
- `PM_BTC_5M_TAKER_FEE_COEFF`

If `PM_BTC_5M_INSTRUMENT_MARKETS` is omitted, the runtime falls back to treating each asset id as its own market id. That is coherent for compile/test purposes but not sufficient for real paired execution.

## Main remaining runtime gaps

- Downstream execution adapter currently supports paper-mode simulation only.
- Primary strategy now defaults to `GoatPairStrategy` (env configured).
- No persistence or replay beyond in-memory event log and inventory state.
- No authenticated execution path from user websocket events into order state transitions.
