# TODO: Execution Roadmap (Rust `polymarket-exec` is now primary)

_Last updated: 2026-04-28_

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
- [ ] Before scaling beyond tiny-live, move live auth/funding to the intended Polymarket profile wallet.
  - Tiny-live may temporarily use the funded MetaMask EOA path while we validate execution behavior.
  - Before increasing capital, set the correct `POLYMARKET_SIGNATURE_TYPE` and `POLYMARKET_FUNDER_ADDRESS` for the Polymarket Safe/proxy wallet, then rerun live smoke and balance/open-order reconciliation.
  - Do not treat EOA smoke success as proof that profile-wallet/Safe execution is configured correctly.
- [ ] Add settlement/redeem operations for resolved market positions.
  - Current live safety now tracks venue cash, but unresolved/redeemable positions can still tie up capital.
  - Add an explicit post-settlement redeem worker or operator command before scaling capital.
- [ ] Finish venue-contract E2E tests without live funds.
  - Add mock CLOB + relayer HTTP fixtures for order submit/cancel, open-order sync, Data API positions, relayer nonce, relayer submit, and transaction polling.
  - Replay one full paired market lifecycle: maker bid submit -> one leg fill -> hedge/pair fill -> merge request -> venue position disappears -> cash recovers.
  - Treat this as CI coverage; live tiny-capital smoke remains the final venue validation, not the only test layer.
- [ ] Add maker-rebate attribution for live fills.
  - Distinguish submitted-as-post-only from actually-rested-and-filled-as-maker.
  - Persist per-order maker/taker outcome, rest time before fill, and rebated-fee eligibility so calibration can answer whether our buys are being left open for takers.
- [ ] Validate relayer wallet mapping before next live run.
  - `RELAYER_API_KEY` is present locally but `RELAYER_API_KEY_ADDRESS` is currently missing.
  - The relayer key owner can be the core/builder account, but the transaction signer still needs to be the bot private key and the proxy wallet must be the inventory-holding Polymarket wallet.
  - Confirm `POLYMARKET_FUNDER_ADDRESS` or `POLYMARKET_PROXY_WALLET_ADDRESS` is the wallet that owns the outcome tokens before enabling live merge.

## Followups deferred to a later session

- [ ] **AWS migration for always-on operation.** Local-Mac shadow_live works for hours-long sessions but isn't sustainable for 24/7 calibration or live capital scaling. AWS-EC2 deployment scaffold exists at `polymarket-exec/ops/deploy/deploy_live_aws_ec2.sh`; S3 backup wired via `polymarket-exec/scripts/backup_to_s3.sh`. When ready: provision cloud-init/Terraform module, ship metrics to CloudWatch, move secrets to Secrets Manager, add healthcheck-driven kill-switch via SSM. Trigger to flip: scaling capital past tinylive OR wanting continuous shadow_live calibration. ~1-2 days of ops work.
- [ ] **Single-line per-order lifecycle log.** Today the maker-fill submission lifecycle is split across `strategy.sizing` + `polymarket_exec::runtime` + `polymarket_exec::wire::execution_adapter` log targets. Operators have to grep `client_order_id` across them to reconstruct intent → submit → ack → fill/reject. A single `info!()` per terminal lifecycle event with the full trail (intent params + venue ack + fill outcome) would make ops debugging much faster. ~10-20 lines of code in `runtime/mod.rs`.
- [ ] **Auto-wire strategy sizing to venue metadata cache (Q6 full closure).** `fetch_market_metadata` exists and logs venue truth on startup; strategy still uses the hardcoded `entry_min_size_multiplier`. Wiring the strategy to read from a cached `MarketMetadata` (TTL ~5min) closes the audit gap fully.
- [ ] **Persist `markets_with_unresolved_drift` across restart.** Currently in-memory only; warn-logged on `recover_from_store`. Persist to order_store schema so drift block survives crashes.
- [ ] **Calibrate paper-env defaults against real Polymarket fill data.** Latency 150ms / queue depth 0.75 / post-only reject 0.85 / cancel race 500ms are all from MM literature, not measured. Run multi-hour shadow_live + use `compare_replay.py` + `suggest_paper_calibration.py` to find values that match observed fill rates within tolerance.

## Notes

- The Python runtime (`execution/core/engine.py`) remains a useful reference implementation for decision/test behavior, but the primary live path appears to be the Rust crate.
- The largest remaining gap to “robust/unreal-like behavior” is not core logic structure (it exists), but execution plumbing and operational controls around confidence/latency/reconciliation.
