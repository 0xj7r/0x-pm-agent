# Current Runtime and V2 Baseline

Status: repo-grounded baseline as of 2026-04-22

## What exists today

This repo is still a Python-first trading system with two live entrypoints:

1. `main.py`
   - starts `core.engine.BTCTradingEngine`
   - used for the directional runtime
   - mixes market discovery, signal generation, live order placement, reconciliation, health HTTP, and some operator controls in one process

2. `scripts/whale_pair_live_bot.py`
   - separate runner for paired-leg execution
   - still uses the same Python `Config`, `PolymarketClient`, `MarketWindowScanner`, and on-chain helpers
   - persists to a dedicated SQLite ledger in `core/whale_pair_ledger.py`

There is no Rust execution plane in the repo today:

- no `Cargo.toml`
- no `.rs` sources
- the root `Dockerfile` builds only a Python 3.13 image

## Current runtime layout

### Venue/data access

- `clients/polymarket.py`
  - wraps `py-clob-client`
  - owns authenticated CLOB access
  - also uses Gamma over `httpx` for market discovery
- `clients/polymarket_ws.py`
  - market-channel websocket for order book updates
- `clients/polymarket_user_ws.py`
  - user-channel websocket for fills/order events
- `clients/market_scanner.py`
  - discovers active windows from Gamma `/events`
  - derives `price_to_beat` and current/next window metadata

### Strategy/execution

- `core/engine.py`
  - directional engine
  - handles Binance feed, Polymarket book feed, signal checks, live placement, polling, reconciliation, and redemption
- `scripts/whale_pair_live_bot.py`
  - paired-leg loop
  - uses market WS first, falls back to HTTP `/book`
  - confirms fills by polling `get_order_status`
  - merges matched inventory through `clients/ctf_merger.py`

### Persistence/state

- local SQLite is the runtime source of truth today
  - directional path: `core/memory.py` / `event_log`
  - whale-pair path: `core/whale_pair_ledger.py`
- Supabase is optional in parts of the live stack, not mandatory across the system
- there is no shared remote execution-state store for leader election, recovery checkpoints, or fleet coordination

### Monitoring/ops

- `core/health.py` exposes an HTTP health/dashboard for the directional engine
- whale-pair deploy scripts rely on log-grep and file checks:
  - `scripts/deploy/whale-pair/health.sh`
  - `scripts/deploy/whale-pair/start.sh`
  - `scripts/deploy/whale-pair/kill.sh`
- there is no first-class metrics pipeline, alert router, run registry, or standby promotion controller

## Current Polymarket integration facts

### Hard dependency on V1 client surface

The repo currently assumes `py-clob-client`:

- `requirements.txt` installs `py-clob-client>=0.0.1`
- `clients/polymarket.py` imports from `py_clob_client.client` and `py_clob_client.clob_types`
- tests in `tests/test_polymarket_client.py` mock the V1 package directly

### Order-placement assumptions baked into repo code

`clients/polymarket.py` currently:

- derives or creates API creds on startup
- calls `get_fee_rate_bps(token_id)`
- builds `OrderArgs(... fee_rate_bps=...)`
- calls `create_order(...)`
- calls `post_order(..., OrderType.GTC)`

That is a V1-shaped client contract, not an abstract venue adapter.

### Collateral assumptions baked into repo code

The repo is still collateralized around USDC.e:

- `config.py` hardcodes `COLLATERAL_TOKEN_ADDRESS` to Polygon USDC.e
- `scripts/pnl_report.py` and multiple docs/reporting scripts treat USDC.e as live cash
- `clients/ctf_redeemer.py` and `clients/ctf_merger.py` pass the configured collateral token into on-chain calls

### Recovery model today

Recovery is local-process oriented:

- PID/lock files in `main.py`
- replay from SQLite logs/ledgers
- ad hoc startup reconciliation inside `core.engine.py`
- no authoritative remote control plane for:
  - active leader
  - last durable venue checkpoint
  - order-intent journal
  - standby promotion safety

## What CLOB V2 changes for this repo

Per Polymarket's official migration docs and changelog:

- V2 requires `py-clob-client-v2`
- order fields remove `nonce`, `feeRateBps`, `taker`
- order fields add `timestamp`, `metadata`, `builder`
- fee handling moves to venue/operator-time logic instead of signed-order fields
- collateral moves from USDC.e to pUSD
- V2 is testable now on `https://clob-v2.polymarket.com`
- production cutover is scheduled for April 28, 2026 at about 11:00 UTC
- open orders are wiped during cutover

That means the current repo is not just "missing an upgrade"; it is structurally coupled to V1 at the client, collateral, and recovery layers.

## Baseline gap summary

### Already usable after targeted migration

- Gamma-driven market discovery
- websocket-first market-book loop
- user websocket integration pattern
- SQLite-backed local journal/ledger model
- pair ledger domain logic
- deploy shell scripts as a starting point

### Not venue-native enough yet

- `PolymarketClient` is a thin wrapper over a specific V1 Python SDK
- execution logic and strategy logic are co-located in Python
- no execution-plane boundary
- no remote control plane for standby/recovery
- no metrics-first observability
- no exact pUSD wrapping/offramp workflow in repo

### Must be treated as blockers before funded V2 launch

1. Replace V1 SDK/package usage end to end.
2. Remove signed-order dependence on `fee_rate_bps`.
3. Rework collateral and balance accounting from USDC.e to pUSD-aware flows.
4. Revalidate merge/redeem/on-chain token operations against V2-era contracts and settlement flow.
5. Add startup reconciliation that is safe after order-book wipe and process crash.
6. Separate hot-path execution from Python research/runtime glue.

