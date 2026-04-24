**Purpose**
This defines the testing framework for the `unlawful_shear` BTC 5m sleeve so we stop discovering basic strategy, config, and runtime bugs from live paper runs.

The rule is simple:
- BDD defines the behavior we want.
- TDD locks down the implementation details that make that behavior possible.
- Paper runtime is the integration truth source before any tiny-live deployment.

**Why**
The recent off-hour override bug is exactly why this exists:
- shell env was correct
- runtime behavior was wrong
- root cause was a silent Rust config-resolution bug

That should have been caught by:
- a BDD scenario: `off-hour override allows entry`
- a TDD unit test: `from_env respects ALLOW_EXTREME_OFFHOUR_OVERRIDE=true`

**Test Pyramid**
Use four layers.

1. `Behavior specs`
- Goal: prove the sleeve behaves correctly at the decision-policy layer.
- Primary file: `whale-pair-exec/src/signals/unlawful_gate.rs`
- Shape: scenario-style tests with explicit market state, BTC regime, and expected mode/aggression.

2. `Implementation tests`
- Goal: prove the config, state, and plumbing behind the behavior are correct.
- Primary files:
  - `whale-pair-exec/src/strategy.rs`
  - `whale-pair-exec/src/runtime/mod.rs`
  - `whale-pair-exec/src/runtime/order_store.rs`

3. `Runtime scenarios`
- Goal: prove the runtime behaves correctly across multi-step flows.
- Primary file: `whale-pair-exec/tests/btc_5m_mm_scenarios.rs`
- Fixture manifest: `tests/fixtures/btc_5m_mm/README.md`
- Shape: fixture-driven scenarios covering submits, fills, cancels, restart, reconcile, flatten, cleanup.

4. `Paper calibration`
- Goal: compare live paper behavior against the whale and catch mismatch classes.
- Primary artifacts:
  - `scripts/export_unlawful_whale_vs_us.py`
  - paper journals + order stores
  - `docs/research/unlawful-whale-vs-us-calibration.md`
- Rule: calibration must always be time-aligned to the whale's actual activity windows.

**BDD Coverage Model**
Each behavioral rule must be written as a named scenario.

Minimum required scenario groups:

1. `Admission`
- preferred-hour entry
- neutral-hour suppression
- opportunistic-hour suppression
- opportunistic override admission
- late-entry rejection
- pre-start rejection

2. `Safety`
- stale BTC feed
- stale/incomplete book
- cleanup backlog exceeded
- inventory hard-cap exceeded
- missing market context

3. `Inventory lifecycle`
- no inventory -> entry
- inventory present -> manage
- merge stall -> cleanup
- close window -> flatten
- hard shock with inventory -> cleanup

4. `Quote management`
- desired quote kept
- no longer desired -> cancel requested -> cancelled
- reduce-only survives cleanup transition
- non-reduce-only is pulled on cleanup/flatten

Immediate next unlawful runtime fixtures should be:

- `unlawful_cleanup_transition_cancels_new_risk.json`
- `unlawful_reduce_only_survives_flatten.json`
- `unlawful_stale_btc_signal.json`
- `unlawful_inventory_hard_cap_cleanup.json`
- `unlawful_settlement_informed_close.json`
- `unlawful_closed_without_context_fallback.json`

**TDD Coverage Model**
Every bug fix must add or tighten one of these:

1. `Config resolution`
- env parsing
- profile/env precedence
- invariant normalization
- defaulting behavior

2. `Runtime state machine`
- status transitions
- reservation accounting
- restart recovery
- reconcile transitions

3. `Strategy logging and observability`
- `unlawful eval` note completeness
- gate reason preservation
- deterministic quote ids

4. `Control plane`
- rolling `prev/current/next` slate export
- launcher refresh safety
- no shared temp-file collisions

**Required Invariants**
These should have hard tests and should almost never be changed casually.

- stale BTC feed -> no new risk
- stale or incomplete book -> no new risk
- cleanup backlog exceeded -> cleanup or flatten
- market closed -> flatten
- explicit env/profile override must be honored
- non-reduce-only orders are cancelled on cleanup/flatten transitions

**Required Observability**
Every paper or live-debug run must emit enough information to explain the decision.

Minimum required per-market eval fields:
- market id
- cheap/expensive ids
- progress / elapsed / remaining
- session bucket
- BTC last / vol 5m / vol 15m / trade counts / returns / age
- cheap/expensive bid/ask
- gap
- hedge ratio
- book age / freshness / both-sides-present
- activity 10s / 30s / 60s / age
- first fill / first merge
- mode / aggression / signal clip scale / phase clip scale / buy clip scale
- market_has_inventory
- reasons

**Required Whale Shadow Compare**
Every meaningful paper run must be compared to what the whale was actually doing in the same windows.

Minimum comparison fields:
- whale participated in window or not
- we participated in window or not
- whale first-entry lag
- our first-entry lag
- whale merge presence
- our merge presence
- whale clip intensity proxy
- our order, fill, and cancel intensity
- mismatch bucket:
  - config
  - btc gate
  - geometry gate
  - timing
  - cleanup or salvage churn
  - control-plane/runtime

The whale is the label source for:
- when to participate
- how early to participate
- whether to keep pressing the window
- when cleanup dominates

The paper engine should always be reviewed as:
- `whale_only`
- `us_only`
- `both`
- `neither`

**Promotion Gates**
Do not move work upward until the lower layer is green.

1. `Unit gate`
- focused unlawful suite green
- no newly introduced config-resolution gaps

2. `Runtime gate`
- scenario harness green
- restart, reconcile, and cancel paths green

3. `Paper gate`
- fresh paper run produces structured eval logs
- no stale-slate or duplicate-order-id issues
- at least one active sleeve demonstrates real participation

4. `Calibration gate`
- window-by-window comparison exists
- top mismatch reasons are classified
- parameter changes are based on mismatch buckets, not ad hoc guesses

**Nightly Process**
1. Run focused unlawful suite.
2. If green, run or restart paper sleeves.
3. Let paper collect journals and order stores.
4. Export whale-vs-us comparison.
5. Bucket mismatches into:
- config
- gate
- geometry
- timing
- inventory lifecycle
- control-plane/runtime
6. Attribute those mismatches to the whale's actual behavior in the same windows, not just our internal runtime state.
7. Add or tighten one test for each real bug found.
8. Only then change runtime logic.

**Bug Policy**
For every real bug:
1. Write the failing behavior down as a scenario or unit assertion.
2. Fix the code.
3. Re-run the focused suite.
4. Re-run paper if the bug was runtime-visible.

If we skip step 1, the same class of bug will return.

**Repo Commands**
Primary focused suite:

```bash
whale-pair-exec/scripts/test_unlawful_stack.sh
```

Direct commands:

```bash
cargo test -p whale-pair-exec unlawful_gate -- --nocapture
cargo test -p whale-pair-exec unlawful_shear_from_env_respects_offhour_override_flag -- --nocapture
cargo test -p whale-pair-exec unlawful_shear_signal_reason_and_aggression_logged_in_decision_notes -- --nocapture
cargo test -p whale-pair-exec --test btc_5m_mm_scenarios -- --nocapture
python3 -m py_compile scripts/export_unlawful_whale_vs_us.py
```

**Done Condition**
The framework is in effect when:
- new unlawful behavior changes come with scenario coverage
- new config/runtime bug fixes come with unit coverage
- paper calibration is driven by structured logs and mismatch buckets
- live paper is no longer the first place we learn obvious behavior regressions
