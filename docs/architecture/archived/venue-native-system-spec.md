# Venue-Native System Spec

Status: target architecture for the repo's next execution generation

## Goal

Build a proper venue-native system around Polymarket CLOB V2 with:

- Python research plane
- Rust execution plane
- explicit monitoring
- passive standby with safe promotion
- crash/cutover recovery that matches current repo constraints

This spec is intentionally anchored to the current repo:

- keep Python for research/backtests/reporting
- do not force remote dependencies into the hot path
- preserve SQLite/local journaling where it helps recovery
- replace only the latency-sensitive and venue-coupled execution surface

## Service decomposition

### 1. Python research plane

Owns:

- strategy research scripts under `scripts/`
- whale analysis, wallet profiling, feature extraction
- backtests, replay, calibration, reporting
- market metadata enrichment from Gamma/Data APIs
- offline parameter generation for execution

Must not own:

- live order signing
- live order submission
- live cancel/replace loop
- live leader election

Repo mapping:

- keep `scripts/analyze_whale.py`, `scripts/save_whale_analysis.py`, `scripts/*paper_bot.py`, `docs/superpowers/*`
- move any remaining live-only execution concerns out of Python over time

### 2. Rust execution plane

New component, not yet present in repo.

Owns:

- CLOB V2 client lifecycle
- market websocket ingestion
- user websocket ingestion
- in-memory top-of-book state
- order submit/cancel/replace
- book freshness and quote eligibility
- startup venue reconciliation
- durable local execution journal

Non-goals for first cut:

- strategy research
- heavy analytics
- UI/dashboard rendering
- long-horizon historical ETL

Required interfaces:

- read-only config snapshot from repo-managed files/env
- local append-only execution journal on disk
- outbound metrics/log events
- optional RPC to Python research plane for shadow comparison only

### 3. Inventory and settlement worker

Separate from the Rust quoting loop, even if initially implemented in Python.

Owns:

- pair completion/merge intents
- redeem intents
- pUSD wrap/unwrap workflow when required
- idempotent on-chain retries

Reason:

`scripts/whale_pair_live_bot.py` currently does fill confirmation and merge handling in the same operational surface. That is workable for shadow mode, but not for a venue-native hot path.

### 4. Control plane

Small but explicit.

Owns:

- active vs standby node lease
- deployment state
- incident flagging / operator kill state
- last durable promoted run
- promoted config version

Use a remote store only for control-plane coordination, not for the quote loop.

Recommended concrete choice:

- PostgreSQL-compatible store, preferably the existing Supabase footprint if retained

Why:

- the repo already tolerates Supabase in live-adjacent paths
- operator tooling is simpler than introducing a new distributed system
- lease rows and promotion metadata are low write-volume

## Execution-plane design

### Hot-path loop

1. Connect CLOB V2 market WS.
2. Connect CLOB V2 user WS.
3. Load active market metadata for current and next BTC 5m windows.
4. Maintain per-token book state in memory.
5. Reject trading when:
   - book age exceeds threshold
   - user WS is disconnected beyond tolerance
   - reconciliation is dirty
   - control plane says this node is standby
6. Submit/cancel orders through V2 client only.
7. Persist every intent, ack, fill, cancel, and reject to a local append-only journal before acknowledging strategy state transitions.

## Local durability

Keep local disk durability because the repo already depends on SQLite and local ledgers.

Required artifacts on each execution node:

- append-only execution journal
- compact inventory snapshot
- last seen order map
- last seen book freshness markers

Concrete recommendation:

- SQLite in WAL mode for the journal/snapshots
- one DB for execution state, separate from research/reporting DBs

This keeps recovery aligned with current operational habits while moving the hot path out of Python.

## Startup reconciliation

Every execution-plane boot must do this before the node is allowed to quote:

1. Acquire or observe control-plane role.
2. Load local execution snapshot.
3. Authenticate to CLOB V2 and restore API creds.
4. Fetch:
   - balances/allowances
   - open orders
   - positions/inventory
   - current market metadata for target windows
5. Compare venue state to local journal.
6. Resolve one of:
   - clean: resume
   - stale local only: rebuild from venue
   - ambiguous split-brain: remain read-only and page operator
7. Only after clean reconciliation:
   - subscribe user WS
   - start quoting

Special case: cutover day / forced order wipe.

The system must treat "no open orders after reconnect" as a valid recovery state, not a failure, when the venue has wiped the book.

## Monitoring spec

### Metrics

Expose machine-readable metrics from execution and settlement workers.

Minimum set:

- `book_age_ms{token}`
- `ws_connected{channel}`
- `http_fallback_total`
- `order_submit_total{status}`
- `order_ack_latency_ms`
- `fill_confirm_latency_ms`
- `cancel_total{status}`
- `reconcile_dirty`
- `inventory_shares{market,side}`
- `local_journal_lag_ms`
- `control_plane_role`

### Logs

Structured JSON only for the execution plane.

Every order lifecycle event must include:

- market/window id
- token id
- side
- decision timestamp
- submit timestamp
- venue ack timestamp
- order id / client order id
- role (`primary` or `standby`)
- config version

### Alerts

Page on:

- no market WS
- no user WS
- reconciliation dirty for more than threshold
- primary and standby both claiming leader
- repeated order rejects
- journal write failure
- disk free below threshold

## Standby design

### Phase 1 standby: passive, operator-promoted

This is the right first step for this repo.

Behavior:

- standby runs the same subscriptions
- standby computes decisions in shadow
- standby does not submit orders
- standby writes its own local journal
- promotion is manual and explicit

Why manual first:

- the repo does not yet have a robust cancel-all / pause-user recovery surface
- split-brain risk is worse than slower failover

## Promotion sequence

1. Operator marks primary unhealthy.
2. Primary is hard-killed or lease revoked.
3. Standby acquires leader lease.
4. Standby runs full startup reconciliation.
5. Only then may it enable live submission.

## Recovery design

Recovery must cover four concrete cases already implied by repo behavior:

1. Process crash
   - rebuild from local journal + venue state
2. Host loss
   - promote standby after reconciliation
3. Venue disconnect
   - remain live only if books/user events are fresh enough
4. Venue maintenance / order wipe
   - rebuild with zero open orders as a valid venue state

## Migration sequencing

### Phase A

- finish docs/specs
- freeze V1-dependent live launch
- identify exact V2 blockers in code

### Phase B

- replace Python V1 client assumptions with a V2 compatibility shim
- get shadow mode running against V2
- keep live funding disabled

### Phase C

- implement Rust execution plane
- Python continues as shadow comparator

### Phase D

- move live quoting to Rust
- keep Python for research and settlement support

### Phase E

- add passive standby promotion
- validate cutover/crash recovery drills

## Definition of done

The system is venue-native only when all of these are true:

- live order submission no longer depends on `py-clob-client`
- Python is no longer in the hot order path
- pUSD collateral flow is implemented and tested
- startup reconciliation is explicit and blocking
- standby promotion is documented and rehearsed
- observability is metrics-first rather than log-grep-first
