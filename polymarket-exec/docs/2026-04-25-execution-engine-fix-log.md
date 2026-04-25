# Execution Engine Fix Log

_Session: 2026-04-25_
_Baseline: `160701a`_

This log records every behavior-changing fix landed during the audit +
paper-env build session, mapped to the design requirements they satisfy.
Each entry: what was wrong, what's now true, why this matches the
overall strategy/requirements, and where to verify.

The reference design + requirements docs live at:
- `EXECUTION_ENGINE_HANDOFF.md` — venue/lifecycle requirements
- `polymarket-exec/docs/2026-04-25-handoff-audit.md` — gap audit
- `docs/architecture/2026-04-25-paper-env-design.md` — paper env design
- `polymarket-exec/docs/2026-04-25-paper-env-calibration-runbook.md` — calibration loop

## Audit fixes (handoff acceptance checks)

### Q14 — Sizing transparency log (`06ff5c3`)
**Was:** sizing math existed (`paired_entry_quantity` in
`strategy.rs:1508`) but no single log line tied min-floor / dollar-clip
/ depth-cap / risk-cap → final-qty.
**Now:** explicit `info!()` in `strategy.sizing` target prints all
inputs + final qty + outcome tag on every sizing decision (and
rejection).
**Why this matches strategy:** handoff Acceptance Check #14 ("What logs
would explain the final submitted size and price?"). Required for the
"5/6.5 share confusion" incident class to be diagnosable.

### Q12 — CLOB V2 startup config log (`1cad481`)
**Was:** V2 signing fields/exchange domain wired but no operator-visible
proof at startup.
**Now:** `live_auth.startup` log emits `clob_v2_exchange`,
`clob_v2_neg_risk`, `clob_v2_builder_code_present`, signature_type,
funder/proxy addresses on every connect.
**Why this matches strategy:** handoff "venue mechanics" requires
verifying V2 readiness before live trading. Operator now has one log
line to confirm the right protocol is in use.

### Q3 — Drift block (`34d271e`)
**Was:** `reconcile_venue_positions` logged local-flat / venue-nonflat
drift but didn't block fresh entry. Incident #1 territory ("Unpaired
Live Position").
**Now:** `markets_with_unresolved_drift` set on `Runtime`. When venue
recon detects local=0 and venue!=0, the market is added; `accept_intent`
rejects new entry intents (reduce_only allowed) until reconcile clears.
**Why this matches strategy:** handoff "fail closed when local truth
and venue truth disagree" + Acceptance Check #3.

### Q5 — Paired-entry atomic guard + test (`471a6fd`)
**Was:** paired-entry relied on reactive `should_keep_btc_mm_hedge_order`
(post-fill). If one leg was venue-rejected, the accepted mate could
remain naked. Incident #4 territory.
**Now:** `OrderIntent.pair_id` (Option<String>); strategy tags both legs
of a pair with the same id; `on_order_rejected` looks up the pair and
emits Cancel for any open mate. Test `paired_entry_rejection_cancels_mate`
covers the one-accept/one-reject race.
**Why this matches strategy:** handoff Acceptance Check #5 + the
maker-first paired-entry invariant.

### Q6 — Venue metadata fetch (`81a009c`)
**Was:** strategy used hardcoded `entry_min_size_multiplier` (1.30) +
default min order qty; no venue-side check.
**Now:** `fetch_market_metadata(condition_id)` on `PolymarketExecutionAdapter`
returns `MarketMetadata { minimum_order_size, minimum_tick_size,
neg_risk, active, closed }`. `run_live_reconcile` deduplicates by
condition_id and logs venue truth so operators can spot mismatches.
**Why this matches strategy:** handoff "live min size comes from venue
metadata" requirement. Closes the "5/6.5 share confusion" visibility
gap. Auto-wiring strategy to consume the cached metadata is a future
follow-up (current closure is operator-visibility only).

## Paper environment (per design doc, Phases 1-5)

### Phase 1: market-close handler (`d2c9772`, `55972d9`)
- New `Runtime::plan_paper_close(now_ms, resolution_price)`: cancels all
  open orders, merges paired inventory, applies synthetic redeem fills
  for stranded inventory at resolution_price. Wired into `run_runtime_loop`
  to fire when clock crosses `paper_market_close_at_ms`.
- New env vars: `WHALE_PAIR_PAPER_MARKET_CLOSE_AT_MS`,
  `WHALE_PAIR_PAPER_MARKET_RESOLUTION_PRICE`.
- Plus: switched default `POLYMARKET_CLOB_VERSION` from "v1" to "v2"
  (`4a3c9da`) — V2 cutover was 3 days away.
**Why:** design Phase 1; required so paper sessions don't accumulate
unbounded marks across market boundaries.

### Phase 2: conservative fill model (`9333bf4`, `3fc1b1e`, `4fc58a6`, `d37e358`, `416f029`)
Five chunks:
- **chunk 1:** `paper_submit_latency_ms` (default 150ms) gate
- **chunk 2:** `paper_queue_depth_fraction` (default 0.75) replaces opaque
  `queue_bias` for non-crossing maker fills (claims `1 - 0.75 = 25%` of
  top-of-book per attempt; Moallemi-Yuan inspired)
- **chunk 3:** `paper_post_only_reject_probability` (default 0.85) —
  deterministic per (client_order_id, book_update_ms) so replays are
  reproducible
- **chunk 4:** `paper_cancel_race_window_ms` (default 500ms) — defers
  `on_order_cancelled` so late fills can still apply within window
- **chunk 5 (added in fix commit `416f029` after replay surfaced it):**
  threaded `observed_at_ms` through `paper_fill_ratio`, replacing
  internal `now_unix_ms()` calls. Was a hidden bug breaking replay
  determinism.
**Why:** design Phase 2; closes the "Paper Fill Optimism" incident #5.

### Phase 3: report card (`c57b0bf`, `4ea1f5d`, `01af8c8`)
- New `polymarket-exec/src/paper/report.rs` with `PaperReportWriter`,
  `PaperFillRecord`, `PaperReportSummary`.
- Captures: per-fill side / limit / fill price / qty / fee / liquidity /
  mid_at_submit / slippage_bps / realized_edge_usd; aggregates into
  fills/edge/queue/vs_whale stats; flushes JSON on shutdown.
- vs_whale section auto-populated from `dashboard_whale_events_path`
  filtered to session window.
**Why:** design Phase 3; gives operators evidence (not just logs) for
calibration decisions. vs_whale is the side-by-side validation the
calibration runbook depends on.

### Phase 4: shadow_live mode (`46c867e`)
- `WHALE_PAIR_EXEC_MODE=shadow_live` → `run_shadow_live` forces
  `paper_mode=true` at the binary entry point (safety contract: no
  capital can leak even if env is misconfigured).
- New env preset `polymarket-exec/env/btc_5m_mm_shadowlive.env`.
**Why:** design Phase 4; the primary mechanism to validate strategy
behavior against real venue data without exposing capital.

### Phase 5: book snapshot capture + replay (`e04c92f`, `3cfb01d`, `39898bb`, `aa20cb7`, `416f029`)
- `polymarket-exec/src/paper/snapshot.rs` — `BookSnapshotWriter`
  writes compact JSONL per book update.
- `polymarket-exec/src/paper/replay.rs` — `ReplayBookRecord` reader.
- `WHALE_PAIR_EXEC_MODE=replay` drives `Runtime<StrategyMode>` through
  the recorded book sequence using `record.t` as the replay clock.
- `polymarket-exec/scripts/compare_replay.py` diffs two paper reports.
- **Critical fix in `aa20cb7`:** added retry-loop in `run_replay_cli`
  so existing open orders get fill attempts on subsequent snapshots
  (mirrors production `execute_execution_adapter` retry loop).
- **Critical fix in `416f029`:** maker/taker classification + maker
  fill price + paper_fill_ratio clock determinism (see "Bug fixes
  surfaced by real-data calibration" section below).
**Why:** design Phase 5; enables A/B parameter calibration without
waiting on live windows.

## Bug fixes surfaced by real-data calibration (`416f029`)

This commit fixed three related bugs in `paper_fill_from_book_snapshot`
that only manifested when running real Polymarket book snapshots through
the strategy-driven replay path.

**Bug 1 — Resting orders misclassified as TAKER when book moved into them.**
A maker buy at 0.45 sitting on the book; book later moves so best_ask
drops to 0.43. Old code classified the resulting fill as TAKER at 0.43.
Real venue: a resting limit buy remains a maker even when the book
moves into it; whoever submits a sell at our 0.45 gets price-improved
and we fill at 0.45 as MAKER. Fix: distinguish "fresh" (within
`paper_submit_latency_ms` of arrival) from "resting"; `crosses_as_taker`
only true when fresh AND crossing.

**Bug 2 — Maker fills used opposite-side touch as fill price.**
Even non-crossing maker fills computed `price = best_opposite`. Real
maker fills land at the maker's limit price. Fix: per-level loop now
uses `intent.limit_price` for makers, `level.price` for takers.

**Bug 3 — `paper_fill_ratio` used wall clock instead of `observed_at_ms`.**
`age_ms` and `staleness_pressure` were computed from `now_unix_ms()`
internally, breaking replay determinism (fill ratios depended on host
speed, not on the recorded sequence). Fix: thread `observed_at_ms`
through and use it.

These bugs all aligned to the same root pattern: the paper model treated
"current book state" as the source of truth at fill evaluation time,
ignoring the order's resting history and the simulated clock. The fix
restores the invariant that an order's `arrival_ms` + the simulated
clock determine its lifecycle position, not wall time.

## Paper env to 9/10 (`01af8c8`)

Four-item delivery in one commit:
- **vs_whale wiring** — `VsWhaleStats` in summary; `record_whale_fill_observed`;
  runtime ingests `dashboard_whale_events_path` filtered to session
  window before flush.
- **Fee model differentiation** — `paper_maker_rebate_coeff` (default 0.0)
  for maker rebates; `paper_taker_fee_coeff_override` for A/B testing
  fee scenarios. Fees can now be negative (rebates) on maker fills.
- **Scenario coverage** — `ScenarioEvent::Cancel` + `PaperMarketClose`
  variants; new fixtures `late_fill_after_cancel.json` +
  `paper_market_close_with_redeem.json`.
- **Calibration framework** — `polymarket-exec/scripts/suggest_paper_calibration.py`
  emits directional knob suggestions per documented heuristic table;
  runbook at `polymarket-exec/docs/2026-04-25-paper-env-calibration-runbook.md`.

## How to verify any of these

- `cargo test -p polymarket-exec` (153 lib + 14 integration after the
  taker fix).
- `polymarket-exec/scripts/test_unlawful_stack.sh` (12 focused scenarios
  covering paired entry, hedge survival, late fills, merge stall, end
  of window cleanup, etc.).
- For paper env behavior: shadow_live preset + replay loop per the
  calibration runbook.
- For audit acceptance checks: `polymarket-exec/docs/2026-04-25-handoff-audit.md`
  has the original audit grid; all partials are now closed at code level.

## Strategy alignment summary

Every fix maps to a documented requirement in `EXECUTION_ENGINE_HANDOFF.md`,
the audit, or the paper env design. No fix introduced new strategy
behavior — they either:
- Closed an audit acceptance check that was already specified, or
- Implemented a phase of the paper env design (which itself was
  brainstormed and committed to git as a spec before any code), or
- Fixed a bug surfaced by running real data through the new paper env.

The strategy's maker-first paired-entry pattern (`unlawful_shear` /
`btc_5m_mm`) is unchanged. The execution engine's venue-truth-first
fail-closed posture is unchanged. The fixes hardened both against
specific incident classes the handoff already named.
