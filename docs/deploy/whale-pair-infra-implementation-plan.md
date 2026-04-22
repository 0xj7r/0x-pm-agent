# Whale-Pair Infra Implementation Plan

Status: active  
Primary region: `us-east-1`  
Runtime split: Python for shadow/research now, Rust hot path before funded live
Launch mode: shadow-only until CLOB V2 compatibility is complete

## Objective

Launch the whale-pair strategy with infrastructure that is:

- low-latency enough for 5-minute crypto markets
- correct and observable
- resilient to venue/API failures
- compatible with the Polymarket CLOB V2 cutover

This plan optimizes for being consistently right and consistently available. It does not optimize for extreme HFT spend.

## Hard launch gate

Polymarket’s official docs now say:

- **latest changelog says CLOB V2 goes live on April 28, 2026 at ~11:00 UTC**
- **there is no backward compatibility after cutover**
- **legacy `py-clob-client` / `clob-client` integrations must migrate**
- **collateral changes from `USDC.e` to `pUSD`**

That means the current Python deployment is a **shadow harness only**. It is useful for:

- live websocket telemetry
- scanner/load testing
- ledger behavior
- host/ops validation

It is **not** the thing we should fund live.

References:

- Polymarket changelog, Apr 17 2026
- Polymarket “Migrating to CLOB V2”

Note: the migration guide and the latest changelog disagree on the exact go-live date. The changelog is newer, so treat **April 28, 2026** as canonical until Polymarket says otherwise.

## Principles

1. The hot path must stay simple:
   - market websocket
   - in-memory state
   - strategy decision
   - order/cancel submission

2. The bot must degrade safely:
   - stale book detection
   - kill switch
   - no blocking merge/redeem path in the quote loop

3. Research and live execution stay separate:
   - Python for backtests, replay, analytics
   - Rust for the eventual execution engine

4. We do not fund live capital until:
   - the shadow system is stable
   - telemetry says the environment is healthy
   - V2 compatibility is verified

## Target Architecture

### Phase 0: Current state

- `main` contains the whale-pair dry-run stack
- us-east-1 primary host runs `scripts/whale_pair_live_bot.py` in dry-run
- ledger is local SQLite
- deployment is Docker Compose
- current client stack still depends on legacy `py-clob-client`

This is a shadow system, not a funded strategy.

### Phase 1: Lean production shadow stack

One primary node in `us-east-1`:

- `whale-pair-live` container
- local SQLite ledger
- structured JSON-ish logs via Docker
- health checks via `scripts/deploy/whale-pair/health.sh`
- daily backup of `/opt/polymarket-agent/data`

One passive standby node in the same region:

- same code
- same dry-run mode
- same market subscriptions
- no trading

### Phase 2: V2-compatible execution path

Replace the legacy client surface with a V2-capable path:

1. **Client / API layer**
   - move from `py-clob-client` to `py-clob-client-v2` or direct V2-compatible Rust client
   - update auth/bootstrap flow if needed
   - update order create/cancel/status calls to V2 semantics

2. **Order model**
   - remove V1-only assumptions around `nonce`, `feeRateBps`, `taker`
   - handle V2 order fields: `timestamp`, `metadata`, `builder`
   - treat fees as match-time state, not signed-order state

3. **Collateral / settlement**
   - move collateral assumptions from `USDC.e` to `pUSD`
   - audit merger/redeemer and any base-unit conversion helpers
   - verify token ops against the new contracts before live capital

4. **Venue cutover handling**
   - all open orders get wiped at cutover
   - no funded launch before this path is tested against V2

Exit criterion:
- the shadow bot runs against V2 endpoints without legacy package assumptions

### Phase 3: Production hot path split

Separate components:

1. **Execution engine**
   - Rust
   - Polymarket CLOB websocket + user channel
   - in-memory order book
   - quote/cancel/order loop

2. **Inventory worker**
   - handles merge/split/redeem operations
   - cannot block the quoting loop

3. **Risk daemon**
   - independent process
   - exposure caps
   - stale feed protection
   - dead-man switch

4. **Research / replay plane**
   - Python
   - historical replay
   - wallet clustering
   - execution feature inference

## Implementation Order

### 1. Stabilize the current us-east shadow

- [ ] fix health checks against actual log format and pipefail behavior
- [ ] reduce noisy Gamma rescans / resubscriptions
- [ ] capture shadow telemetry for at least one session:
  - `book_age_ms`
  - HTTP fallback rate
  - websocket reconnect count
  - ledger write cadence

Exit criterion:
- shadow runs cleanly for multiple hours without stale-book failures

### 2. Lock CLOB V2 migration path

- [ ] replace `py-clob-client` dependency in the live path
- [ ] audit all V1 order-shape assumptions
- [ ] audit all collateral assumptions (`USDC.e` -> `pUSD`)
- [ ] validate base URL / hot-swap behavior against official V2 docs
- [ ] test against `https://clob-v2.polymarket.com`
- [ ] document exact cutover risks and rollback posture

Exit criterion:
- us-east shadow is running on a V2-compatible client path

### 3. Add backups and operator safety

- [ ] nightly tar/rsync snapshot of `/opt/polymarket-agent/data`
- [ ] cloud volume snapshots for the primary node
- [ ] documented restore test
- [ ] hard kill and soft kill runbook validation

Exit criterion:
- ledger can be restored onto a fresh machine

### 4. Add monitoring and alerting

Minimum:

- [ ] health script from cron every minute
- [ ] alert on non-zero exit
- [ ] alert on no recent logs
- [ ] alert on stale ledger
- [ ] alert on stale book or repeated HTTP fallback

Preferred:

- [ ] node exporter + Prometheus
- [ ] Grafana dashboard
- [ ] Telegram/Discord alert hook

Exit criterion:
- operator is notified on disconnect, stale feed, or failed container

### 5. Stand up passive standby

- [ ] duplicate the us-east primary
- [ ] run dry-run only
- [ ] validate shadow equivalence against the primary
- [ ] document failover procedure

Exit criterion:
- standby can take over in one command

### 6. Build Rust execution engine

Scope:

- market websocket ingestion
- user websocket ingestion
- in-memory per-market state
- order/cancel/replace loop
- heartbeat / stale-order protection
- API auth lifecycle

Python remains responsible for:

- research
- reporting
- replay
- non-hot-path analysis

Exit criterion:
- Rust engine can shadow the Python dry-run decisions without trading

### 7. Move merge/split/redeem off the hot path

- [ ] separate worker for token ops
- [ ] queue intents from the execution engine
- [ ] make merge/redeem idempotent and restart-safe
- [ ] expose inventory state to the risk daemon

Exit criterion:
- quoting continues even if token ops lag

### 8. Re-validate strategy economics on actual shadow telemetry

- [ ] feed real `book_age_ms`, fallback rate, and reconnect data back into replay
- [ ] re-run execution-realism scenarios with shadow-derived latency
- [ ] verify edge survives V2 fee/collateral assumptions
- [ ] confirm whether maker behavior is required

Exit criterion:
- paper/shadow economics are still positive under observed conditions

### 9. Promote to tiny funded live

Preconditions:

- [ ] us-east primary stable
- [ ] standby stable
- [ ] V2 compatibility acceptable
- [ ] Rust or equivalent execution path ready
- [ ] telemetry reviewed
- [ ] separate funded wallet/proxy prepared

Initial live constraints:

- very low gross cap
- strict stale-book halt
- strict order reject threshold
- strict inventory drift threshold

Exit criterion:
- real fills and reconciliations match expectations before any scaling

## Concrete Infra Deliverables

### Host layer

- primary `us-east-1` node
- passive standby `us-east-1` node
- SSH keys isolated from Hetzner
- chrony enabled
- Docker + Compose

### Runtime layer

- `whale-pair-live` compose stack
- dry-run env
- live env template
- kill scripts
- health scripts
- V2-compatible client package / runtime

### Data layer

- local SQLite ledger
- daily backup
- snapshot retention policy
- replayable market-state capture
- cutover-safe migration notes for wiped open orders

### Monitoring layer

- health checks
- alert transport
- log retention
- key metrics:
  - websocket age
  - HTTP fallback rate
  - decision-to-submit time
  - order reject rate
  - inventory drift

## Budget

Target spend for the first serious version:

- primary us-east node
- passive standby
- storage/snapshots
- lightweight monitoring

Expected band:

- roughly `$120–$220/month`

Do not add:

- managed Kubernetes
- distributed databases in the hot path
- GPU nodes
- multi-region active/active

until the strategy has proven itself.

## Known Open Risks

1. current deploy still uses legacy `py-clob-client` semantics
2. CLOB V2 cutover wipes all open orders and changes collateral/order model
3. current Python hot path is not the final production engine
4. websocket telemetry is present, but operational dashboards are still thin
5. current scanner/book subscription path is still noisier than it should be

## Immediate Next Actions

1. get the us-east shadow green
2. patch health/telemetry mismatches
3. migrate the shadow path off legacy `py-clob-client`
4. add backup + alerting
5. provision passive standby
6. start Rust execution implementation
