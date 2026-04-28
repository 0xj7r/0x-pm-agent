# Live Execution Priority Checklist

Working checklist for the live `polymarket-exec` engine. Check items only when the repo has the implementation and focused validation for that item. Operational rollout notes belong in the item notes until the deployed service is confirmed healthy.

## P0 - Live Trading Blockers

- [x] 1. Fix IOC/FOK/GTC V2 order expiration
  - Issue: IOC orders were rejected with `Only GTD orders may have non-zero expiration`.
  - Fix: V2 order expiration is epoch zero for `Gtc`, `Ioc`, and `Fok`; only `Gtd` carries a non-zero expiration.
  - Validation: Unit coverage in `wire/execution_adapter.rs`.

- [x] 2. Tolerate empty optional Decimal fields in V2 fills sync
  - Issue: V2 fills sync failed with `invalid Decimal: empty string`.
  - Fix: Raw V2 fills parsing treats empty optional decimal fields as `None` and skips only rows with missing essential numeric fields.
  - Validation: Unit coverage for empty optional decimal fallback and empty essential decimal skipping.

- [x] 3. Cap hedge-rescue retries and track in-flight rescue attempts
  - Issue: Hedge rescue thrashed, including 1000+ attempts in one session.
  - Fix: Per-market rescue state tracks signature, attempt count, and in-flight TTL.
  - Validation: Scenario tests cover no repeat while in flight and stop after max attempts.

- [x] 4. Stop user websocket reconnect storm on unchanged market universe
  - Issue: User websocket reconnected several times per second even with unchanged `market_count=1`.
  - Fix: Market subscription comparison now normalizes order, whitespace, and duplicate market IDs.
  - Validation: Unit coverage for equivalent and changed subscriptions.

## P1 - Operational Safety Before Production Scale

- [ ] 5. Restore or intentionally replace persistent V2 API credentials on Dublin
  - Issue: `POLYMARKET_API_KEY`, `POLYMARKET_SECRET`, and `POLYMARKET_PASSPHRASE` are not reliably present in the Dublin live env.
  - Desired test: Startup integration confirms `has_user_credentials=true` after env load, or explicitly confirms runtime derivation is the intended source of truth.
  - Fix shape: Re-derive V2 API credentials through Polymarket auth and persist them to the host env, or make derivation a first-class runtime path with clear logs.

- [x] 6. Wire live RiskOff auto-recovery into the runtime loop
  - Issue: RiskOff was sticky even after the underlying live health issue cleared.
  - Fix: Runtime loop can promote RiskOff back to Running after configured healthy ticks.
  - Validation: Unit coverage for healthy recovery and no recovery while health is still failing.

- [x] 7. Add engine-side USDC.e to pUSD wrap path
  - Issue: pUSD collateral requires manual wrapping from USDC.e before live quoting.
  - Fix: EOA Polygon submitter can approve USDC.e and call the documented pUSD collateral onramp, gated by explicit config.
  - Validation: Unit coverage for 6-decimal scaling and documented contract addresses.
  - Rollout note: Remote tinylive currently has auto-wrap disabled; enable only after checking balances/allowances and confirming spend intent.

- [x] 8. Add paper-vs-live comparison tool
  - Issue: `compare_paper_vs_live.py` did not exist.
  - Fix: Added script to join shadow/live reports or journals by `(market_id, window_start_ms)`.
  - Validation: Python compile check.
  - Rollout note: Another agent may replace this with a richer harness.

- [ ] 9. Make live artifact deployment reproducible
  - Issue: Tinylive is running through `cargo run` / `target/debug`, which makes binary provenance and overwrite behavior too loose.
  - Desired test: Deployed service reports the expected git SHA and binary path.
  - Fix shape: Build a release artifact, deploy it atomically, and point systemd at that artifact rather than rebuilding or running debug binaries in place.

- [x] 10. Fix live CTF merge/redeem collateral routing
  - Issue: Tinylive forced pUSD into `mergePositions`, but the active BTC 5m conditional token IDs were USDC.e-derived. Polygon reverted with `SafeMath: subtraction overflow` when CTF tried to burn zero pUSD-derived position balance.
  - Fix: Added `POLYMARKET_CTF_COLLATERAL_TOKEN_ADDRESS` so order collateral and CTF recycle collateral can differ; tinylive sets CTF collateral to USDC.e.
  - Validation: On-chain `eth_call` replay succeeded with USDC.e and failed with pUSD; relayer unit coverage verifies merge calldata uses configured CTF collateral.

- [x] 11. Stop repeated non-retryable merge resubmits
  - Issue: After a merge revert, reconciliation cleared the pending merge and replanned the same failing CTF transaction every sweep.
  - Fix: Runtime now blocks the exact failed merge signature after non-retryable merge rejection while still allowing retryable failures and changed inventory to proceed.
  - Validation: Runtime regression test covers no second identical merge after non-retryable failure.

- [x] 12. Redeem resolved legacy capital with USDC.e CTF collateral
  - Issue: Resolved/legacy positions needed redeeming after the pUSD/V2 cutover, but CTF recycle must use the collateral that produced the position IDs.
  - Fix: Manual redeem supports `POLYMARKET_REDEEM_COLLATERAL_TOKEN_ADDRESS`; tinylive CTF collateral is set to USDC.e.
  - Validation: Operator redeem completed successfully after the CTF collateral routing fix.

- [ ] 13. Add an operator flow to sweep idle collateral back to MetaMask
  - Issue: Trading capital can sit in the bot signer/intermediary wallet, making it hard to track from the user's normal MetaMask account.
  - Desired test: With no open positions/orders, dry-run reports transferable pUSD/USDC.e balances and execute transfers only to an explicit configured wallet address.
  - Fix shape: Add a guarded one-shot `live_sweep_collateral` mode or documented operator command that verifies flat inventory, keeps MATIC gas reserve, and transfers idle ERC20 collateral to `POLYMARKET_OPERATOR_WALLET_ADDRESS`.

- [ ] 14. Fix graceful shutdown for the live runtime
  - Issue: `systemctl --user stop polymarket-exec@btc_5m_mm_tinylive` timed out and systemd sent SIGKILL.
  - Desired test: Service exits cleanly on SIGINT/SIGTERM before `TimeoutStopSec`.
  - Fix shape: Wire shutdown signal handling through the runtime select loop and close websocket/tasks cleanly.

## P2 - Bandaid-Killing S1 Refactors

- [ ] 15. Introduce `CoolingReason` enum
  - Why: Replaces string-prefix cooling reason checks such as the convex accumulation bandaid.

- [ ] 16. Persist `StrategyTag` and `IntentKind`
  - Why: Removes `client_order_id.starts_with("btc-5m-mm:")` checks across runtime/reporting paths.

- [ ] 17. Introduce typed `EventKind` and `MmQuoteKind`
  - Why: Collapses duplicated string classifiers.

- [ ] 18. Add single `PairLeg::classify` source of truth
  - Why: Collapses repeated yes/no leg detection logic.

- [ ] 19. Introduce typed cancel reasons
  - Why: Replaces string comparisons such as `reason != "no longer desired"`.

- [ ] 20. Feed live fills into `paper_report`
  - Why: Enables fill-level shadow vs tinylive parity, not only decision-level parity.

## P3 - Phase 4 V2 Signal-Derived Calibration

- [ ] 21. Replace `CONVEX_TREND_PERSISTENCE_BPS=50`
  - Target: Derive from about `2.0 * realized_vol_5m_bps`.

- [ ] 22. Replace `CONVEX_MIN_BAR_REMAINING_MS=60_000`
  - Target: Derive from `bar_window_ms / 5`.

- [ ] 23. Replace `CONVEX_MIN_BAR_ELAPSED_MS=60_000`
  - Target: Derive from `bar_window_ms / 5`.

- [ ] 24. Replace `CONVEX_MAX_BIDS_PER_BAR=4`
  - Target: Derive from `min(N, max_leg_cost / typical_clip)`.

- [ ] 25. Add order-flow imbalance signal
  - Target: Rolling 60s taker buy vs sell volume per leg.

- [ ] 26. Add convex self-feedback signal
  - Target: Rolling P&L adjusts the per-bar count cap.

## P4 - Bash Supervisor Cleanup

- [ ] 27. Move market discovery/restart logic out of `scripts/run_unlawful_shear_paper.sh`
  - Why: The engine should refresh context and subscriptions without losing in-flight state.

- [x] 28. Move venue, EOA, and strategy tuning out of `scripts/dublin_tinylive.sh`
  - Why: Launcher scripts should load env and launch the binary only.
  - Fix: `scripts/dublin_tinylive.sh` now delegates to `run_sleeve.sh btc_5m_mm_tinylive`; tinylive settings live in the canonical env.

- [x] 29. Create one canonical tinylive env
  - Why: Live drifted across `common.env`, `live.env`, `paper.d`, repo sleeve env, and launcher exports.
  - Fix: Systemd instance services now load `~/.config/polymarket-exec/%i.env`; `run_sleeve.sh` prefers that host-local canonical env over repo presets.

## P5 - Larger Refactors

- [ ] 30. Split `strategy.rs`
  - Target modules: `strategy/btc_mm.rs`, `strategy/unlawful_shear.rs`, `strategy/goat_pair.rs`, `strategy/types.rs`, and `strategy/profile.rs`.
  - Status: Tests are already extracted; main split remains pending.

- [ ] 31. Split `runtime/runner.rs`
  - Why: File is too large for safe navigation and isolated testing.

- [ ] 32. Split `runtime/mod.rs`
  - Why: File is too large for safe navigation and isolated testing.

- [ ] 33. Add `Gate` trait and `GateOutcome` enum
  - Why: Replaces ad-hoc gate composition and makes paired-vs-convex suppression explicit.

- [ ] 34. Add `BarFraction` timing primitive
  - Why: Makes 5m to 15m strategy timing changes zero or near-zero code changes.

## P6 - Defer Until P0-P3 Are Stable

- [ ] 35. Increase clip size from $1.10 to $4-$5
  - Gate: Only after 5m behavior is statistically stable.

- [ ] 36. Increase ladder depth from 2 levels to 4-8 levels
  - Gate: Only after current live execution quality is stable.

- [ ] 37. Expand to multi-asset or multi-timeframe
  - Gate: Only after we match per-market unlawful execution quality on BTC 5m.
