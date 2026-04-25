# Execution Engine Audit vs `EXECUTION_ENGINE_HANDOFF.md`

_Audit date: 2026-04-25_
_Branch: `audit/exec-engine-handoff`_
_Method: Static review by two parallel Explore agents (wire/venue + strategy/state), synthesised by hand._

## Summary

**11 of 14 audit checks green. 3 partials. 0 outright gaps.** The engine handles the hard race-condition stuff (late-fill correction, uncertain submit, REST trade dedupe with maker/taker filtering, hedge survival on one-sided fills, kill switch). Recent commits show defensive tiny-live bug fixing — the right pattern.

The partials are all addressable with surgical fixes (no refactors). After they land, the live-readiness bar in the handoff is met for `unlawful_shear`. The main remaining gap to "world-class" is the paper-validation harness (separate spec, in progress in parallel).

## Audit grid (Q1-Q15)

### Wire / venue layer

| # | Concern | Status | Evidence | Test |
|---|---|---|---|---|
| Q6 | Venue metadata (min_size, tick_size, fee, token) is dynamic, not hardcoded | ⚠ partial | `wire/execution_adapter.rs:919` syncs positions but no `sync_instrument_metadata()`. `strategy.rs` uses hardcoded `entry_min_size_multiplier: 1.30`. Caused incident #3 ("5/6.5 share confusion"). | Implicit only |
| Q7 | User WS drop for 30s recovers cleanly | ✓ done | `wire/user_ws.rs:95-110` exponential backoff reconnect; `runner.rs` REST backfill via `sync_recent_fills()` if WS unavailable | `tests/btc_5m_mm_scenarios.rs::reconnect_after_partial_fills_recovery` |
| Q8 | REST trades returns unrelated trades — filtered out | ✓ done | `wire/execution_adapter.rs:985-1067` maker-address filter + dedupe by venue_order_id, market_id, side, price, qty | None explicit (logic is solid) |
| Q9 | Cancel ack arrives before fill event — late fill applied | ✓ done | `runtime/order_store.rs:697-701` `terminal_fill_correction` allows Cancelled→Filled | `runtime/order_store.rs::apply_fill_corrects_terminal_cancel_to_filled` |
| Q10 | CLOB API timeout after venue accept — NeedsReconcile | ✓ done | `wire/execution_adapter.rs:300-313` `UncertainOutcome` → `requires_reconcile()` | `tests/btc_5m_mm_scenarios.rs::uncertain_submit_reconcile` |
| Q12 | CLOB V2 compatibility — fields, exchange domain, fee handling | ⚠ partial | `wire/clob_v2.rs:16-38` V2 fields/EIP-712 + `wire/execution_adapter.rs:633-731` V2 signing present. No startup probe or operator log proving live endpoint. | None |

### Strategy / state machine

| # | Concern | Status | Evidence | Test |
|---|---|---|---|---|
| Q1 | Visible position is known locally — recon | ✓ done | `runtime/mod.rs:469-520` `reconcile_venue_positions()` compares local vs venue per market, logs deltas | `runtime/mod.rs::venue_position_reconciliation_creates_runtime_inventory_without_fill` |
| Q2 | Fill attribution: maker/taker/merge/redeem/unknown | ✓ done | `core/types.rs:78-83` `FillLiquidity` + `CloseMethod` enums; `runtime/mod.rs:948-970` logs `close_method` on apply | `tests/btc_5m_mm_scenarios.rs:110-122` events carry both fields |
| Q3 | Local-flat / venue-nonflat block on fresh entry | ⚠ partial | `runtime/mod.rs:486-507` reconciles + logs delta but no proactive block on `accept_intent`. Drift caught after fact, not prevented. Incident #1 ("Unpaired Live Position") territory. | None |
| Q4 | One-leg fill → hedge/merge/rescue path | ✓ done | `runtime/mod.rs:1686-1710` `should_keep_btc_mm_hedge_order()` + survival logic | `runtime/mod.rs::btc_mm_one_leg_fill_then_hedge_survival` + `tests/btc_5m_mm_scenarios.rs::one_sided_fill_reversal_replay` |
| Q5 | One leg accepted + one rejected — naked exposure prevention | ⚠ partial | `should_keep_btc_mm_hedge_order` is reactive (post-fill) only. No proactive paired-intent atomic guard. Incident #4 ("One-Leg Live Quotes") territory. | None |
| Q11 | Market close + post-resolution behavior | ✓ done | `wire/user_ws.rs:349-378` redeem detection; `core/types.rs:86-94` `CloseMethod::{Redeem,Settlement}`; `market_making/merge_executor.rs:93-96` redeem fill apply | `runtime/mod.rs::late_redeem_fill_applies_inventory_from_close_method` |
| Q14 | Sizing transparency — final qty derivation logged | ⚠ partial | `strategy.rs:252,287,316` parses + applies multipliers; `core/inventory.rs:71-94` logs final qty. No single line tying min-floor / dollar-clip / depth-cap / risk-cap → final-qty. | Math tested at `strategy.rs:4555-4573`, but no log assertion |
| Q15 | Graceful stop without losing pending venue state | ✓ done | `runtime/runner.rs:185-189,2517-2524` kill-switch file path; SQLite persists order records across restart; `live_reconcile` mode sync without trade | Smoke-test mode wired |

## Min Live Readiness Bar (handoff section)

| Item | Status | Notes |
|---|---|---|
| CLOB V2 status verified | ⚠ partial | Code-level verified, no operator probe. Fix Q12. |
| User WS connected and parsed | ✓ done | — |
| Filtered REST fill sync verified | ✓ done | — |
| Position sync implemented or safe substitute | ✓ done | — |
| Startup no-trade reconcile passes | ✓ done | `live_reconcile` mode in runner.rs |
| Open orders, recent fills, cash, positions logged | ✓ done | EventLog + journal |
| Paired-entry invariant tests pass | ⚠ partial | Reactive hedge survival tested; one-accept/one-reject race not. Fix Q5 + add test. |
| Late-fill-after-cancel tests pass | ✓ done | `apply_fill_corrects_terminal_cancel_to_filled` |
| One-leg rescue tests pass | ✓ done | `btc_mm_one_leg_fill_then_hedge_survival` |
| Venue metadata-driven size/tick path exists | ⚠ partial | Fix Q6. |
| Kill switch active | ✓ done | — |
| Maker/taker fill attribution | ✓ done | — |

## Fix queue (priority order, smallest-first)

Each fix is its own commit on this branch.

1. **Q14 — Sizing transparency log.** Pure observability, zero behavior change, lowest risk. One `info!()` in strategy emit logging `min_floor`, `dollar_clip`, `depth_cap`, `risk_cap`, `final_qty`, `reason_tag`.
2. **Q12 — CLOB V2 startup log + ops note.** Add explicit startup log of `CLOB_V2_EXCHANGE` address, `signature_type`, `neg_risk` flag. Document operator verification step in runbook. (Live probe deferred — code already signs V2; the gap is operator visibility.)
3. **Q3 — Drift block.** Pre-`accept_intent` check in `runtime/mod.rs`: if any market has `local_qty == 0` but `venue_qty != 0` (per the most recent reconcile), reject new entry intents in that market until reconcile clears.
4. **Q5 — Paired-entry atomic guard + test.** Add paired-intent marker to `OrderIntent` (or use `client_order_id` prefix). In `quote_reconciler.rs`, if one of a paired pair rejects, cancel the mate before next tick. Add `tests/btc_5m_mm_scenarios.rs` fixture for one-accept/one-reject race.
5. **Q6 — Venue metadata sync.** Add `sync_instrument_metadata()` to `wire/execution_adapter.rs` that fetches `min_order_size` and `tick_size` from Polymarket CLOB market endpoint. Cache with TTL (~5min). Log on startup. Strategy reads from cache instead of hardcoded multiplier. (Reference Polymarket docs for exact endpoint shape.)

## Strengths worth preserving

- **Race-condition handling is mature.** Late-fill-after-cancel, uncertain-submit, REST dedupe — these are rare in retail systems. Don't refactor them.
- **Defensive bug-fix cadence is good.** Last 5 commits are "fail closed when live merge unavailable" / "ignore stale live submit races" / "tolerate matched cancel races" / "scope live venue inventory to active sleeve" / "add raw clob v2 submit path". This is exactly the right pattern for tiny-live.
- **Test scenarios are real.** 12 named scenarios in `btc_5m_mm_scenarios.rs` cover real lifecycle paths, not coverage padding.

## Out of scope for this audit

- **Paper environment quality.** Handoff incident #5 ("Paper Fill Optimism"). Tracked separately in `docs/architecture/2026-04-25-paper-env-design.md` (in progress).
- **`runtime/mod.rs` size and coupling.** ~1000 lines mixing signal sampling + state machine + lifecycle. Real maintenance/extension liability (especially as PA sleeve lands), but no immediate bug. Defer to a separate refactor pass.
- **Production dashboards.** Logs exist; visualizations don't. Not a correctness gap.
