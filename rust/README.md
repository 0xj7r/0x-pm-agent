# Rust Workspace

This workspace contains the Rust execution scaffold for whale-pair.

## Current shape

- `whale-pair-exec` is now a real library + binary crate.
- The binary owns websocket intake, metrics, and a minimal runtime loop.
- The hot path now feeds top-of-book updates into the deterministic execution runtime instead of only logging book summaries.

## Key env vars

- `WHALE_PAIR_ASSET_IDS`
- `WHALE_PAIR_INSTRUMENT_MARKETS` as `asset_id:market_id[,asset_id:market_id...]`
- `WHALE_PAIR_EXEC_STARTING_CASH_USD`
- `WHALE_PAIR_EXEC_EVENT_LOG_CAPACITY`
- `WHALE_PAIR_EXEC_MAX_ORDER_NOTIONAL_USD`
- `WHALE_PAIR_EXEC_MAX_GROSS_NOTIONAL_USD`
- `WHALE_PAIR_EXEC_MAX_NET_NOTIONAL_PER_MARKET_USD`
- `WHALE_PAIR_EXEC_MAX_POSITION_QTY_PER_INSTRUMENT`
- `WHALE_PAIR_EXEC_MIN_FREE_CASH_USD`
- `WHALE_PAIR_EXEC_MAX_OPEN_ORDERS_TOTAL`
- `WHALE_PAIR_EXEC_MAX_OPEN_ORDERS_PER_MARKET`

If `WHALE_PAIR_INSTRUMENT_MARKETS` is omitted, the runtime falls back to treating each asset id as its own market id. That is coherent for compile/test purposes but not sufficient for real paired execution.

## Main remaining runtime gaps

- No downstream order submit/cancel adapter is wired to `RuntimeCommand`.
- No real strategy is plugged into the runtime; `NoopStrategy` keeps the hot path deterministic.
- No persistence or replay beyond in-memory event log and inventory state.
- No authenticated execution path from user websocket events into order state transitions.
