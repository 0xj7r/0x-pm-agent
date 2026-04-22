# Whale-Pair Infra Implementation Plan

Status: active  
Primary region: `us-east-1`  
Runtime split: Python for shadow/research now, Rust hot path before funded live

## Objective

Launch the whale-pair strategy with infrastructure that is:

- low-latency enough for 5-minute crypto markets
- correct and observable
- resilient to venue/API failures
- upgradeable for the Polymarket CLOB V2 cutover

This plan optimizes for being consistently right and consistently available. It does not optimize for extreme HFT spend.

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

### Phase 2: Production hot path split

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

- [ ] fix health checks against actual log format
- [ ] reduce noisy Gamma rescans / resubscriptions
- [ ] capture shadow telemetry for at least one session:
  - `book_age_ms`
  - HTTP fallback rate
  - websocket reconnect count
  - ledger write cadence

Exit criterion:
- shadow runs cleanly for multiple hours without stale-book failures

### 2. Add backups and operator safety

- [ ] nightly tar/rsync snapshot of `/opt/polymarket-agent/data`
- [ ] cloud volume snapshots for the primary node
- [ ] documented restore test
- [ ] hard kill and soft kill runbook validation

Exit criterion:
- ledger can be restored onto a fresh machine

### 3. Add monitoring and alerting

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

### 4. Stand up passive standby

- [ ] duplicate the us-east primary
- [ ] run dry-run only
- [ ] validate shadow equivalence against the primary
- [ ] document failover procedure

Exit criterion:
- standby can take over in one command

### 5. Lock CLOB V2 migration path

- [ ] confirm Polymarket CLOB V2 changes and cutoff
- [ ] identify all code paths tied to current order format / collateral token
- [ ] build compatibility checklist
- [ ] run a canary path against the new environment if available

Exit criterion:
- no funded launch until the venue migration risk is understood

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

### 8. Promote to tiny funded live

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

### Data layer

- local SQLite ledger
- daily backup
- snapshot retention policy
- replayable market-state capture

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

1. CLOB V2 migration risk
2. current Python hot path is not the final production engine
3. websocket telemetry is present, but operational dashboards are still thin
4. current scanner/book subscription path is still noisier than it should be

## Immediate Next Actions

1. get the us-east shadow green
2. patch health/telemetry mismatches
3. add backup + alerting
4. provision passive standby
5. start Rust execution implementation
