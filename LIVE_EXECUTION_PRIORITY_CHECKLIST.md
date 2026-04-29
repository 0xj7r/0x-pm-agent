# Live Execution Priority Checklist

Working checklist for the live `polymarket-exec` engine. Check items only when the repo has the implementation and focused validation for that item. Operational rollout notes belong in the item notes until the deployed service is confirmed healthy.

## Session Anchor (2026-04-29)

Apr 28 raw activity for the canonical maker-active wallet `0xb27bc932...` showed multiple priors encoded in the codebase were stale. The earlier reconstruction doc (`docs/research/unlawful-shear-reconstruction-thread.md`) is no longer trusted as ground truth. Fresh analysis showed: zero sells (capital recycled via merge/redeem only), broad price distribution $0.05-$0.95 (not bimodal), bursty sub-second cadence (not steady), 13:1 redeem:merge ratio, ~310 trades/market/day across 227 markets. Items 49-56 below capture the structural follow-ups.

**Single highest-leverage next item: item 21 (operator calibration report).** Without it, no further fix can be validated as helpful or harmful. Treat as the immediate unlock; everything else compounds from it.

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

- [x] 4a. Fix dust-pinned inventory + permanent blocked-merge deadlock
  - Issue: Sub-venue-min residuals (qty < 5.0 but > 1e-9) pinned `market_has_inventory=true`, which blocked `maybe_clear_market_timing` from clearing `blocked_merge_by_market`. After a CTF merge revert the same-signature merge stayed blocked forever because inventory never cleared.
  - Fix: `actionable_order_qty_for_market` returns venue minimum_order_size; `position_is_actionable_inventory` and `market_has_inventory` use it. Blocked merges retry after a 15-second backoff (`BLOCKED_MERGE_RETRY_AFTER_MS`) instead of waiting for inventory change. Commit `392ade3`.
  - Validation: `sub_venue_min_single_leg_dust_is_not_actionable_inventory` and `blocked_merge_retries_after_backoff_instead_of_permanent_suppression` cover both legs of the deadlock.

- [x] 4b. Exempt Close (rescue) intents from `enforce_unlawful_mode` cancel sweep
  - Issue: The cleanup-mode cancel filter only excluded `reduce_only` orders, so hedge-rescue lift orders (kind=Close, reduce_only=false) were cancelled mid-flight whenever the unlawful gate transitioned to Cleanup or Flatten. This was the missed 5th cap-bypass site CLAUDE.md predicted alongside risk.rs, quote_reconciler, strategy.rs, and accept_intent.
  - Fix: Add `kind != IntentKind::Close` to the cancel filter. Commit `bd28ff0`.
  - Validation: `enforce_unlawful_mode_does_not_cancel_close_rescue_intents` asserts a working rescue Close intent survives a Manage→Cleanup transition.

- [x] 4c. Derive avg_cost from total_bought when venue avg_price is zero
  - Issue: `venue_position_from_data_position` only read `avg_price`, ignoring `initial_value` and `total_bought` even though all three are in the Polymarket Data API Position struct. When the venue returned `avg_price=0` for inherited or post-split positions, parser silently lost recoverable cost-basis info, leaving positions stuck in `decide_stranded_exposure`'s "unknown cost basis" branch.
  - Fix: Fall back to `total_bought / size`, then `initial_value / size`, before defaulting to zero. Commit `bd7c16c`. Strictly additive — positions with positive `avg_price` keep that value.
  - Validation: `data_api_position_derives_avg_price_from_total_bought_when_avg_is_zero` covers the recoverable case; existing `btc_5m_mm_holds_stranded_inventory_when_cost_basis_is_unknown` remains valid for the genuine all-zero case.

- [x] 4d. Remove regime-trending and regime-inactive paired suppression
  - Issue: `regime_entry_mode` skipped paired entries when `return_60s > 15.0bps` or BTC `trade_count_5m < 30` with `realized_vol_5m_bps < 1.0`. Apr 28 raw activity for `0xb27bc932` showed 69k trades through normal BTC volatility windows with no observable self-imposed paired suppression beyond venue health. The hardcoded thresholds fired on most active minutes (BTC routinely oscillates 20-50 bps/min) without empirical support.
  - Fix: Remove both regime branches from `regime_entry_mode`; keep price-based guards (`market mid moved`, `premium fair cap`). Convex_accum still gates on its own trend persistence signal at strategy.rs:2828. Commit `cb76fdd`.
  - Validation: `btc_5m_mm_emits_paired_bids_through_normal_btc_volatility` asserts paired bids fire at `return_60s=20bps`.

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

- [ ] 15. Opportunistically maintain a pUSD trading float from USDC.e reserves
  - Issue: V2 trading consumes pUSD, but deposits/redeems/operator tracking can leave idle USDC.e outside the CLOB trading balance.
  - Desired test: With pUSD below a configured floor, USDC.e above reserve, gas below cap, and the top-up cooldown elapsed, the runtime wraps only the top-up amount through CollateralOnramp; otherwise it logs the skipped reason and submits no transaction.
  - Fix shape: Add an engine-side balance worker that logs pUSD, USDC.e, and MATIC separately, keeps explicit USDC.e/MATIC reserves, enforces gas-price and minimum-interval guards, and wraps asynchronously to `WHALE_PAIR_LIVE_PUSD_TARGET_USD` rather than blindly converting all USDC.e.

## P1.5 - Deployment And Process Hardening

- [ ] 16. Build a proper deployment pipeline
  - Issue: Deploys can install code without restarting the service, and the operator has to manually infer whether the running process picked up the new binary.
  - Desired test: One deploy command builds the release binary, installs it atomically, restarts the intended service, and verifies the service is active on the expected binary/version.
  - Fix shape: Make the deploy pipeline produce a single immutable release artifact, record git SHA/build metadata, install via atomic symlink or versioned path, restart only the named tinylive service, and print the exact active PID/binary/git SHA after rollout.

- [ ] 17. Add pre-deployment checks
  - Issue: We deployed into a full remote disk and only found out when the live process failed to flush its journal.
  - Desired test: Preflight fails before deployment if local git state is dirty unexpectedly, required tests fail, remote disk is below reserve, env validation fails, or the target service/env/binary path is ambiguous.
  - Fix shape: Add a `preflight` step that checks `cargo test -p polymarket-exec --lib`, local/main SHA alignment, remote disk free space, remote journal writability, canonical env presence, required secrets, Polygon RPC, CLOB/Data API reachability, kill-switch state, and systemd unit target.

- [ ] 18. Add post-deployment smoke checks and rollback path
  - Issue: A successful build/install is not the same as a healthy live bot.
  - Desired test: After deploy, smoke checks prove the service stays active for N seconds, writes a journal record, syncs venue cash, connects market/user/spot websockets, exposes metrics, and has no crash loop or deterministic venue rejects.
  - Fix shape: Add deploy-time health gates plus rollback to the previous installed binary if startup fails or health checks do not pass.

- [ ] 19. Harden shadow/paper environments
  - Issue: Paper/shadow artifacts filled the production host disk and took live trading down.
  - Desired test: Running paper/shadow cannot consume the live root disk past a configured reserve, cannot write under live execution paths, and cannot share the tinylive env by accident.
  - Fix shape: Separate paper/shadow data roots from live data, enforce retention/rotation, cap artifact size, add disk-reserve checks, and keep paper services isolated from live service env/secrets.

- [ ] 20. Add remote artifact retention and disk guardrails
  - Issue: Remote build caches and execution artifacts can silently consume the small EC2 root volume.
  - Desired test: A scheduled or deploy-time guard reports disk usage by category and refuses live start when journal/order-store writes would fail.
  - Fix shape: Keep only bounded Cargo cache, bounded paper artifacts, bounded journals, and explicitly preserve live order-store/audit data.

- [ ] 21. Add operator-facing live calibration report **— IMMEDIATE NEXT UNLOCK; everything else compounds from this**
  - Issue: Polymarket UI activity does not tell us whether fills were maker-rebate eligible, taker rescues, convex accumulation, merges, or redeems.
  - Desired test: Report answers, per market/session, maker vs taker fills, intent kind, quote kind, notional, estimated fees/rebates, merge/redeem recycling, and convex-vs-paired capital use.
  - Fix shape: Persist fill liquidity plus strategy tag/intent kind/quote kind into the live report path and expose a command or metrics endpoint for daily calibration.
  - Why blocking: Without this, no further fix can be validated as helpful or harmful, no constant can be derived from data, and no architectural refactor (Gate trait, etc.) has empirical guidance for what the unified gate should do. Items 49-56 below all depend on this being live.

## P2 - Bandaid-Killing S1 Refactors

- [ ] 22. Introduce `CoolingReason` enum
  - Why: Replaces string-prefix cooling reason checks such as the convex accumulation bandaid.

- [ ] 23. Persist `StrategyTag` and `IntentKind`
  - Why: Removes `client_order_id.starts_with("btc-5m-mm:")` checks across runtime/reporting paths.

- [ ] 24. Introduce typed `EventKind` and `MmQuoteKind`
  - Why: Collapses duplicated string classifiers.

- [ ] 25. Add single `PairLeg::classify` source of truth
  - Why: Collapses repeated yes/no leg detection logic.

- [ ] 26. Introduce typed cancel reasons
  - Why: Replaces string comparisons such as `reason != "no longer desired"`.

- [ ] 27. Feed live fills into `paper_report`
  - Why: Enables fill-level shadow vs tinylive parity, not only decision-level parity.

## P3 - Phase 4 V2 Signal-Derived Calibration

- [ ] 28. Replace `CONVEX_TREND_PERSISTENCE_BPS=50`
  - Target: Derive from about `2.0 * realized_vol_5m_bps`.

- [ ] 29. Replace `CONVEX_MIN_BAR_REMAINING_MS=60_000`
  - Target: Derive from `bar_window_ms / 5`.

- [ ] 30. Replace `CONVEX_MIN_BAR_ELAPSED_MS=60_000`
  - Target: Derive from `bar_window_ms / 5`.

- [ ] 31. Replace `CONVEX_MAX_BIDS_PER_BAR=4`
  - Target: Derive from `min(N, max_leg_cost / typical_clip)`.

- [ ] 32. Add order-flow imbalance signal
  - Target: Rolling 60s taker buy vs sell volume per leg.

- [ ] 33. Add convex self-feedback signal
  - Target: Rolling P&L adjusts the per-bar count cap.

## P4 - Bash Supervisor Cleanup

- [ ] 34. Move market discovery/restart logic out of `scripts/run_unlawful_shear_paper.sh`
  - Why: The engine should refresh context and subscriptions without losing in-flight state.

- [x] 35. Move venue, EOA, and strategy tuning out of `scripts/dublin_tinylive.sh`
  - Why: Launcher scripts should load env and launch the binary only.
  - Fix: Removed the `scripts/dublin_tinylive.sh` live launcher entirely. Tinylive runs only through the systemd unit and the host canonical env.

- [x] 36. Create one canonical tinylive env
  - Why: Live drifted across `common.env`, `live.env`, `paper.d`, repo sleeve env, and launcher exports.
  - Fix: Systemd instance services now load `~/.config/polymarket-exec/%i.env`; `run_sleeve.sh` prefers that host-local canonical env over repo presets.

## P5 - Larger Refactors

- [ ] 37. Split `strategy.rs`
  - Target modules: `strategy/btc_mm.rs`, `strategy/unlawful_shear.rs`, `strategy/goat_pair.rs`, `strategy/types.rs`, and `strategy/profile.rs`.
  - Status: Tests are already extracted; main split remains pending.

- [ ] 38. Split `runtime/runner.rs`
  - Why: File is too large for safe navigation and isolated testing.

- [ ] 39. Split `runtime/mod.rs`
  - Why: File is too large for safe navigation and isolated testing.

- [ ] 40. Add `Gate` trait and `GateOutcome` enum
  - Why: Replaces ad-hoc gate composition and makes paired-vs-convex suppression explicit.

- [ ] 41. Add `BarFraction` timing primitive
  - Why: Makes 5m to 15m strategy timing changes zero or near-zero code changes.

## P6 - Defer Until P0-P3 Are Stable

- [ ] 42. Increase clip size from $1.10 to $4-$5
  - Gate: Only after 5m behavior is statistically stable.

- [ ] 43. Increase ladder depth from 2 levels to 4-8 levels
  - Gate: Only after current live execution quality is stable.

- [ ] 44. Expand to multi-asset or multi-timeframe
  - Gate: Only after we match per-market unlawful execution quality on BTC 5m.
