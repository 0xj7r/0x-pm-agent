# Replay Engine Determinism Audit

Scope: `polymarket-exec/src/replay/` and the modules it depends on at runtime.

The replay determinism contract (see `polymarket-exec/src/replay/mod.rs`) calls
out three things: no wall-clock reads, no `HashMap` iteration in any path that
produces an output record, and `BTreeMap` everywhere. Findings below check
those invariants and adjacent risks.

## Severity scale

- HIGH: causes per-run divergence in journal output for the same input.
- MEDIUM: only matters when a HashMap key collides on rehash (unlikely on
  current input cardinality) but still violates the contract.
- LOW: documentation drift only, no observable effect.

## Findings

### F1 (HIGH, fixed) -- HashMap iteration leaks into floating-point sums in `RiskContext`

File: `polymarket-exec/src/replay/strategy_adapter.rs`, lines 1055-1077.

`runtime_state.open_orders()` (defined in
`polymarket-exec/src/runtime/state_store.rs:158`) returns
`HashMap<ClientOrderId, ManagedOrder>`. The adapter then iterates the values
of that HashMap and accumulates `f64` sums for `open_buy_notional_total_usd`,
`open_signed_notional_for_market_usd`, and `open_position_qty_for_instrument`.
Floating-point addition is not associative, so the iteration order produces
different sums across runs. Those sums feed `RiskContext` and decide
accept/reject; a flipped decision cascades through journal output (different
intent ids, different fills).

Fix: sort `open_orders` entries by `ClientOrderId` before iterating. This is
the same convention `quote_reconciler.rs` already uses (`sort_by(|left,
right| left.0.as_str().cmp(right.0.as_str()))` on line 375 / 576).

### F2 (HIGH, fixed) -- HashMap iteration in `open_convex_order_exposure` accumulator

File: `polymarket-exec/src/replay/strategy_adapter.rs`, line 645.

Same shape as F1: `self.managed_open_orders().values().filter(...)` is
summed into `open_convex_order_exposure.{yes,no}_{qty,notional_usd}`. Those
fields land on `StrategyInput` and feed strategy decisions. Non-deterministic.

Fix: sort entries by `ClientOrderId` before iterating, identical pattern to
F1.

### F3 (MEDIUM, fixed) -- `managed_open_orders` returns `HashMap`

File: `polymarket-exec/src/replay/strategy_adapter.rs`, lines 978-980.

The helper that everything goes through returns `HashMap`. Today F1 and F2
are the only two consumers that matter for journal output, but the helper
shape invites future regressions. Convert at the helper boundary so callers
can iterate freely.

Fix: change `managed_open_orders` to return `BTreeMap<ClientOrderId,
ManagedOrder>`. Only one HashMap consumer remains: `quote_reconciler::plan`,
which takes `&HashMap<...>`. Convert just for that call site.

### F4 (LOW, documented, not fixed)

The `quote_reconciler::plan` (`polymarket-exec/src/market_making/quote_reconciler.rs`)
takes `&HashMap<ClientOrderId, ManagedOrder>` as its open-orders argument and
uses `HashMap`/`HashSet` internally. All output paths (`plan.actions`,
`plan.notes`) are guarded by an explicit final sort
(`quote_reconciler.rs:611`) and explicit pre-sorts on the per-key vec
(`quote_reconciler.rs:374-376`) and on `unmatched_ids`
(`quote_reconciler.rs:572-576`). Internal HashSets are used only for
membership lookups, never iterated. Net: deterministic given a deterministic
input map. No fix required for replay determinism.

### F5 (informational) -- Parquet writer determinism

File: `polymarket-exec/src/replay/journal.rs:450`.

`ArrowWriter::try_new(file, schema, None)` uses default writer properties.
`parquet = "54"` writes a constant `created_by` string and does not embed
wall-clock timestamps in row groups by default. Output is byte-identical
across runs of the same binary for identical inputs. Confirmed by the new
`replay_determinism::byte_identical_journal` test.

### F6 (LOW, not fixed) -- Module doc-comment vs reality

File: `polymarket-exec/src/replay/mod.rs:11`, `strategy_adapter.rs:10-12`.

The module doc strings claim `BTreeMap everywhere`. F1-F3 contradicted that
on 2026-05-06 and have now been brought into compliance. No further change
needed; the doc is now accurate.

## Summary

| Severity | Count |
|----------|-------|
| HIGH     | 2     |
| MEDIUM   | 1     |
| LOW      | 2     |
| INFO     | 1     |

Three issues fixed in this branch; two LOW and one INFO documented only. No
deferred HIGH or MEDIUM findings. The new property test
(`polymarket-exec/tests/replay_determinism.rs::byte_identical_journal`) runs
the same fixture through `run_run` and `write_journal_parquet` twice and
asserts the resulting Parquet bytes are identical.
