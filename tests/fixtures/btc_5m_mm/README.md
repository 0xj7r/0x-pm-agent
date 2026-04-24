# BTC 5m unlawful fixture manifest

This directory is the runtime-scenario surface for the unlawful BTC 5m sleeve.

Use it as the fixture contract for:

- `polymarket-exec/tests/btc_5m_mm_scenarios.rs`
- `docs/architecture/2026-04-24-btc-5m-behavior-contract.md`

## Current fixture set

Existing runtime fixtures:

- `end_of_window_cleanup.json`
- `one_sided_fill_reversal.json`
- `reconnect_partial_fills.json`
- `replay_reconcile_merge_recovery.json`
- `stale_book.json`
- `uncertain_submit.json`
- `unlawful_entry_window.json`
- `unlawful_late_window_cleanup_only.json`
- `unlawful_merge_stall_cleanup.json`
- `unlawful_regime_closed.json`

These already cover:

- basic unlawful entry
- stale-book suppression
- merge-stall cleanup
- late-window cleanup-only behavior
- closed-market fail-closed behavior
- restart/reconcile recovery paths

## Highest-priority missing fixtures

The next fixture additions should be:

1. `unlawful_cleanup_transition_cancels_new_risk.json`
2. `unlawful_reduce_only_survives_flatten.json`
3. `unlawful_stale_btc_signal.json`
4. `unlawful_inventory_hard_cap_cleanup.json`
5. `unlawful_settlement_informed_close.json`
6. `unlawful_closed_without_context_fallback.json`

## Required assertion shape for new unlawful fixtures

Every new unlawful fixture should prove all of the following that apply:

- resulting runtime mode or posture
- whether fresh submits happened or did not happen
- whether cleanup/flatten produced cancel requests
- which orders survived if reduce-only behavior is the subject
- whether checkpoint/runtime state remained bounded
- whether the journal contains the expected unlawful behavior note or close-path note

For cancellation-focused fixtures, explicitly assert:

- which client order ids were cancelled
- which client order ids were preserved
- whether preserved orders were `reduce_only`

For settlement-focused fixtures, explicitly assert:

- settlement-informed close notes vs fallback cleanup notes
- absence of fresh entry/manage intents after market close

## Naming rule

Fixture names should describe the behavior under test, not the implementation detail.

Good:

- `unlawful_stale_btc_signal.json`
- `unlawful_reduce_only_survives_flatten.json`

Weak:

- `scenario_10.json`
- `cleanup_edge_case_b.json`

## Contract rule

If a behavior is listed as required in the unlawful behavior contract and is not
proven by one of the fixtures in this directory or a focused unit test, it is
still an open QA gap.
