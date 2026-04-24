# BTC 5m MM Engine Implementation Spec

Status: draft for implementation handoff  
Date: 2026-04-23  
Primary target: `whale-pair-exec`  
Secondary targets: Python discovery/control-plane helpers under `execution/` and `scripts/`  
Execution posture: dual-sleeve platform

## 1. Purpose

This document specifies the implementation required to turn the current Rust execution runtime into a production-grade Polymarket execution platform for BTC 5-minute markets with two concurrent sleeves:

- sleeve A: maker-first MM / complement-bid engine
- sleeve B: two-sided taker / recycler engine already under development

It is written as a handoff document for multiple implementation agents. The emphasis is:

- exact architecture boundaries
- current-state-aware file ownership
- required interfaces
- durable state model
- live safety invariants
- phased delivery and acceptance criteria

This spec is intentionally concrete. It should be possible to assign sections of this document to separate agents without requiring them to infer hidden requirements from prior research.

## 2. Executive Summary

The repo already contains a substantial Rust execution scaffold in `whale-pair-exec`:

- market websocket ingestion
- user websocket ingestion
- in-memory runtime and inventory
- risk checks
- queue/depth-aware paper matching
- journal and event log hooks
- strategy logic for paired execution variants
- dashboard and metrics

The missing pieces are the ones that make a market-making engine real:

- a live CLOB execution adapter
- durable order/fill/reconciliation state
- complement-bid quote engine
- explicit inventory and pair-completion management
- startup recovery and replay
- stale-data and kill-switch controls
- canary and rollout discipline

The objective is not to build a generic “trading bot.” The objective is a venue-native execution plane optimized for:

- BTC 5m complement-style quoting
- maker-first flow capture
- rapid pair completion and merge/redeem recycling
- limited stranded inventory
- exact fee/rebate-aware accounting
- concurrent paper operation across multiple strategy sleeves
- fast promotion of proven sleeves into tiny-live calibration mode

## 3. Scope

### In scope

- Rust execution plane for BTC 5m maker-first operation
- quote generation, submit/cancel/replace, fill reconciliation
- live inventory and pair ledger
- merge/redeem orchestration boundary
- metrics, alerts, health checks, and canary controls
- persistent state needed for restart-safe operation
- small control-plane hooks for rotating into active BTC windows

### Out of scope for v1

- full multi-venue routing
- sports/event MM strategy logic
- cross-host active-active leader election
- optimizer/ML policy learning
- generalized whale-copy ingestion as a hard dependency
- colo-specific infra work

### Explicit non-goal

This spec does not aim to replicate any single observed wallet exactly. It aims to build infrastructure capable of expressing:

- unlawful-style maker-first complement bidding
- hybrid maker/taker cleanup behavior
- eventual extension to other market sleeves

## 4. Current Baseline

### Rust execution runtime already present

Current crate:

- `polymarket-exec/src/config.rs`
- `polymarket-exec/src/book.rs`
- `polymarket-exec/src/market_ws.rs`
- `polymarket-exec/src/user_ws.rs`
- `polymarket-exec/src/runtime.rs`
- `polymarket-exec/src/inventory.rs`
- `polymarket-exec/src/risk.rs`
- `polymarket-exec/src/strategy.rs`
- `polymarket-exec/src/runner.rs`
- `polymarket-exec/src/journal.rs`
- `polymarket-exec/src/metrics.rs`
- `polymarket-exec/src/api.rs`

### Baseline strengths

- typed runtime and inventory loop already exists
- strategy decisions are already emitted as `RuntimeCommand`
- user websocket events already enter runtime state transitions
- paper simulation already models depth and some maker/taker behavior
- metrics and dashboard wiring already exist

### Baseline weaknesses

- no real downstream execution adapter
- no durable order/fill state beyond journal/event log
- no startup reconciliation against live venue state
- no pair ledger as authoritative source of merge eligibility
- no explicit live quote laddering logic
- no full stale-feed / staleness gate / hard stop layer
- config is env-heavy and not versioned by strategy profile

## 5. Product Requirements

### Core product objective

For each active BTC 5m market pair, the engine should:

1. subscribe to both YES and NO order books
2. maintain maker quotes on both sides when edge is positive
3. track partial fills and pair completion in real time
4. merge or redeem completed pairs to free capital
5. skew or pull quotes when adverse selection risk rises
6. survive restart without orphaning live orders or mis-accounting inventory

### Economic objective

Primary edge sources:

- maker execution
- complement spread / pair completion economics
- rebates where applicable
- disciplined inventory recycling

Secondary edge sources:

- short-horizon spot-informed skewing or quote suppression
- cleanup taker actions only when required to protect pair economics

### Operational objective

The engine must fail closed:

- if data is stale, stop quoting
- if reconciliation is uncertain, stop quoting
- if inventory exceeds hard limits, stop quoting
- if control-plane context for active market is missing, do not improvise

## 6. System Design Principles

1. Maker-first, not maker-only.
2. Runtime truth must not depend on the dashboard.
3. Venue acknowledgements beat local assumptions.
4. Every order action must be replayable.
5. Quote logic and risk logic must remain separately testable.
6. Control-plane discovery must not block the hot path.
7. Merge economics must be first-class, not an afterthought.
8. Restart safety matters more than marginal quote sophistication.

## 7. Target Runtime Topology

### Single-host topology for v1

One primary runtime process in US East:

- market websocket task
- user websocket task
- execution adapter task
- runtime loop
- metrics/http task
- optional merge worker

One warm standby process:

- discovery and passive health only
- no quoting until manual or explicit promotion

### Process boundaries

Process A: `whale-pair-exec`

- owns quote generation
- owns order lifecycle
- owns runtime inventory and pair ledger state
- owns journal and reconciliation

Process B: market discovery / control-plane helper

- scans Gamma / market metadata
- maps active BTC 5m markets to instrument IDs
- writes current market context consumed by the Rust process

Process C: optional post-trade maintenance worker

- merge/redeem execution
- settlement cleanup
- archival and reporting

v1 may run B and C on the same host. They must not share mutable hot-path state with A except through explicit files or storage APIs.

## 8. Required Modules

The sections below specify the target shape, not just suggestions.

### 8.1 Live CLOB Adapter

New module:

- `polymarket-exec/src/execution_adapter.rs`

Responsibilities:

- submit limit orders
- cancel orders
- replace orders by cancel + resubmit or native replace if available
- normalize venue responses
- emit deterministic execution events into runtime

Required interface:

```rust
pub trait ExecutionAdapter {
    async fn submit(&self, req: SubmitOrderRequest) -> Result<SubmitOrderAck, ExecutionError>;
    async fn cancel(&self, req: CancelOrderRequest) -> Result<CancelOrderAck, ExecutionError>;
    async fn sync_open_orders(&self) -> Result<Vec<VenueOpenOrder>, ExecutionError>;
    async fn sync_balances(&self) -> Result<VenueBalances, ExecutionError>;
}
```

Required request/response types:

- `SubmitOrderRequest`
- `SubmitOrderAck`
- `CancelOrderRequest`
- `CancelOrderAck`
- `VenueOpenOrder`
- `VenueBalances`
- `ExecutionError`

Required behavior:

- client order IDs must be generated locally and sent downstream
- all requests must be idempotent at the caller level
- all acks must include local `client_order_id` plus venue `order_id` where available
- errors must classify retryability

Retry classes:

- transient network
- auth failure
- bad request
- rate limit
- venue rejection
- uncertain submission outcome

Uncertain submission outcome must trigger reconciliation, not blind retry.

### 8.2 Order Lifecycle Store

New module:

- `polymarket-exec/src/order_store.rs`

Responsibilities:

- authoritative local view of:
  - intended orders
  - submitted orders
  - acknowledged orders
  - partial fills
  - cancel requested
  - cancelled
  - filled
  - rejected
  - unknown / needs reconcile

State transitions must be explicit and logged.

Required persisted fields:

- `run_id`
- `client_order_id`
- `venue_order_id`
- `market_id`
- `instrument_id`
- `side`
- `limit_price`
- `original_qty`
- `remaining_qty`
- `filled_qty`
- `status`
- `submitted_at_ms`
- `last_update_ms`
- `reason`
- `strategy_tag`
- `quote_level_tag`

### 8.3 Durable Journal

Current module exists:

- `polymarket-exec/src/journal.rs`

It must be upgraded to support:

- append-only command journal
- append-only execution-event journal
- periodic checkpoints

Durable journal record types:

- `intent_submit`
- `intent_cancel`
- `adapter_submit_ack`
- `adapter_submit_error`
- `adapter_cancel_ack`
- `adapter_cancel_error`
- `user_fill`
- `user_cancel`
- `user_reject`
- `reconcile_snapshot`
- `merge_started`
- `merge_completed`
- `runtime_halt`

Journal invariants:

- any downstream action must be journaled before or at the exact moment runtime accepts it as real
- replay from journal plus reconcile must reconstruct local order state after crash

### 8.4 Pair Ledger

New module:

- `polymarket-exec/src/pair_ledger.rs`

Purpose:

Track paired YES/NO inventory at the market level instead of only per-instrument position totals.

This is required because market-making economics depend on:

- how much paired inventory exists
- how much stranded YES exists
- how much stranded NO exists
- weighted acquisition cost of each leg
- whether merge is profitable now

Required ledger entities:

- `MarketPairState`
- `PairLot`
- `StrandedLot`
- `MergeCandidate`

Per-market tracked fields:

- total YES qty
- total NO qty
- paired qty = `min(yes_qty, no_qty)`
- stranded YES qty
- stranded NO qty
- weighted average acquisition price per leg
- mergeable notional
- expected merge gain/loss after fees/gas
- last merge timestamp

Pairing model:

- FIFO by default
- weighted average summary for runtime decisions
- full lot detail retained for reconciliation and analytics

### 8.5 Quote Engine

New module:

- `polymarket-exec/src/quote_engine.rs`

Responsibilities:

- compute target passive quotes on YES and NO
- maintain quote ladders
- produce desired quote set per market

Required inputs:

- best bid/ask and depth
- recent fill imbalance
- current inventory skew
- pair completion state
- market age / time to expiry
- external spot or timing signal if enabled
- config profile

Required outputs:

- `DesiredQuoteSet`
  - zero or more bid quotes on YES
  - zero or more bid quotes on NO
  - optional aggressive cleanup intents
  - quote metadata tags for attribution

Minimum v1 quoting model:

- one to three bid levels per side
- size ladder by edge and inventory
- widen or pull if skew too large
- no asks required for v1 complement-bid mode

Required decision fields per quote:

- `instrument_id`
- `price`
- `quantity`
- `quote_role`
  - `primary_yes_bid`
  - `primary_no_bid`
  - `inventory_repair`
  - `cleanup_cross`
- `edge_bps`
- `inventory_skew_before`
- `time_bucket`

### 8.6 Quote Reconciler

New module:

- `polymarket-exec/src/quote_reconciler.rs`

Responsibilities:

- compare desired quote set versus live working orders
- decide:
  - keep
  - cancel
  - replace
  - submit new

Rules:

- never spam replace if price/size unchanged
- minimum quote age before replace unless hard risk override
- hard pull if market data stale or expiry cutoff reached
- max N cancels / submits per loop to stay inside venue limits

### 8.7 Merge Executor Boundary

New module:

- `polymarket-exec/src/merge_executor.rs`

Responsibilities:

- decide whether to merge now or defer
- submit merge operation through existing on-chain helper boundary
- write merge lifecycle events

v1 acceptable implementation:

- thin Rust wrapper around existing merge path if robust
- or explicit command out to a controlled helper process

Required merge decision inputs:

- paired qty
- gas estimate
- expected recovered collateral
- time to market resolution
- current free cash pressure

Rules:

- merge is not mandatory immediately upon pair completion
- merge should free capital when paired qty is meaningful or cash pressure is high
- merge should not block quoting loop

### 8.8 Reconciliation Engine

New module:

- `polymarket-exec/src/reconcile.rs`

Responsibilities:

- startup sync against venue state
- periodic background sync
- resolve uncertain order states
- compare local and venue balances
- compare local and venue open orders

Startup sequence must be:

1. load journal/checkpoint
2. fetch venue open orders
3. fetch venue balances
4. fetch recent user events if available
5. repair local state
6. only then allow quoting

If reconciliation confidence is below threshold, runtime must remain in degraded mode.

### 8.9 Market Discovery / Rotation

Current state:

- external discovery exists elsewhere in repo

Required output into Rust runtime:

- current active BTC 5m market pair
- next active market pair
- asset IDs for YES/NO instruments
- market start/end timestamps
- price-to-beat / context if available

Recommended file/API contract:

- JSON file updated by control-plane helper
- atomically replaced
- consumed by `market_context.rs`

Required schema:

```json
{
  "generated_at_ms": 0,
  "markets": [
    {
      "market_id": "condition-id",
      "yes_asset_id": "token-id",
      "no_asset_id": "token-id",
      "symbol": "BTC",
      "window_seconds": 300,
      "start_time_ms": 0,
      "end_time_ms": 0,
      "final_price": null,
      "price_to_beat": null
    }
  ]
}
```

### 8.10 Signal Overlay

Optional for v1, but module boundary should exist now:

- `polymarket-exec/src/signal_overlay.rs`

Responsibilities:

- provide risk-off / skew multipliers
- not direct entry signals

Initial supported overlays:

- spot move velocity
- spot/orderbook volatility
- whale activity overlay
- time-to-expiry phase classification

Required outputs:

- `quote_width_multiplier`
- `quote_size_multiplier`
- `maker_enabled`
- `cleanup_bias`

## 9. Persistence Model

v1 durable state should use SQLite or Postgres. For speed of implementation and consistency with repo history:

- local SQLite is acceptable for v1
- schema must be explicit and migration-backed

Required tables:

### `runs`

- `run_id`
- `strategy_name`
- `started_at_ms`
- `stopped_at_ms`
- `host`
- `git_sha`
- `config_hash`
- `paper_mode`
- `status`

### `orders`

- fields from order store

### `fills`

- `fill_id`
- `run_id`
- `venue_order_id`
- `client_order_id`
- `market_id`
- `instrument_id`
- `side`
- `price`
- `quantity`
- `fee_usd`
- `liquidity`
- `close_method`
- `observed_at_ms`

### `pair_lots`

- `lot_id`
- `run_id`
- `market_id`
- `yes_qty`
- `no_qty`
- `yes_cost_usd`
- `no_cost_usd`
- `status`
- `created_at_ms`
- `updated_at_ms`

### `merge_events`

- `merge_id`
- `run_id`
- `market_id`
- `quantity`
- `expected_cash_usd`
- `actual_cash_usd`
- `gas_usd`
- `submitted_at_ms`
- `completed_at_ms`
- `status`

### `checkpoints`

- `checkpoint_id`
- `run_id`
- `created_at_ms`
- `runtime_status`
- `inventory_json`
- `open_orders_json`
- `pair_state_json`

## 10. Configuration Model

Current env-based config should remain supported, but we need one versioned strategy profile file.

New config path:

- optional named profile artifacts if we later choose to serialize sleeve variants into JSON

Required fields:

- quote ladder count
- base clip size
- max quote per side
- quote refresh cadence
- min edge threshold
- skew caps
- merge thresholds
- cleanup thresholds
- expiry phase cutoffs
- stale feed cutoffs
- kill switch thresholds

Example top-level shape:

```json
{
  "profile_name": "btc_5m_mm_v1",
  "quote": {
    "levels_per_side": 2,
    "base_clip_usd": 25.0,
    "max_clip_usd": 100.0,
    "min_edge_bps": 15.0,
    "min_quote_age_ms": 700,
    "refresh_interval_ms": 300
  },
  "inventory": {
    "max_gross_notional_usd": 10000.0,
    "max_net_notional_per_market_usd": 1500.0,
    "max_stranded_leg_usd": 750.0,
    "pair_merge_min_qty": 25.0
  },
  "risk": {
    "book_stale_ms": 1200,
    "user_ws_stale_ms": 3000,
    "max_consecutive_reconcile_failures": 3,
    "disable_making_on_spot_shock_bps": 25.0
  }
}
```

## 11. Runtime State Machine

### Top-level runtime states

- `Starting`
- `Reconciling`
- `Running`
- `Degraded`
- `RiskOff`
- `Stopped`

### Allowed transitions

- `Starting -> Reconciling`
- `Reconciling -> Running`
- `Reconciling -> Degraded`
- `Running -> RiskOff`
- `Running -> Degraded`
- `RiskOff -> Running`
- `RiskOff -> Degraded`
- `Degraded -> Reconciling`
- `* -> Stopped`

### State behavior

`Running`

- passive quoting allowed
- cleanup taker actions allowed if enabled

`RiskOff`

- no new maker quotes
- allow cancels
- allow merge
- allow controlled cleanup only

`Degraded`

- no new exposure
- cancel live orders
- no merge unless needed for cash safety

## 12. Safety Invariants

These must be enforced in code, not only in docs.

1. No quote placement if book is stale.
2. No quote placement if user stream auth is configured but disconnected past threshold.
3. No quote placement if startup reconciliation has not completed.
4. No order may exceed configured per-order notional.
5. No market may exceed configured net notional.
6. No runtime tick may assume an order exists without either:
   - local submit ack, or
   - venue reconciliation evidence.
7. Cancels must be issued before replace if native replace is unavailable.
8. Stranded inventory beyond threshold must force quote skew or quote suppression.
9. Any uncertain submission outcome must put the affected market into reconcile-only mode.
10. Merge execution failure must not corrupt inventory or pair ledger state.

## 13. Metrics and Telemetry

Current metrics exist but are insufficient. Add:

### Quote quality

- `quote_submit_total`
- `quote_cancel_total`
- `quote_replace_total`
- `quote_live_count`
- `quote_age_ms`
- `quote_edge_bps`

### Fill quality

- `fill_total`
- `fill_maker_total`
- `fill_taker_total`
- `fill_maker_share`
- `fill_notional_usd_total`

### Pair completion

- `pair_completed_qty_total`
- `stranded_yes_qty`
- `stranded_no_qty`
- `merge_candidate_qty`
- `merge_latency_ms`

### Risk / quality

- `book_stale_events_total`
- `reconcile_failures_total`
- `runtime_riskoff_transitions_total`
- `uncertain_submit_total`
- `inventory_skew_usd`

### Economics

- `realized_pnl_usd`
- `unrealized_pnl_usd`
- `fees_usd_total`
- `rebates_usd_total`
- `net_edge_usd_total`

### Host health

- `market_ws_connected`
- `user_ws_connected`
- `execution_adapter_connected`
- `last_market_message_age_ms`
- `last_user_message_age_ms`
- `last_reconcile_age_ms`

## 14. Alerts

Required live alerts:

- market WS disconnected > threshold
- user WS disconnected > threshold
- no successful reconcile in N seconds
- open orders present but runtime in degraded mode
- quote age unexpectedly high
- stranded inventory above cap
- gross exposure above cap
- repeated submit uncertainty
- repeated cancel rejection

## 15. Testing Requirements

### Unit tests

Must cover:

- quote generation
- quote reconciliation diff logic
- pair ledger transitions
- merge candidate computation
- risk gates
- runtime state transitions

### Deterministic scenario tests

New test fixture directory:

- `tests/fixtures/btc_5m_mm/`

Fixture classes:

- clean passive fills on both sides
- one-sided fill then reversal
- stale-book event
- uncertain submit
- reconnect after partial fills
- end-of-window forced cleanup

### Integration tests

Required:

- adapter mock with submit/cancel/ack/fill lifecycle
- replay from journal + reconcile
- crash mid-submit then recovery
- crash mid-merge then recovery

### Paper/live shadow mode

Before real money:

- live books + live user channel + paper execution adapter
- desired quote set recorded to journal
- compare desired quotes against actual market evolution

## 16. Rollout Plan

### Phase 0: execution truth

Deliver:

- real execution adapter
- durable order store
- startup reconciliation

Acceptance:

- engine can submit and cancel one live quote safely
- restart preserves order truth

### Phase 1: maker-first single-market canary

Deliver:

- quote engine v1
- quote reconciler v1
- pair ledger v1

Acceptance:

- one BTC 5m market only
- tiny clip sizes
- maker fills observed
- no stuck orders after window ends

### Phase 2: merge/recycle

Deliver:

- merge executor
- economics metrics
- pair completion reporting

Acceptance:

- completed pairs merged/redeemed safely
- cash recycling visible and accurate

### Phase 3: production pilot

Deliver:

- risk-off layer
- alerts
- canary controls
- profile-based config

Acceptance:

- 24h stable operation
- controlled quotes across multiple BTC windows
- operator confidence in restart/reconcile path

## 17. Detailed Task Decomposition For Parallel Agents

Each task below should have one owner and a disjoint write scope where possible.

### Agent A: Live execution adapter

Write scope:

- `polymarket-exec/src/execution_adapter.rs`
- `polymarket-exec/src/types.rs`
- `polymarket-exec/src/runner.rs`

Deliverables:

- adapter trait and live Polymarket implementation
- submit/cancel request types
- normalized downstream acks/errors
- integration into runtime loop

Acceptance:

- compile passes
- submit/cancel integration test passes
- paper adapter still works

### Agent B: Durable order store and reconciliation

Write scope:

- `polymarket-exec/src/order_store.rs`
- `polymarket-exec/src/reconcile.rs`
- `polymarket-exec/src/journal.rs`
- `polymarket-exec/src/runtime.rs`

Deliverables:

- persistent order lifecycle store
- startup reconciliation path
- uncertain-submit handling
- checkpoint serialization

Acceptance:

- replay test passes
- restart reconciliation test passes

### Agent C: Pair ledger and merge boundary

Write scope:

- `polymarket-exec/src/pair_ledger.rs`
- `polymarket-exec/src/merge_executor.rs`
- `polymarket-exec/src/inventory.rs`
- `polymarket-exec/src/runtime.rs`

Deliverables:

- market-level pair tracking
- merge candidate logic
- merge lifecycle events

Acceptance:

- paired fill scenarios produce correct mergeable qty
- merge completion updates cash and pair state correctly

### Agent D: Quote engine and quote reconciler

Write scope:

- `polymarket-exec/src/quote_engine.rs`
- `polymarket-exec/src/quote_reconciler.rs`
- `polymarket-exec/src/strategy.rs`
- `polymarket-exec/src/risk.rs`

Deliverables:

- complement-bid quote logic
- quote diffing and order action planner
- skew-aware quote suppression

Acceptance:

- deterministic quote tests pass
- no churn on unchanged desired quotes

### Agent E: Config, metrics, control-plane

Write scope:

- `polymarket-exec/src/config.rs`
- `polymarket-exec/src/metrics.rs`
- `polymarket-exec/src/api.rs`
- `execution/clients/*` or `scripts/*` for market context generation

Deliverables:

- profile-based strategy config
- extra metrics and API state
- market context handoff contract

Acceptance:

- runtime boots from profile config
- dashboard exposes added risk/economics fields

### Agent F: Tests and replay harness

Write scope:

- `tests/*`
- `polymarket-exec/tests/*` if added
- `tests/fixtures/btc_5m_mm/*`

Deliverables:

- adapter mock
- restart/reconcile tests
- quote engine scenario tests
- paper/live shadow harness support

Acceptance:

- full test suite passes
- key failure-mode fixtures covered

## 18. Interfaces Between Agents

To avoid drift, the following shared contracts must be agreed before parallel implementation proceeds:

1. `ExecutionAdapter` trait
2. `ManagedOrderStatus` lifecycle enum
3. `PairLot` and `MarketPairState` schema
4. `DesiredQuoteSet` schema
5. checkpoint serialization format
6. strategy profile JSON schema

If any agent needs to change one of these, they must update this spec first or produce a patch note against it.

## 19. Open Questions To Resolve During Implementation

These are real questions, but none should block starting the scaffolding.

1. Should durable state remain SQLite in v1, or move directly to Postgres?
2. What is the best venue-side source of truth for “open orders at startup”?
3. Is merge execution better as in-process Rust or external helper in v1?
4. Which exact rebate fields are available live and how should they be reconciled into net PnL?
5. What minimum quote age avoids pointless queue churn on BTC 5m?

## 20. Definition Of Done

The engine is ready for a funded canary when all of the following are true:

- live adapter is wired and tested
- startup reconciliation is mandatory and working
- pair ledger exists and is correct on replay
- quote engine produces complement bids on both sides
- quote reconciler can maintain and pull quotes deterministically
- runtime halts on stale data or reconcile uncertainty
- metrics expose fill quality, skew, and economics
- one-day canary can run without orphaned orders or unexplained inventory

## 21. Immediate Next Step

Before parallel implementation begins:

1. Freeze the shared contracts listed in section 18.
2. Create the missing module files with empty trait/type skeletons.
3. Assign Agents A-F ownership exactly as listed above.
4. Merge only contract-safe patches first, then runtime wiring, then live canary logic.

That order matters. The biggest implementation risk is not strategy logic. It is state inconsistency across submit, fill, cancel, merge, and restart.
