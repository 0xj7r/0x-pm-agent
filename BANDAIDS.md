# Bandaid Catalog — 2026-04-28

Output of systematic discovery scan run 2026-04-28 across 4 dimensions:
type-system, config-name alignment, telemetry asymmetry, bash supervisor smells.
Each finding tagged S0 (lost money / wrong behavior), S1 (likely future regression),
S2 (hygiene only).

## Top S0 (production silent failures)

| # | Finding | File:line | Category | Fix shape |
|---|---|---|---|---|
| C-3 | `WHALE_PAIR_LIVE_AUTO_REDEEM` never set in `dublin_tinylive.sh` → auto-redeem silently disabled in production | `scripts/dublin_tinylive.sh` (missing) | config | Add `export WHALE_PAIR_LIVE_AUTO_REDEEM=true` and `_PERIOD_SEC=300`. Stops manual redeem dance. |
| T-1 | `paper_report` writer construction gated on `if config.paper_mode` → live mode writes no decision log | `runtime/runner.rs:932-943` | telemetry | Construct iff `config.paper_report_path.is_some()`. Mirror `book_snapshot` pattern. |
| T-3 | `record_book_observation` only fires in paper → no follow-through marks for live suppression samples | `runtime/runner.rs:1591-1593` | telemetry | Drop `if let Some(report)` wrap (transitively fixed by T-1). |
| B-1 | `polymarket-exec/scripts/run_unlawful_shear_paper.sh:170-267` — full bash supervisor loop with context-hash restart kills in-flight engine state | `polymarket-exec/scripts/run_unlawful_shear_paper.sh` | bash | Extend engine-side `runtime/runner.rs:1872-1970` discovery to be strategy-agnostic. |
| B-2 | Python Gamma pre-pull duplicated by both `dublin_tinylive.sh:116-123` AND the engine's discovery tick — two implementations of the same mapping that can drift | `scripts/dublin_tinylive.sh`, `scripts/live_smoke.sh` | bash | Drop Python pre-pull, engine self-seeds. |
| C-1 | `WHALE_PAIR_BTC_5M_MM_SELL_CLIP_USD=1.10` in env file but no consumer in code | `polymarket-exec/env/btc_5m_mm_tinylive.env:42` | config | Either wire to `Btc5mMmConfig` (if intended) or delete from env file. |
| C-2 | `WHALE_PAIR_BTC_5M_MM_ALLOW_SINGLE_LEG_ENTRY=false` in env file but flag was dead (just removed in commit `cbdf743`) | `polymarket-exec/env/btc_5m_mm_tinylive.env:60` | config | Delete from env file (already removed from code today). |

## S1 findings (will cause regressions if left)

### Telemetry (T)
- **T-5**: Live user_ws fills (`runner.rs:2449/2487/2517`) bypass `paper_report.record_fill` → live report would have zero fills even after T-1 fixed. Thread report into `handle_user_event`.
- **T-6**: Same for `apply_sync_report` venue-driven fills (`runner.rs:3825`).
- **T-7**: `record_submit_edge` only fires on paper submit branch (`runner.rs:2902-2912`) → live has no expected-edge denominator.
- **T-8**: `record_reject` only fires on paper synthetic post-only rejects → live venue rejects don't increment `paper_report.reject_count`.

### Type-system (TS)
- **TS-1**: `should_keep_btc_mm_hedge_order` does `if reason != "no longer desired"` to detect reconciler-driven cancels. Cancel origin should be a typed enum (`CancelOrigin::{...}`), not a string compare. (`runtime/mod.rs:2245`)
- **TS-2**: `is_btc_mm_buy_intent` uses `client_order_id.starts_with("btc-5m-mm:")` to detect strategy ownership. Add `strategy_tag: StrategyTag` enum on `OrderIntent`. (`runtime/mod.rs:2289`)
- **TS-3**: TWO places (`runtime/mod.rs:3060` and `:3102`) reconstruct IntentKind from `tag.starts_with("mm-hedge-rescue")` for orders restored from checkpoint. Persist `kind: IntentKind` on `RuntimeCheckpointOrder` and `OrderRecord`.
- **TS-4**: `classify_runtime_event(message: &str)` duplicated VERBATIM in `runner.rs:2671` and `paper/report.rs:805`. Both pattern-match on free-form `EventRecord.message` text. Promote to typed `EventKind` enum on `EventRecord`.
- **TS-5**: `classify_runtime_command` and `classify_submit_intent` each do four `tag.starts_with("mm-paired-bid"|"mm-convex-accum"|"mm-hedge-rescue"|"mm-reduce")` checks for a metric label. Add `MmQuoteKind` enum.
- **TS-6**: `Btc5mMmMarketMode::Cooling { reason: String }` — strategy formats reason then parses its own output via `cooling_reason_key` and `cooling_allows_convex_accumulation` (string-prefix matches). Promote to `CoolingReason` enum with `{MarketMidMoved{move_bps}, PremiumFairCap{max_fair}, BtcRegimeFlat{vol_bps,trades}, BtcRegimeTrending{return_bps}, AsymmetricEntryFillCooldown}` and `Display` for human text. (`strategy.rs:673-680, 2041-2061`)
- **TS-7**: `is_own_entry_fill` does string-pattern detection on COID. Same fix as TS-2 (StrategyTag) + IntentKind on FillReport.
- **TS-8**: **FOUR copies** of yes/no/up/down/long/short instrument-leg classifier (`strategy.rs:2191, 4418`, `runner.rs:2600`, `pair_ledger.rs:105`) with subtly different keyword sets. Promote `PairLeg::classify` in `pair_ledger.rs` to SSOT. Even better: store leg classification on `MarketContextRecord`.
- **TS-9**: `dashboard_refresh_ms: u64` parsed from env, stored on `AppConfig`, never read. (`config/mod.rs:88, 362-363, 502`)

### Bash supervisor (B)
- **B-3**: `dublin_tinylive.sh:43-46` hardcodes `POLYMARKET_CLOB_API_URL`, `POLYMARKET_CLOB_VERSION=v1`, USDC.e collateral address (override AFTER sourcing `.env`, so env file can't win). Move venue config to `polymarket-exec/env/dublin_tinylive.env` as SSOT.
- **B-4**: `dublin_tinylive.sh:39-41` does the `unset POLYMARKET_FUNDER_ADDRESS POLYMARKET_FUNDER` then `export ''=''` ritual. Bash workaround for inconsistent empty-string vs absent handling in `wire/execution_adapter.rs:617`. Validate at engine startup, drop the dance.
- **B-5**: `live_redeem.sh:53-58` exports placeholder `WHALE_PAIR_ASSET_IDS=1` etc. to bypass `split_csv_required` validation in redeem mode. Branch on `WHALE_PAIR_EXEC_MODE=live_redeem` in `config/mod.rs::from_env`.

### Config (C)
- **C-4**: `live_redeem.sh:13-14` docstring references `POLYMARKET_SECRET`, `POLYMARKET_PASSPHRASE`, `POLYMARKET_BOT_PRIVATE_KEY` — none are real (consumers expect `POLYMARKET_API_SECRET`, `POLYMARKET_API_PASSPHRASE`, `POLYMARKET_PRIVATE_KEY`). Operator following docs gets silent auth failure.
- **C-5**: `RELAYER_API_KEY` validation missing in `live_redeem.sh` startup — silent failure if name mismatches.
- **C-6**: `WHALE_PAIR_LIVE_KILL_ON_RECONCILE_MISMATCH` defaults to `true`, never overridable by docs. Single transient mismatch will degrade bot. Document or expose.

## S2 findings (hygiene)

### Type-system
- **TS-10**: `submit_rejection_counts_against_live_budget(reason: &str)` lowercases venue text to detect "post-only-cross". Wire layer already classifies into `ExecutionError`. Add `ExecutionError::PostOnlyCrossed` variant.
- **TS-11**: `core/risk.rs:120` round-trips typed `RiskRejectReason` enum through `Debug` formatting to land as `Option<String>` in `EventMetrics`. Keep typed.
- **TS-12**: `DesiredQuote` carries both `is_cleanup: bool` and `suppress_if_stale: bool` plus `expires_at_ms`. File defines orphan `enum StaleMode {Ignore, Remove}` and `enum ExpiryMode {Ignore, Remove}` (lines 64-74) that were never wired. Promote.

### Bash
- **B-6**: 50+ LOC of strategy tuning knobs in `dublin_tinylive.sh:51-100` (with rationale comments). Move to versioned env file.
- **B-7**: `WHALE_PAIR_CONTEXT_REFRESH_INTERVAL_SEC` is bash-only — duplicate of engine-side `WHALE_PAIR_MARKET_DISCOVERY_INTERVAL_MS`.
- **B-8**: Hardcoded EOA wallet address in `dublin_tinylive.sh:3, 139` comment + banner. Have engine print derived wallet at startup.
- **B-9**: `live_smoke.sh:48-66` re-exports 18 lines of tinylive risk caps. Should `source` the same env file.

### Config
- **C-7**: `WHALE_PAIR_TAKER_FEE_COEFF` read in three different strategy constructors with same default. Factor up.
- **C-8**: 98 of 138 `WHALE_PAIR_*` env vars consumed by binary are NOT set by `dublin_tinylive.sh` → running on engine defaults. Most are correctly absent (paper-only or unlawful-only) but ~5-10 live-relevant ones (`LIVE_ORDER_TTL_MS`, `LIVE_ORDER_MAX_AGE_MS`, `LIVE_RECONCILE_AFTER_MS`) silently default. Audit which defaults are intentional for production.

## Found this session, not yet in catalog

- **Merge fills trip post-fill cooldown** — `strategy.rs:3503` updates `last_fill_ms` on Merge-method fills, triggering 15s entry cooldown after merges that should let next paired cycle start immediately. Same family as #56 (capital guard not bypassing IntentKind::Close). Fix proposed in this session: skip `last_fill_ms` update when `fill.close_method == Some(CloseMethod::Merge)`. **SHIPPED in commit 7a38b15.**

- **Persisted RiskOff with no auto-recovery (S0)** — Commit `2982576 Persist risk-off runtime state` writes RiskOff to `runtime_state` table in `orders.sqlite`. On every restart, `restored runtime status from durable store status=RiskOff` reads it back and the bot starts in RiskOff. Suppresses ALL entries indefinitely until operator manually clears the table (`DELETE FROM runtime_state;`). Once we trigger RiskOff once (e.g., by a transient reconcile mismatch), the bot is dead until human intervenes. **Hit this twice today.** Fix shape: add either (a) auto-recovery — clear RiskOff after N healthy ticks post-restart, (b) `WHALE_PAIR_LIVE_RECOVER_FROM_RISK_OFF=true` env to override on startup, or (c) explicit operator runbook for clearing the state. Recommend (a) with N=10-30 ticks (10-30s) as the safe default — gives the protection without trapping the bot indefinitely.

## Anti-pattern shapes (so we can recognize the next one)

1. **String-prefix as type discriminator**: any `.starts_with("mm-")`, `.contains(":")`, `if reason == "..."`, `format!("{...}")` → parse-back. Should be enum + `Display`.
2. **Mode-coupled telemetry**: any observability gated on `paper_mode` or `live_mode`. If it's a count/log of events that exist in both modes, it should be path-driven, not mode-driven.
3. **Duplicated classifier**: same `match`/`if` ladder in 2+ files. Promote to a single typed function on the type itself.
4. **Dead config knob**: env var parsed and stored but never read. Either wire or delete.
5. **Bash business logic**: scripts that poll, restart, retry, or know about strategy state. Move to engine; deploy script does env+launch only.
6. **Boolean flag plumbing**: 3+ touchpoints for one concept. Promote to enum variant.

## What to fix first (ordering for the refactor pass)

1. **C-3 auto-redeem env** — 2-line launcher fix, eliminates manual redeem dance, **today**.
2. **C-1, C-2 dead env entries** — remove from env file, **today**.
3. **Merge-fills-trip-cooldown** — 5-line strategy fix, **today**.
4. **T-1 + T-2 + T-3 + T-4 — `DecisionLog` extraction** — single PR, decouples paper_report from paper_mode, unlocks shadow ↔ tinylive comparison. ~half day.
5. **T-5 + T-6 + T-7 + T-8 — wire live ingress points to DecisionLog** — second PR after T-1. ~half day.
6. **TS-6 — `CoolingReason` enum** — replaces the string-prefix bandaid we just added in `cooling_allows_convex_accumulation`. ~2 hours.
7. **TS-1 + TS-2 + TS-3 + TS-7 — strategy_tag + persist IntentKind on records** — ~half day.
8. **TS-4 + TS-5 — typed `EventKind` + `MmQuoteKind`** — ~2 hours, eliminates classifier duplication.
9. **TS-8 — single `PairLeg::classify` SSOT** — ~1 hour, eliminates 4 copies.
10. **B-1 — engine-side discovery for unlawful_shear strategy** — biggest, ~1 day.
11. **B-3, B-4, B-6 — move all config out of `dublin_tinylive.sh` into versioned env file** — ~2 hours.
12. **TS-9 — delete dead `dashboard_refresh_ms`** — ~10 min.

## Total

- **40 findings** across 4 categories
- **7 S0** (production silent failures or capital risk)
- **18 S1** (will cause future regressions)
- **15 S2** (hygiene)

The S0 list is short and tractable. The S1 list is dominated by string-prefix anti-patterns that share a common fix shape: promote to enum.
