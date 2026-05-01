# Whale Pair Rust Execution Skeleton

This crate is a standalone execution-side scaffold for the whale-pair strategy. It is deliberately narrow:

- subscribe to Polymarket market WebSocket for configured outcome assets
- optionally subscribe to the authenticated user WebSocket
- maintain an in-memory top-of-book cache
- export Prometheus metrics and structured logs
- leave order placement, ledger writes, and merge/redeem execution for the next interface layer

## Workspace

- `rust/Cargo.toml`: local Rust workspace root
- `rust/polymarket-exec/Cargo.toml`: binary crate definition

## Module map

- `src/config.rs`: env-backed service configuration
- `src/logging.rs`: `tracing` setup with pretty or JSON output
- `src/metrics.rs`: Prometheus registry plus `/metrics` and `/healthz`
- `src/book.rs`: shared top-of-book cache used by the runtime loop
- `src/market_ws.rs`: public Polymarket CLOB market channel
- `src/user_ws.rs`: authenticated Polymarket user channel
- `src/main.rs`: runtime wiring, periodic freshness loop, graceful shutdown

## Expected env

- `PM_BTC_5M_ASSET_IDS`: comma-separated Polymarket asset IDs to subscribe
- `PM_BTC_5M_USER_MARKETS`: optional comma-separated market/condition IDs for the user stream
- `POLYMARKET_API_KEY`
- `POLYMARKET_API_SECRET`
- `POLYMARKET_API_PASSPHRASE`
- `POLYMARKET_MARKET_WS_URL` / `POLYMARKET_USER_WS_URL` if overrides are needed
- `PM_BTC_5M_EXEC_METRICS_BIND`, `PM_BTC_5M_EXEC_BOOK_STALE_MS`, `PM_BTC_5M_EXEC_LOOP_INTERVAL_MS`

## Intended next interfaces

1. Order-entry adapter:
   a typed request/response boundary for create/cancel/replace against the CLOB
2. User-stream order tracker:
   local order state keyed by order ID, not just raw event logging
3. Market discovery handoff:
   a scanner or control plane that rotates the subscribed asset IDs as the active BTC window changes
4. Pair ledger sink:
   persistence for fills, matched pairs, and merge eligibility
5. Merge/redeem executor:
   Rust boundary around the existing CTF merge/redeem lifecycle
