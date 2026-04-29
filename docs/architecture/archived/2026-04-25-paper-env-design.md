# Paper Environment Design: Polymarket Execution Engine

**Date:** 2026-04-25
**Status:** Draft for review
**Scope:** `polymarket-exec` crate, `unlawful_shear` / `btc_5m_mm` strategy, pre-live validation use case

---

## 1. Gap Analysis

### What exists

The existing paper environment in `runner.rs:2614-2765` has:

- Level-by-level book crossing: bid/ask levels are iterated and the order crosses available depth at `intent.limit_price`. Buy orders match against asks at or below limit; sell orders match against bids at or above limit (`paper_fill_from_book_snapshot`).
- Queue-biased partial fills: a deterministic per-order `queue_bias` (FNV hash of `client_order_id`, mapped to [0.05, 1.0]) gates how much of top-level size is claimable for maker orders.
- Maker/taker classification: orders whose limit price crosses the top of book are labeled `Taker`; those that rest and later get traded through are labeled `Maker`.
- Fill-rate throttles: `paper_max_fills_per_order`, `paper_min_fill_notional_usd`, `paper_min_fill_interval_ms`, and one fill per book update.
- Merge/cancel lifecycle: cancel and merge commands are applied immediately in paper mode with no round-trip latency.

The existing scenario harness (`tests/btc_5m_mm_scenarios.rs`) covers 12 named integration paths but exercises the strategy-runtime contract directly, not the full runner including `paper_fill_from_book_snapshot`.

### The specific gaps

**Gap 1: Submit-to-ack and ack-to-fill latency are not modeled.**
`PaperExecutionAdapter::submit` returns an ack instantly at `req.submitted_at_ms` (line 375 in `execution_adapter.rs`). In paper mode the submit path in `execute_execution_adapter` (runner.rs:1584) calls `paper_fill_from_book_snapshot` in the same tick as the submit command. No simulated network round-trip exists. The real path has ~50-300 ms between submit and venue ack, and the book can move in that window. A maker order submitted at `t=0` and snapped at the same book state as `t=0` is filled against the same snapshot it was quoted against. This makes it impossible to observe post-only cross-rejection, repricing misses, or the fill-versus-cancel race.

**Gap 2: Post-only cross-rejection is not simulated.**
`submit_request_from_intent` sets `post_only: !execution_policy.paper_mode` (runner.rs:2110). Paper orders are never post-only. If the book crosses between the strategy quoting and the simulated ack, no rejection event is generated. Unlawful_shear is a maker-first strategy; the question "how often does our quote arrive post-crossing?" is unanswerable from paper output today.

**Gap 3: `paper_fill_ratio` is a heuristic, not calibrated to observed venue fill rates.**
The fill ratio combines five pressure terms (age, size, queue, staleness, limit premium) clamped to 0.20 for makers and 0.65 for crossings (runner.rs:2798-2799). These coefficients are arbitrary. There is no comparison against actual fill rates observed in `audit.jsonl` live runs. The model may be systematically optimistic (fills too readily at small queue positions) or pessimistic (caps too early on taker crosses).

**Gap 4: Late-fill-after-cancel is not exercised in paper mode.**
In live, a cancel ack arrives and then a fill can still come in for a brief window. The live path handles this correctly (`on_fill` can be called after `on_order_cancelled`). In paper, `cancel` is applied immediately in the same tick, and `paper_order_ctx` is removed synchronously (runner.rs:1791-1793). The paper environment cannot surface the paired-entry race where one leg cancels and then fills before the hedge is placed.

**Gap 5: Settlement at market close is not modeled.**
The paper environment has no concept of market expiry. BTC 5m markets resolve at a known UTC timestamp. Paired inventory held through close becomes a redeem operation worth {0, 1} per token, not a merge. Paper sessions that run across a close will accumulate unrealized marks forever with no forced resolution.

**Gap 6: No comparison report.**
A paper run produces journal events and dashboard metrics but no structured "report card": expected edge vs realized edge, maker fraction, queue-position estimate, slippage, or counterfactual vs unlawful_shear's observed fills on the same markets. Without this, paper results cannot be promoted or rejected for live with evidence.

**Gap 7: No historical replay.**
The audit logs (`audit.jsonl`) and market WS snapshots exist, but there is no tooling to feed a recorded session back through the engine deterministically and compare paper decisions to what actually happened live.

**Gap 8: Shadow-live mode does not exist.**
Running the engine against live market data feeds without submitting to the CLOB is not currently a named mode. The closest is `WHALE_PAIR_PAPER_MODE=true` with live feeds connected, but the venue reconciliation loop is also disabled in paper mode (`apply_sync_report` returns early on line 2259). A shadow-live run would connect market WS and spot WS but route all submits through the paper adapter, so strategy decisions track the live book exactly.

---

## 2. Modes of Operation

Four modes are proposed, ordered from cheapest to most realistic.

### Mode A: Deterministic Fixture (exists, extend it)

**Data source:** Handcrafted JSON fixtures in `tests/fixtures/btc_5m_mm/`.
**Fill model:** Scripted adapter outcomes (`AdapterPlan` in the scenario harness).
**Good for:** Regression testing, lifecycle invariants, paired-entry edge cases, late-fill-after-cancel races.
**Does not validate:** Fill rate realism, book dynamics, market-close resolution, latency.

This mode exists and works. The gap is that the 12 scenarios do not currently cover the late-fill-after-cancel or market-close resolution paths. Add those two fixture scenarios as part of Phase 1.

### Mode B: Synthetic Scenario (live engine, synthetic book)

**Data source:** Synthetic `BookState` sequences generated in a test harness, not from real data.
**Fill model:** The existing `paper_fill_from_book_snapshot` with upgraded conservative parameters (see Section 3).
**Good for:** Stress-testing the fill model under controlled book shapes, verifying the report card output, calibrating the latency model.
**Does not validate:** Real-world book dynamics, actual top-of-book sizes, real fill rates.

Activate with `WHALE_PAIR_PAPER_MODE=true` plus a synthetic book injector that drives `BookStore` from a JSON book sequence file. No code changes to `run_with_config`; the injector replaces `spawn_market_ws`.

### Mode C: Shadow-Live (existing feeds, paper fills)

**Data source:** Live `market_ws`, `spot_ws`, and optionally `user_ws` for whale observation. No live submit.
**Fill model:** `paper_fill_from_book_snapshot` with the upgraded model.
**Good for:** End-to-end validation of strategy decisions against the real book. Answers "what would we have done, and what would we have filled, if we had been live?"
**Does not validate:** Venue ack latency, post-only reject frequency under live load, wallet/auth correctness.

This is the primary new mode. It requires a named env: `WHALE_PAIR_EXEC_MODE=shadow_live` + `WHALE_PAIR_PAPER_MODE=true`. The runner already supports live market WS in paper mode; the only addition is connecting `user_ws` in paper mode (currently suppressed when `user_auth` is absent, but can be enabled for observation without submitting).

### Mode D: Historical Replay

**Data source:** Recorded `audit.jsonl` + book snapshot files from a past live session.
**Fill model:** Conservative `paper_fill_from_book_snapshot` replayed against the recorded book states.
**Good for:** Post-hoc counterfactual analysis. "Given the same book sequence from session X, what decisions and fills would the current engine version produce?" Enables direct comparison against what unlawful_shear filled.
**Does not validate:** Anything forward-looking; replay is inherently a look-back tool.

Requires a new binary or subcommand `replay` that reads a session directory and drives `Runtime` through recorded events. Design is in Section 4.

---

## 3. Realistic Fill Model Upgrade

The core of the upgrade is making the paper fill model conservative by default, and explicitly attributable in the report card.

### 3.1 Submit-to-ack latency

Add a `paper_submit_latency_ms: u64` config field (default: 150). In `execute_execution_adapter`, when processing `RuntimeCommand::Submit` in paper mode, record the submit timestamp in `PaperOrderContext::arrival_ms` (it already uses `intent.created_at_ms`). Then gate any fill attempt on:

```
observed_at_ms >= ctx.arrival_ms + paper_submit_latency_ms
```

This forces the book to have updated at least once after the simulated round-trip before a fill can be considered. The book update check (runner.rs:2635-2638) already gates on `last_fill_book_update_ms != book.last_update_unix_ms`, but that can be the same book snapshot that triggered the submit. The latency gate ensures the fill is evaluated against a book that arrived after the order would realistically have reached the venue.

### 3.2 Post-only cross-rejection

In paper mode, set `post_only = true` on the simulated intent (mirror live behavior). Before computing candidate levels, check:

```
if intent.post_only && crossing(book, intent) {
    // reject, emit on_order_rejected with reason "post-only-cross"
    return None;
}
```

Where `crossing` reuses the existing definition (runner.rs:2662-2666). Track the rejection count and reason in the paper report card. Probability matters: on Polymarket's BTC 5m markets, books are thin and fast. If the quote arrives during a 1c spread environment and we quote the touch, the rejection rate is high. The paper env should surface this, not hide it.

A calibrated rejection probability alternative: rather than a deterministic reject on any cross, apply a `paper_post_only_reject_probability` field (default: 0.85 when crossing, 0.0 when not crossing). This allows occasional taker fills when the book moves through the order, matching real venue behavior where post-only orders sometimes cross because the ack and the book update are not atomic.

### 3.3 Queue position estimate

Replace the opaque `queue_bias` (FNV hash) with a named `queue_position_estimate`:

- For a new maker order at a price level, the queue position is estimated as `level.size * paper_queue_depth_fraction` where `paper_queue_depth_fraction` defaults to 0.75 (we assume we are 75% back in the queue, i.e., conservative). This is the Moallemi/Yuan model: queue position determines fill probability under a Poisson flow assumption.
- Track `queue_depth_at_submit` in `PaperOrderContext`.
- The max claimable quantity at the level becomes `level.size - queue_depth_at_submit`. If `queue_depth_at_submit >= level.size`, the order cannot fill until the level refreshes.

This replaces the current level_ratio of `0.9 * order_ctx.queue_bias` (runner.rs:2707) with something that has a clearer physical interpretation.

### 3.4 Partial-fill probability based on queue position

Adopt a simplified version of the Cont-Larrard (2013) approach: a maker order fills if the quantity traded through its price level exceeds the queue position. Rather than modeling the full Poisson flow, use the observable `book.last_trade_price` event as a proxy trade event:

- When `maker_trade_through` is true (a trade occurred at or through the limit price), allow up to `(level.size - queue_depth_at_submit) * per_trade_fill_fraction` to fill, where `per_trade_fill_fraction` defaults to 0.3. This means a single trade event delivers 30% of the queue-adjusted position at most.
- Accumulated fills across multiple trade events can complete the order.
- This is conservative: real fill rates on thin prediction market books may be higher, but starting conservative and loosening with calibration is the correct direction.

### 3.5 Late-fill race after cancel

When `RuntimeCommand::Cancel` is processed in paper mode, instead of immediately calling `on_order_cancelled`, introduce a `paper_cancel_race_window_ms` (default: 500 ms) during which the order remains in a `CancelPending` local state. On the next tick within the window, check `paper_fill_from_book_snapshot` one more time. If a fill fires, apply the fill first, then the cancel is moot. If no fill fires, apply the cancel.

In practice, implement this by not removing the order from `paper_order_ctx` on cancel request. Tag the order as `cancel_requested_at_ms` in `PaperOrderContext`. On subsequent ticks, if `observed_at_ms - cancel_requested_at_ms < paper_cancel_race_window_ms`, attempt one more fill. After the window, apply the cancel.

This is the minimal mechanism needed to exercise the paired-entry race documented in the handoff (Incident #4) in paper mode.

### 3.6 Venue-reject simulation

Beyond post-only cross, the live venue rejects for min-size violations, duplicate order IDs, and insufficient allowance. These are deterministic in paper mode (all pass), but a `paper_venue_reject_rate` config field (default: 0.0) can inject random rejections to verify the engine's error handling. Keep this at 0.0 by default; it is a chaos-testing knob, not a calibration parameter.

### 3.7 Settlement at market close

Add a `paper_market_close_at_ms: Option<u64>` config field. When `observed_at_ms >= paper_market_close_at_ms`:

1. Cancel all open orders.
2. For each paired YES+NO position, emit a synthetic merge event at the paired quantity.
3. For stranded positions, emit a synthetic redeem event at the resolution price (0.0 for losers, 1.0 for winners, or 0.5 if unknown).
4. Log the final settlement in the report card.

The resolution price can be set via `paper_market_resolution_price: Option<f64>` (0.0 = down, 1.0 = up, 0.5 = unknown/split). This directly exercises the redeem path that exists in code but has never been paper-validated.

---

## 4. Replay Infrastructure

### 4.1 What to capture for replay

A replay session requires two inputs:

1. The `audit.jsonl` from a live run, which contains the strategy's decisions, order lifecycle events, and fill events at each point in time.
2. A book snapshot log: a JSONL file with timestamped `BookState` snapshots as the engine observed them during the session.

The book snapshot log does not currently exist. Add it as a side-channel write in `run_with_config`: when `audit_path` is set and a book snapshot is received, append a compact book record:

```json
{"t": 1745600123456, "asset": "...", "bids": [[0.82, 150.0], ...], "asks": [[0.84, 80.0], ...], "last_trade": 0.83}
```

This is lightweight: one line per market WS update per asset. With two assets and ~1 update/second, that is ~86K lines per 24-hour session, approximately 5-10 MB.

### 4.2 Replay binary

Add a `replay` subcommand (or `WHALE_PAIR_EXEC_MODE=replay`) that:

1. Loads the book snapshot log into an in-memory time-ordered sequence.
2. Instantiates `Runtime` with the same config as the original session.
3. Replays book snapshots in order, calling `runtime.on_book_state(...)` at the recorded timestamps.
4. Routes all `RuntimeCommand::Submit` through `paper_fill_from_book_snapshot` using the book state at the simulated timestamp (not `now_unix_ms()`).
5. Writes a new `replay_journal.jsonl` and paper report card to a replay output directory.

The critical detail is that `paper_fill_from_book_snapshot` currently calls `now_unix_ms()` internally (runner.rs:2781, 2791). For replay, the "now" must be the replay clock, not wall clock. This requires threading a `replay_clock_ms: u64` parameter through `paper_fill_ratio` and `paper_fill_from_book_snapshot`.

### 4.3 Counterfactual comparison

The replay output can be compared to the original `audit.jsonl` to answer: "Given the same book sequence, would we have submitted the same orders? Would we have filled at similar rates?" Differences surface either a strategy regression (decision changed) or a fill model issue (same decision, different fill outcome).

The comparison is a Python script, not Rust. It reads both JSONL files, aligns events by timestamp, and produces the report card (Section 5). Keep the comparison logic outside the Rust binary to avoid adding a dependency and to allow iteration without recompilation.

---

## 5. Paper Run Report Card

Every paper session should produce a `paper_report.jsonl` summary at the end of the run. The structure:

```json
{
  "session": {
    "run_id": "...",
    "mode": "shadow_live",
    "started_at_ms": 1745600000000,
    "ended_at_ms": 1745686400000,
    "market_close_at_ms": null,
    "resolution_price": null
  },
  "fills": {
    "total_count": 42,
    "maker_count": 38,
    "taker_count": 4,
    "maker_fraction": 0.905,
    "total_notional_usd": 310.5,
    "total_fees_usd": 1.24,
    "total_rebates_usd_estimate": 0.93
  },
  "edge": {
    "expected_edge_usd": 4.20,
    "realized_edge_usd": 3.15,
    "edge_capture_ratio": 0.75,
    "avg_slippage_bps": 8.3
  },
  "queue": {
    "avg_queue_depth_fraction_at_submit": 0.71,
    "post_only_reject_count": 7,
    "post_only_reject_fraction": 0.14,
    "late_fill_after_cancel_count": 2
  },
  "pairs": {
    "completed_pair_count": 18,
    "stranded_yes_count": 2,
    "stranded_no_count": 1,
    "merge_count": 15,
    "redeem_count": 0
  },
  "vs_whale": {
    "whale_fill_count_same_period": null,
    "our_fill_notional_usd": 310.5,
    "whale_fill_notional_usd": null,
    "timing_correlation": null
  }
}
```

**Expected edge** is computed as: for each submitted order, `(mid_at_submit - limit_price) * quantity` for buys. This is the edge the strategy believed it was capturing at quote time.

**Realized edge** is: for each fill, `(fill_price - cost_basis) * quantity` accounting for fees. For a maker buy at 0.83c with mid at 0.85c, expected edge is 2c x qty; if the fill came at 0.83c, realized edge matches. If a partial fill came at 0.84c (walking through queue), realized edge is lower.

**Slippage** is `|fill_price - limit_price| / limit_price * 10000` in bps, averaged across all fills. For post-only maker orders that fill exactly at limit, slippage should be zero. Any non-zero slippage means the fill crossed levels.

**vs_whale** fields are populated by the comparison script when a whale activity log is available (the existing `dashboard_whale_events_path` can supply this).

The report card is written by a new `PaperReportWriter` struct, which is notified at each fill, cancel, reject, and session-end event. It is separate from `JournalWriter` to keep concerns separated.

---

## 6. Phased Build Plan

### Phase 1: Close the fixture gaps (no new infrastructure)

- [ ] Add scenario fixture: late-fill-after-cancel. One leg submits, cancel is requested, fill arrives in the cancel window. Verify that inventory is updated correctly and the hedge/rescue path fires.
- [ ] Add scenario fixture: market-close resolution. Set `paper_market_close_at_ms` to a known timestamp, drive the runtime past it, verify merge and redeem events fire with correct accounting.
- [ ] Add `paper_market_close_at_ms` and `paper_market_resolution_price` to `AppConfig` and `ExecutionPolicy`.
- [ ] Implement the close-event emission in `run_runtime_loop`: when the clock passes `paper_market_close_at_ms`, emit cancel + merge/redeem commands and write a settlement entry to the journal.

These changes touch `runner.rs`, `config/mod.rs`, and two new fixture JSON files. No structural changes.

### Phase 2: Conservative fill model (upgrade `paper_fill_from_book_snapshot`)

Depends on Phase 1 being merged.

- [ ] Add `paper_submit_latency_ms` to `AppConfig` (default 150). Gate fill attempts on `observed_at_ms >= ctx.arrival_ms + paper_submit_latency_ms`.
- [ ] Add `paper_queue_depth_fraction` to `AppConfig` (default 0.75). Replace `level_ratio = 0.9 * queue_bias` with `(level.size - level.size * paper_queue_depth_fraction).max(0.0)` for non-crossing orders.
- [ ] Add `paper_post_only_reject_probability` to `AppConfig` (default 0.85 when crossing). In paper mode, set `post_only = true` on the intent context. Emit `on_order_rejected` with reason "post-only-cross-paper" when the crossing check fires.
- [ ] Add `paper_cancel_race_window_ms` to `AppConfig` (default 500). Implement the cancel-pending state in `PaperOrderContext`.
- [ ] Thread `clock_ms: u64` into `paper_fill_ratio` and `paper_fill_from_book_snapshot`, replacing internal `now_unix_ms()` calls. This is the prerequisite for replay accuracy.
- [ ] Add unit tests for each new behavior: latency gate, post-only rejection, queue depth cap, cancel race window.

These changes are confined to `runner.rs` and `config/mod.rs`.

### Phase 3: Report card output

Depends on Phase 2.

- [ ] Define `PaperFillRecord` and `PaperReportSummary` structs in a new `src/paper/report.rs`.
- [ ] Implement `PaperReportWriter` that accumulates records and writes the final JSON on drop or explicit flush.
- [ ] Wire `PaperReportWriter` into `run_with_config` alongside `JournalWriter`.
- [ ] Add `paper_report_path: Option<PathBuf>` to `AppConfig`.
- [ ] Emit expected-edge at submit time from `execute_execution_adapter` (book mid is already available at that point via `books.snapshot`).
- [ ] Emit realized-edge and slippage at fill time.

New file: `src/paper/report.rs`. Changes to `runner.rs` and `config/mod.rs`.

### Phase 4: Shadow-live mode

Depends on Phase 3.

- [ ] Add `WHALE_PAIR_EXEC_MODE=shadow_live` as a named mode in `run()`.
- [ ] In shadow-live mode, connect `market_ws`, `spot_ws`, and `user_ws` (read-only, for whale observation). Route submits through `PaperExecutionAdapter`. Do not call `apply_sync_report` for balance/position reconciliation.
- [ ] Log whale fills from `user_ws` events that arrive for non-local order IDs into the report card's `vs_whale` section.
- [ ] Write a shadow-live env file: `env/btc_5m_mm_shadowlive.env`.

Changes to `runner.rs` (new mode branch) and a new env file. No changes to `Runtime` or strategy code.

### Phase 5: Book snapshot capture and replay

Depends on Phase 4. This is the most involved phase.

- [ ] Add `book_snapshot_log_path: Option<PathBuf>` to `AppConfig`.
- [ ] In `run_runtime_loop`, after each book snapshot is received, append a compact record to the snapshot log.
- [ ] Add `WHALE_PAIR_EXEC_MODE=replay` that: loads a snapshot log, instantiates `Runtime`, replays snapshots using a replay clock, routes submits through the upgraded paper fill model, writes a replay journal and report card.
- [ ] Thread `clock_ms` through `paper_fill_from_book_snapshot` (Phase 2 prerequisite already handles this).
- [ ] Write `scripts/compare_replay.py`: reads `audit.jsonl` + `replay_journal.jsonl`, aligns by timestamp, produces a diff table of decisions and fill outcomes.

New file: `src/paper/replay.rs` or a new binary in `src/bin/replay.rs`. Changes to `runner.rs` and `config/mod.rs`.

### Dependency graph

```
Phase 1 (fixture gaps)
  -> Phase 2 (fill model)
       -> Phase 3 (report card)
            -> Phase 4 (shadow-live)
                 -> Phase 5 (replay)
```

Phases 1-2 are the highest leverage. Shadow-live (Phase 4) is the primary validation tool for pre-live readiness. Replay (Phase 5) is the forensic tool for post-hoc analysis.

---

## 7. Critical Details

### Error handling

Post-only rejection in paper mode must go through `runtime.on_order_rejected(...)` not a silent drop. The strategy must see the rejection and decide whether to reprice or suppress. If the paper model silently discards the order, the engine never exercises the reprice-or-suppress branch that live will face.

### State management

`PaperOrderContext` gains two new fields: `cancel_requested_at_ms: Option<u64>` and `queue_depth_at_submit: f64`. These must be initialized when the order is first seen and cleaned up on terminal states (fill, cancel, reject).

### Testing

The scenario harness (`btc_5m_mm_scenarios.rs`) should gain two helpers:
- `make_book_crossing(price)`: creates a `BookState` where the best ask/bid crosses the given limit price.
- `advance_clock(ms)`: simulates time passage by calling the fill model with a future timestamp.

These helpers are internal to the test file and do not require any API changes to `Runtime` or `BookState`.

### Performance

Book snapshot logging (Phase 5) writes to disk on every market update. Use the existing `JournalWriter` rotation pattern (write to a `BufWriter`, flush on each rotation boundary). With ~2 assets and ~1 update/second, the overhead is negligible.

### Security

Shadow-live mode connects `user_ws` with API credentials for whale observation. The paper adapter must never route a signed order to the CLOB even if auth is available. The `paper_mode` flag in `ExecutionPolicy` is the gate; shadow-live sets this flag.

---

## 8. References

**Queue position and fill probability models:**

- Moallemi, C.C., Yuan, K. (2016). "A Model for Queue Position Valuation in a Limit Order Book." [paper](https://moallemi.com/ciamac/papers/queue-value-2016.pdf) — foundational queue-position value model; the conservative default of 75% back in the queue is grounded in this framework.
- Cont, R., Kukanov, A., Stoikov, S. (2013). "The Price of a Price." Review of Financial Studies — derives fill probability as a function of queue depth and order flow. The `per_trade_fill_fraction` parameter in Section 3.4 is a practical approximation of their result.
- Huang, W., Rosenbaum, M., Saliba, P. (2019). "From Glosten-Milgrom to the Whole Limit Order Book." [arXiv:1902.10743](https://arxiv.org/abs/1902.10743) — useful for understanding how spread and depth relate to informed vs. uninformed flow on thin books.
- Xu, H. et al. (2024). "Fill Probabilities in a Limit Order Book with State-Dependent Dynamics." [arXiv:2403.02572](https://arxiv.org/pdf/2403.02572) — machine-learning-enhanced fill probability estimation; the survival-analysis framing is relevant if calibration data from `audit.jsonl` accumulates.

**Optimal market making:**

- Avellaneda, M., Stoikov, S. (2008). "High-frequency trading in a limit order book." [Cornell](https://people.orie.cornell.edu/sfs33/LimitOrderBook.pdf) — the reservation-price / spread framework motivates why expected edge at submit time is a meaningful pre-trade metric for the report card.

**Venue mechanics:**

- Polymarket documentation on maker/taker fee mechanics and CLOB V2 upgrade (pUSD, new exchange contracts, April 28 2026 cutover): [Polymarket Changelog](https://docs.polymarket.com/changelog) — the V2 fee structure and order semantics affect what "maker rebate" means in the report card's `total_rebates_usd_estimate` field.

---

## Appendix: Config fields added by this design

| Field | Default | Phase | Purpose |
|---|---|---|---|
| `paper_submit_latency_ms` | 150 | 2 | Min ms between submit and first fill attempt |
| `paper_queue_depth_fraction` | 0.75 | 2 | Assumed queue position as fraction of level size |
| `paper_post_only_reject_probability` | 0.85 | 2 | Probability of post-only rejection when crossing |
| `paper_cancel_race_window_ms` | 500 | 2 | Window for late fill after cancel request |
| `paper_market_close_at_ms` | None | 1 | UTC timestamp of market expiry |
| `paper_market_resolution_price` | None | 1 | 0.0/1.0/0.5 for redeem settlement |
| `paper_report_path` | None | 3 | Path for JSON report card output |
| `book_snapshot_log_path` | None | 5 | Path for book snapshot JSONL (replay prereq) |

All fields are optional with safe defaults. No existing paper-mode behavior changes unless the new fields are explicitly configured.
