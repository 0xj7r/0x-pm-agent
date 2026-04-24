# BTC 5m Strategy and Runtime Behavior Contract

Status: implementation-grade QA contract  
Date: 2026-04-24  
Primary target: `polymarket-exec`  
Scope: `unlawful_shear` signal and runtime behavior, plus shared BTC 5m execution semantics that should stay true across strategy, runtime, reconcile, and paper calibration.

## 1. Purpose

This document is the single behavior-contract thread for the BTC 5m sleeve.

Its job is to define:

- what the strategy and runtime are expected to do
- which semantics are hard invariants vs configurable policy
- which tests must prove those behaviors
- which runtime metrics and structured logs must make the behavior explainable after the fact

This is not a fresh architecture rewrite.

It is a contract over the existing implementation surfaces:

- `polymarket-exec/src/signals/unlawful_gate.rs`
- `polymarket-exec/src/strategy.rs`
- `polymarket-exec/src/runtime/mod.rs`
- `polymarket-exec/src/runtime/reconcile.rs`
- `polymarket-exec/src/runtime/order_store.rs`
- `polymarket-exec/src/inventory.rs`
- `polymarket-exec/src/mm/quote_reconciler.rs`
- `polymarket-exec/tests/btc_5m_mm_scenarios.rs`

This doc should be used as the acceptance contract for future fixes in this thread. If behavior changes, this doc should change with it and the test suite should tighten accordingly.

## 2. Definitions

### 2.1 Market phases

The runtime-visible market phases are:

- `Standby`
  - observe only
  - no new risk
- `Entry`
  - fresh paired-leg formation is allowed
- `Manage`
  - inventory shaping and pair completion are allowed
  - opening a fresh market from zero inventory is no longer the default
- `Cleanup`
  - no new non-reduce-only risk
  - pair completion, salvage, merge, and controlled exit are allowed
- `Flatten`
  - cancel non-reduce-only working orders
  - only close or settle residual inventory

These phases are already reflected in `signals/unlawful_gate.rs`, mapped in `runtime/mod.rs`, and consumed in `strategy.rs`.

### 2.2 Order classes

All order intents must fall into one of these execution meanings:

- `new-risk`
  - increases gross exposure or opens a previously flat market
- `manage`
  - rebalances an already open market without increasing unbounded risk
- `reduce-only`
  - cannot increase net market exposure
  - allowed to survive into cleanup and flatten

If a new intent cannot be classified cleanly, it should be treated as `new-risk` until proven otherwise.

### 2.3 Market end states

We need to distinguish:

- `window closed`
  - the market has reached its configured end time
  - no fresh risk is allowed
- `settlement-informed`
  - the market context has a `final_price`, `price_to_beat`, or winner inference good enough to anchor an intelligent close
- `closed without settlement context`
  - the market is over, but we do not have trustworthy winner/final-price metadata yet

That distinction matters because strategy behavior should differ between intelligent settlement closing and generic fallback cleanup.

## 3. Canonical Invariants

These are hard invariants. Changing them requires an explicit doc update plus test updates.

1. Stale BTC feed means no new risk.
2. Stale or incomplete paired books mean no new risk.
3. Cleanup backlog exceeded means the market must move toward `Cleanup` or `Flatten`, not back toward `Entry`.
4. Inventory hard-cap exceeded means no new risk and forced cleanup posture.
5. Market closed means `Flatten`, never `Entry` or `Manage`.
6. Transition into `Cleanup` or `Flatten` must request cancellation for all non-reduce-only working orders in that market.
7. `reduce-only` orders may remain through cleanup or flatten if they are still valid closing intents.
8. A fresh flat market may only be opened during a valid `Entry` window with fresh signals and in-band pricing.
9. `Manage` may reshape existing inventory, but it must not silently behave like a fresh-entry phase.
10. If runtime state is uncertain after restart or reconcile, the engine must fail closed and classify open orders as needing reconcile rather than assuming safety.

## 4. Runtime Evaluation Order

This is the mandatory order for strategy admission and phase selection.

1. Market context present?
2. Market start and end time present?
3. BTC signal fresh?
4. Both books fresh, two-sided, and positive-size?
5. Session bucket classified?
6. BTC regime thresholds satisfied?
7. Cheap and expensive legs derived from live asks?
8. Hard or preferred pricing band satisfied?
9. Phase determined from elapsed time?
10. Merge-stall override applied?
11. BTC shock override applied?
12. Inventory imbalance and cleanup-backlog overrides applied?
13. Result mapped into strategy-visible mode and runtime enforcement?

This order should stay aligned with `evaluate_unlawful_mode` and `build_unlawful_signal_snapshot`.

## 5. Signals Contract

## 5.1 Required signal inputs

The strategy and runtime must not infer behavior from partial context if these inputs are intended to drive a decision:

- market start time
- market end time
- current UTC session bucket
- paired book best bid and ask for both legs
- paired book freshness
- BTC last price
- BTC realized vol over 5m and 15m
- BTC trade count over 5m and 15m
- BTC short returns over 30s and 60s
- first fill time for the market
- first merge time for the market
- local inventory state for the market
- cleanup backlog state for the market

If any required input is absent for a behavior that depends on it, the runtime should degrade to the safer phase.

## 5.2 Signal freshness semantics

The freshness contract is:

- if books are stale or one side is missing:
  - no new entry
  - no new rebalance buys
  - `Cleanup` if inventory exists
  - otherwise `Standby`
- if BTC regime data is stale:
  - no new entry
  - no new aggression increase
  - `Cleanup` if inventory exists
  - otherwise `Standby`

This is a safety rule, not a tuning parameter.

## 5.3 Session semantics

Session classification is a prior, not a hard switch.

- preferred hours should lower the confirmation bar
- neutral hours should require stronger BTC confirmation
- opportunistic hours should require the strongest regime plus structure confirmation

The runtime must log blocked opportunities outside preferred hours so we can distinguish:

- a strategy threshold issue
- a lack of real opportunity
- stale or missing inputs

## 5.4 Shock semantics

BTC shock is a risk overlay, not the primary entry signal.

- soft shock:
  - no fresh core-entry
  - allow hedge repair and pair cleanup
- hard shock:
  - force `Cleanup`
  - cancel non-reduce-only working orders on transition

## 6. Overlap Contract

This section defines what is allowed when markets overlap in time or when inventory and open orders already exist.

## 6.1 Cross-market overlap

The runtime may observe consecutive or overlapping BTC 5m windows, but it must enforce risk at the market and portfolio level.

Required behavior:

- a new market may only open if:
  - global inventory and cash constraints still pass
  - per-market net notional bounds still pass
  - open-order count bounds still pass
  - existing cleanup pressure in adjacent markets does not already exceed the configured backlog cap

Not allowed:

- opening a new market just because the gate is open while another market is already in forced cleanup and consuming the same risk budget
- treating sequential windows as independent if shared inventory pressure already pushed the runtime into a fail-closed posture

## 6.2 Same-market overlap between working orders and new intents

The runtime must reconcile desired intents against existing working orders before adding more churn.

Required behavior:

- if an existing working order already matches the desired quote, keep it
- if the desired quote changed materially, request cancel and replace subject to cancel-rate limits
- if the market enters cleanup or flatten, cancel all non-reduce-only working orders
- if a working order is uncertain after restart, mark it `NeedsReconcile` and count it toward cleanup pressure

This is primarily enforced through `mm/quote_reconciler.rs` and runtime cancel semantics.

## 6.3 Same-market overlap between inventory states

If the market already has inventory:

- `Entry` should effectively behave like `Manage`
- strategy logic may add hedge or rebalance orders
- strategy logic must not behave as if it is still forming a fresh market from zero unless that is explicitly allowed and covered by tests

This is already visible in `evaluate_unlawful_mode`, where inventory present during the entry window maps to `Manage`.

## 7. Phase Semantics Contract

## 7.1 `Standby`

Expected behavior:

- no new orders that can add risk
- passive observation and logging only
- no fake “keep warm” quoting

Proof:

- unit test on gate evaluation
- scenario where stale data with no inventory produces zero submits

## 7.2 `Entry`

Expected behavior:

- fresh market formation is allowed only in the configured early window
- entry requires:
  - fresh BTC signal
  - fresh two-sided books
  - in-band geometry
  - no cleanup-backlog hard block
  - no inventory hard-cap violation
- allowed actions:
  - `core-entry`
  - `hedge-probe`
  - `early-probe`

Not allowed:

- opening a flat market outside the entry window
- opening a flat market while books or BTC signals are stale
- opening a flat market while the market is already effectively in cleanup or flatten

Proof:

- `unlawful_entry_window.json`
- a unit test for late-entry rejection
- a unit test for pre-start rejection

## 7.3 `Manage`

Expected behavior:

- strategy may rebalance or complete an already-open market
- new orders in this phase must be explainable as inventory management, hedge repair, or bounded rebalancing
- manage must not silently re-open a dead market from zero

Allowed actions:

- `add-hedge`
- `rebalance-core`
- `rebalance-flip`

Not allowed:

- unconstrained fresh-risk accumulation
- continuing to behave like entry once the early window has passed and the market is still flat

Proof:

- unit tests over mode-to-action permissions
- scenario where inventory is present and the market continues to manage rather than re-enter

## 7.4 `Cleanup`

Expected behavior:

- no new non-reduce-only buys
- pair completion, salvage, merge, and bounded close actions are allowed
- transition into cleanup must request cancellation for non-reduce-only working orders
- if merge progress is stalled beyond the configured lag while inventory exists, cleanup must be forced

Trigger classes:

- cleanup time bucket reached
- cleanup backlog exceeded
- merge stalled
- hard volatility shock
- stale signal or stale books with inventory present
- inventory hard-cap breach

Proof:

- `unlawful_merge_stall_cleanup.json`
- `one_sided_fill_reversal.json`
- `unlawful_late_window_cleanup_only.json`
- unit test for cleanup-triggered cancel of non-reduce-only orders

## 7.5 `Flatten`

Expected behavior:

- this is the terminal market phase
- cancel all non-reduce-only working orders
- only allow closing, settlement-anchored exit, merge, or redeem behavior
- never allow new market formation

Trigger classes:

- close window reached
- market end reached
- market explicitly closed

Proof:

- `end_of_window_cleanup.json`
- `unlawful_regime_closed.json`
- unit test that `Flatten` never launches entry or manage actions

## 8. Cancellation Contract

## 8.1 Cancellation triggers

Cancellation must be requested when any of the following becomes true:

- desired quote no longer matches the existing working order
- market transitions into `Cleanup` or `Flatten`
- reconcile determines the local order is unsafe or outdated
- quote replacement is needed and the adapter/runtime uses cancel-and-resubmit

## 8.2 What must be cancelled

Must cancel:

- `PendingSubmit`, `Submitted`, or `Working` orders that are not `reduce_only` when entering cleanup or flatten

Must not be blanket-cancelled solely because of cleanup:

- valid `reduce_only` close intents that remain consistent with the current closing posture

## 8.3 Cancellation rate and churn limits

Quote churn is a real runtime concern, so cancellation is constrained by the quote reconciler.

Required behavior:

- desired-quote drift should not create infinite cancel/replace loops
- if cancel-rate caps are reached, the system must record that explicitly
- capped cancellation is acceptable only if the remaining orders do not violate the phase safety contract

If cancel-rate capping would leave unsafe new-risk orders live during cleanup or flatten, that is a bug.

## 8.4 Cancellation proof

Required tests:

- unit tests in `mm/quote_reconciler.rs` for desired-changed and replace-cleanup behavior
- runtime scenario proving non-reduce-only cancellation on phase transition
- runtime scenario proving reduce-only orders survive cleanup when still valid

Required metrics and logs:

- cancel requests by reason
- cancel acknowledgements by reason
- cancel rate-cap hits
- count of non-reduce-only orders still live while market mode is cleanup or flatten

The last metric should normally stay at zero.

## 9. Settlement vs Closed-Market Contract

## 9.1 Settlement-informed closing

If the market is near end or past end and trustworthy market context exists with `final_price` or enough metadata to infer the winning instrument:

- strategy should prefer winner-aware close behavior
- strategy notes must include the settlement anchor:
  - final price
  - price to beat
  - inferred winner if known

This is already partially implemented in `strategy.rs`, where end-window logic attempts winner inference and emits notes like:

- settlement anchor with `final` and `beat`
- winner-leg inference

## 9.2 Closed without settlement context

If the market is over but the runtime lacks reliable settlement metadata:

- the market still must move to `Flatten`
- strategy must not wait indefinitely for perfect context
- fallback cleanup should execute using a bounded close fraction
- the log must say settlement was skipped and why

This is the distinction between:

- `closed and informed`
- `closed and uninformed`

Both require no new risk, but the close plan differs.

## 9.3 Closed-market behavior

Once a market is closed:

- mode must be `Flatten`
- no `Entry`
- no `Manage`
- non-reduce-only working orders must be cancelled
- residual inventory may still be closed, merged, or redeemed

If the engine sees a closed market and still emits fresh entry intents, that is a contract violation.

## 9.4 Settlement proof

Required tests:

- scenario where settlement metadata exists and winner-aware close intents are emitted
- scenario where market is closed but settlement metadata is missing and fallback cleanup is used
- scenario where market context is missing entirely and the market still fails closed

Current fixtures already cover part of this:

- `end_of_window_cleanup.json`
- `unlawful_regime_closed.json`

At least one additional settlement-aware fixture should be added if winner inference is going to remain a first-class behavior.

## 10. Pricing Band Contract

## 10.1 Hard entry band

New-risk entry is only allowed if the live geometry is inside the hard band:

- `cheap_ask <= 0.47`
- `expensive_ask >= 0.56`
- `expensive_ask <= 0.84`
- `price_gap >= 0.22`

Outside this band:

- no fresh flat-market entry
- manage or cleanup behavior may still be allowed if inventory already exists, but only under explicit safety rules

## 10.2 Preferred band

Preferred geometry means:

- `cheap_ask <= 0.40`
- `expensive_ask >= 0.62`
- `expensive_ask <= 0.78`
- `price_gap >= 0.35`

In preferred geometry:

- full clip scaling is allowed

If only the hard band passes:

- clip scaling should be reduced

## 10.3 Price-band semantics

Pricing bands are an admission and aggression tool, not a sole liquidation tool.

That means:

- bands decide whether new risk is allowed
- bands influence clip scale in `Entry` and `Manage`
- bands do not override hard safety states like stale data, cleanup backlog, or flatten

## 10.4 Price-band proof

Required tests:

- unit tests for hard-band rejection
- unit tests for preferred-band promotion
- unit tests proving stale or incomplete books still suppress entry even if nominal prices are in band
- runtime scenario where bad geometry blocks entry while inventory-free

Required metrics:

- counts of blocked windows by pricing reason
- counts of preferred-band vs hard-band-only windows
- clip-scale histogram by market phase

## 11. Inventory and Bounds Contract

## 11.1 Inventory accounting

Inventory is the runtime truth source for:

- free cash
- reserved cash
- per-instrument quantity
- net exposure by market
- realized PnL

No strategy decision should assume inventory was updated unless the runtime has acknowledged the reservation, fill, cancel-release, merge, or settlement effect.

## 11.2 Reservation semantics

Before submit:

- runtime must reserve inventory for the order

On rejection or cancellation:

- reservation must be released

On fill:

- inventory must move from reservation to real position and realized cost basis

On merge:

- paired inventory must be reduced and recycled consistently

## 11.3 Hard bounds

The runtime must enforce at least these bounds:

- max order notional
- max gross notional
- max net notional per market
- max position quantity per instrument
- min free cash
- max open orders total
- max open orders per market

If a bound is hit:

- no new risk
- the rejection reason must be explicit
- if the breach is market-specific and inventory already exists, the phase should bias toward cleanup

## 11.4 Inventory proof

Required tests:

- existing inventory unit tests for reserve, fill, cancel-release, and realized PnL
- unit tests for market net-exposure cap forcing cleanup
- scenario where open-order backlog or reconcile pressure pushes the market into cleanup
- scenario where restart recovery preserves open-order uncertainty instead of double-counting inventory

Current scenarios that already touch this surface:

- `reconnect_partial_fills.json`
- `replay_reconcile_merge_recovery.json`
- `uncertain_submit.json`

## 12. Behavior-to-Test Matrix

This is the minimum contract matrix. Every row needs a proving test, not just a paragraph.

| Behavior | Required proof | Current or target artifact |
| --- | --- | --- |
| preferred-hour early entry allowed | runtime scenario | `unlawful_entry_window.json` |
| late entry rejected | runtime scenario or focused gate test | add if absent |
| pre-start entry rejected | focused gate test | add if absent |
| stale book blocks new risk | runtime scenario | `stale_book.json` |
| stale BTC blocks new risk | focused gate test | add if absent |
| merge stall forces cleanup | runtime scenario | `unlawful_merge_stall_cleanup.json` |
| one-sided fill with reversal pressure moves to cleanup | runtime scenario | `one_sided_fill_reversal.json` |
| end-window cleanup only, no new risk | runtime scenario | `unlawful_late_window_cleanup_only.json` |
| close window forces flatten | runtime scenario | `end_of_window_cleanup.json` |
| closed market forces flatten | runtime scenario | `unlawful_regime_closed.json` |
| uncertain submit is reconciled safely | runtime scenario | `uncertain_submit.json` |
| restart preserves recovery state | runtime scenario | `reconnect_partial_fills.json`, `replay_reconcile_merge_recovery.json` |
| non-reduce-only orders cancelled on cleanup transition | unit + runtime scenario | add explicit assertion if absent |
| reduce-only orders survive cleanup if still valid | unit + runtime scenario | add if absent |
| winner-aware settlement close works when metadata exists | runtime scenario | add if absent |
| fallback cleanup works when metadata is missing | runtime scenario | partly `end_of_window_cleanup.json`, tighten if needed |
| inventory hard-cap forces cleanup | focused gate/runtime test | add if absent |
| price-band hard reject blocks flat entry | focused gate test | add if absent |
| preferred band increases clip scale | focused gate test | add if absent |

## 12.1 Current coverage audit

From the repo as it exists now, the unlawful contract has these concrete proofs already in place:

| Behavior | Current proof artifact | Status |
| --- | --- | --- |
| preferred-hour early entry allowed | `tests/fixtures/btc_5m_mm/unlawful_entry_window.json` plus `unlawful_entry_window_valid_core_entry_and_hedge_probe` | covered |
| stale book blocks new risk | `tests/fixtures/btc_5m_mm/stale_book.json` plus `stale_book_risk_gate` | covered |
| merge stall forces cleanup-only posture | `tests/fixtures/btc_5m_mm/unlawful_merge_stall_cleanup.json` plus `unlawful_merge_stall_drives_cleanup_only_actions` | covered |
| late-window cleanup only, no fresh buys | `tests/fixtures/btc_5m_mm/unlawful_late_window_cleanup_only.json` plus `unlawful_late_window_only_reduce_only_cleanup` | covered |
| closed market forces fail-closed posture | `tests/fixtures/btc_5m_mm/unlawful_regime_closed.json` plus `unlawful_regime_closed_no_buy_intents` | covered |
| end-window cleanup / flatten path | `tests/fixtures/btc_5m_mm/end_of_window_cleanup.json` plus `end_of_window_cleanup` | covered |
| one-sided fill reversal / cleanup path | `tests/fixtures/btc_5m_mm/one_sided_fill_reversal.json` | covered |
| uncertain submit recovery | `tests/fixtures/btc_5m_mm/uncertain_submit.json` | covered |
| reconnect / replay recovery | `tests/fixtures/btc_5m_mm/reconnect_partial_fills.json`, `tests/fixtures/btc_5m_mm/replay_reconcile_merge_recovery.json` | covered |
| focused unlawful gate state transitions | `polymarket-exec/src/signals/unlawful_gate.rs` scenarios 1-9 and invariants | covered |
| unlawful eval note completeness | `unlawful_shear_signal_reason_and_aggression_logged_in_decision_notes` | covered |

These are still not proven strongly enough from the current repo state:

| Behavior | Why current proof is insufficient | Required next proof |
| --- | --- | --- |
| reduce-only survival through cleanup / flatten | docs require it, but no dedicated runtime fixture asserts the survivor set explicitly | add runtime fixture |
| non-reduce-only cancellation on cleanup transition | behavior exists in `runtime/mod.rs`, but current scenarios do not assert exact cancellation membership on transition | add runtime fixture |
| stale BTC feed blocks new risk end-to-end | covered in gate unit tests only, not in multi-step runtime scenario form | add runtime fixture |
| inventory hard-cap forcing cleanup | covered implicitly by code path, not by fixture or focused runtime assertion | add focused test or runtime fixture |
| settlement-aware close vs fallback close | docs require the distinction, but current fixtures do not prove both branches separately | add two runtime fixtures |
| hard-band rejection and preferred-band clip scaling | partially implied by gate math, not locked down with narrow unlawful behavior assertions | add focused unit tests |
| persisted unlawful snapshot semantics | runtime now persists signal snapshots, but there is no explicit contract test for cadence, field completeness, or mode-change persistence | add persistence-focused runtime/unit coverage |

## 13. Metrics and Structured Logging Contract

Tests prove correctness in controlled cases. Metrics and logs prove explainability in paper runs.

## 13.1 Required per-eval fields

Every material market evaluation should emit enough structured detail to answer:

- why did we enter?
- why did we not enter?
- why did we switch to cleanup?
- why are we still carrying inventory?

Minimum fields:

- market id
- cheap instrument id
- expensive instrument id
- elapsed / remaining / progress
- session bucket
- BTC last
- BTC vol 5m and 15m
- BTC trade count 5m and 15m
- BTC return 30s and 60s
- BTC signal age
- cheap bid and ask
- expensive bid and ask
- price gap
- hedge ratio
- book age
- both-sides-present
- activity 10s / 30s / 60s
- activity age
- first fill time
- first merge time
- mode
- aggression
- signal clip scale
- phase clip scale
- buy clip scale
- market has inventory
- gate reasons

This aligns with the current unlawful eval notes in `strategy.rs` and persisted signal snapshots in `runtime/mod.rs`.

## 13.2 Required counters and gauges

Minimum required runtime metrics:

- markets evaluated
- markets admitted
- markets suppressed, by reason bucket
- transitions into each phase
- cleanup transitions by trigger class
- flatten transitions by trigger class
- submit attempts by action class
- submit rejects by reason
- cancel requests by reason
- cancel acknowledgements by reason
- live non-reduce-only orders while in cleanup or flatten
- inventory imbalance breaches
- cleanup backlog breaches
- merge-stall detections
- settlement-informed closes
- fallback cleanup closes

## 13.3 Paper calibration metrics

For whale-vs-us review, the minimum comparison surface is:

- whale participated or not
- we participated or not
- whale first-entry lag
- our first-entry lag
- whale merge present or not
- our merge present or not
- whale clip intensity proxy
- our order, fill, and cancel intensity
- mismatch bucket:
  - config
  - BTC gate
  - geometry gate
  - timing
  - cleanup / salvage churn
  - control-plane / runtime

The review unit is the window, not an aggregate vanity metric.

## 14. Done Condition for This Contract

This contract is in effect when all of the following are true:

1. Every behavior row in the matrix is covered by a focused test or scenario.
2. Cleanup and flatten cancellation semantics are explicitly tested, including reduce-only survival.
3. Settlement-informed close vs closed-without-context close are explicitly separated in tests and logs.
4. Price-band and freshness decisions are explainable from structured output alone.
5. Paper calibration reports can classify why we differed from the whale window by window.
6. Future fixes in this thread are rejected if they change behavior without changing either:
   - the contract doc
   - the proving tests
   - or both

## 15. Immediate Follow-on Test Gaps

Based on the current repo state, the most important gaps to close next are:

1. `unlawful_cleanup_transition_cancels_new_risk.json`
   - market starts in `Manage`
   - one non-reduce-only working order and one valid reduce-only order exist
   - signal transitions to `Cleanup`
   - assert:
     - non-reduce-only order receives cancel request
     - reduce-only order is not cancelled just because of cleanup
     - resulting mode-specific notes remain fail-closed
2. `unlawful_reduce_only_survives_flatten.json`
   - market is at or past close
   - only reduce-only close intents remain valid
   - assert:
     - no fresh submit of new-risk intents
     - reduce-only close intent survives transition into `Flatten`
3. `unlawful_stale_btc_signal.json`
   - books are fresh and geometry is valid
   - BTC signal age exceeds configured freshness
   - assert:
     - no new submit
     - mode is `Standby` when flat, `Cleanup` when inventory exists
4. `unlawful_inventory_hard_cap_cleanup.json`
   - market inventory exceeds per-market hard cap
   - assert:
     - `clip_scale=0`
     - mode forced to `Cleanup` or `Flatten`
     - no fresh new-risk actions survive
5. `unlawful_settlement_informed_close.json`
   - final-price / winner metadata exists
   - assert:
     - winner-aware close notes are emitted
     - closing behavior differs from generic fallback cleanup
6. `unlawful_closed_without_context_fallback.json`
   - market is over but settlement metadata is missing
   - assert:
     - fail-closed posture still holds
     - fallback cleanup notes are emitted explicitly
7. focused unit tests for:
   - hard-band rejection with flat inventory
   - preferred-band promotion to full clip scale
   - persisted unlawful signal snapshot cadence and field completeness
These are the highest-priority remaining gaps because they separate:

- documented safety semantics from merely implied code behavior
- gate-only proof from runtime proof
- explainable paper behavior from “the code probably did the right thing”
