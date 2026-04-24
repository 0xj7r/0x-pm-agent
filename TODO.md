# TODO: Execution Roadmap (Rust `polymarket-exec` is now primary)

_Last updated: 2026-04-23_

## What is already in place

- Execution scaffold for paired strategies is implemented in Rust via `execution/rust/polymarket-exec`.
- Runtime/event architecture exists:
  - `execution/rust/polymarket-exec/src/runtime.rs`
  - `execution/rust/polymarket-exec/src/inventory.rs`
  - `execution/rust/polymarket-exec/src/risk.rs`
  - `execution/rust/polymarket-exec/src/event_log.rs`
  - `execution/rust/polymarket-exec/src/journal.rs`
- Market + user websocket drivers are connected (`market_ws.rs`, `user_ws.rs`) and wired into a timed runtime loop in `runner.rs`.
- Strategy modes are implemented:
  - `goat_pair`
  - `unlawful_shear`
  - `noop`
- `unlawful_shear` already includes:
  - dual-side book geometry, cheap/core split logic
  - paired-leg salvage behavior
  - time-in-window progression and late/early phase behavior
  - clip-based incremental re-entry/rebalance
  - anchor-aware end-of-window close attempts when `event_end_time_ms` and `final_price` are available, with close attribution notes.
- Market context file is supported in runtime (`market_context.rs`) and strategy notes include `price_to_beat` and timing context when available.
- Paper dashboard endpoint + JSON state + whale overlay:
  - `GET /dashboard`, `GET /api/state`, `GET /api/whale/events` in `execution/rust/polymarket-exec/src/metrics.rs`.
  - `runner.rs` now refreshes runtime snapshots for dashboard consumers.

## Remaining work before we can call this “aligned/launch-ready” (incremental)

- [x] Add a local command execution bridge for non-paper fallback mode.
  - `runner.rs` now executes strategy commands through a deterministic local adapter and reconciles submit/cancel + fill simulation against market books so runtime state advances even when no live connector is wired.
- [ ] Replace the local execution bridge with a real CLOB adapter in non-paper mode.
  - Keep fallback adapter available for paper-like simulation while connector integration matures.
- [ ] Replace paper-mode-only matching assumptions with explicit execution connector events.
  - `runtime::accept_intent` still has a TODO around downstream acknowledgement lifecycle.
- [x] Replace current paper fill model with depth + queue-aware matching.
  - `runner.rs` now models level-by-level crossing with queue/depth pressure, partial fills, and mixed maker/taker classification.
  - `book.rs` now persists top-of-book depth (up to N levels), and `market_ws.rs` passes full levels through to store.
  - `paper_order_ctx` is now retained across loop ticks for order-age based fill behavior and retry attribution.
- [x] Connect user stream events to runtime state transitions.
  - `user_ws.rs` now emits normalized lifecycle events.
  - `runner.rs` now consumes those events and routes them through `on_order_opened`, `on_order_rejected`, `on_order_cancelled`, and `on_fill`.
- [ ] Add explicit whale-copy feed ingestion/overlay signals for pair mode.
  - Current strategy is book-only and does not consume wallet-event clustering / whale-copy signal overlays.
- [ ] Add explicit BTC spot / timing signal channel into strategy decisioning (if part of unreal-S operational spec).
  - We still run from book-derived triggers in this Rust path.
- [ ] Add staleness/freshness and hard preflight checks.
  - stream connect/auth gating and stale-book alarms are minimal.
- [x] Add explicit merge/redeem lifecycle and stale-leg salvage behavior expected by unreal-S.
  - `types.rs` now has explicit `CloseMethod` values.
  - `user_ws.rs` classifies `OrderMerged` / `OrderRedeemed` events and tags close intent when present.
  - `runner.rs` maps merged/redeemed lifecycle events to sell fills with explicit close method for inventory/equity attribution.
- [x] Add attribution for realized close method (sell vs merge vs redeem) in order lifecycle events.
  - close-method is now attached to fill reports when source event exposes close/redeem/merge intent.
- [x] Add explicit end-window fallback close policy for unresolved/faded windows.
  - `unlawful_shear` now issues deterministic cleanup trims when final price is unavailable but late-phase signals suggest expiry risk.
- [ ] Add canary/rollback controls for live runtime and a unified launch checklist.
  - safer mode switches for pair strategy variants and emergency kill-path.
- [ ] Centralize per-strategy config contract.
  - `config.rs` still reads strategy parameters from many env variables; this has risk of drift.

## Notes

- The Python runtime (`execution/core/engine.py`) remains a useful reference implementation for decision/test behavior, but the primary live path appears to be the Rust crate.
- The largest remaining gap to “robust/unreal-like behavior” is not core logic structure (it exists), but execution plumbing and operational controls around confidence/latency/reconciliation.
