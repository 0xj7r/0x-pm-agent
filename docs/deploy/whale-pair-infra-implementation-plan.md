# Whale-Pair Infra Implementation Plan

Status: revised against current repo and CLOB V2 reality  
Primary goal: produce a funded-live path that is venue-native, observable, and recoverable  
Primary references:

- `docs/architecture/current-runtime-and-v2-baseline.md`
- `docs/architecture/venue-native-system-spec.md`
- `docs/deploy/clob-v2-gap-analysis.md`

## Scope

This plan is for the whale-pair strategy specifically, but it uses repo-wide facts:

- current live code is Python
- whale-pair has its own dedicated runner and ledger
- the repo still depends on V1 `py-clob-client`
- CLOB V2 cutover is the forcing function

This is a build plan, not a generic ops memo.

## Repo-grounded current state

### What is already in place

- `scripts/whale_pair_live_bot.py`
  - dedicated pair runner
  - market WS first, HTTP `/book` fallback
  - local SQLite pair ledger
- `core/whale_pair_ledger.py`
  - domain model for fills, open lots, matches, actions, redeems
- `scripts/deploy/whale-pair/*.sh`
  - provisioning, env check, start, kill, health
- `scripts/deploy/whale-pair/docker-compose.whale-pair.yml`
  - isolated deployment topology from root `docker-compose.yml`

### What is not in place

- no Rust execution plane
- no V2-compatible client path
- no pUSD collateral workflow
- no metrics-first monitoring
- no control plane for standby promotion
- no recovery drill that handles V2 order wipe cleanly

## Design decision

Do not fund the existing Python whale-pair runner as the final production engine.

Use it only in these roles:

- V2 shadow validation
- strategy comparison harness
- ledger semantics reference
- incident replay input

The funded hot path should move to a Rust execution service, with Python retained for research and support workflows.

## Target rollout

### Phase 0: freeze unsafe assumptions

Before any live funding:

- no new V1-only runtime work
- no new USDC.e-only collateral assumptions in live docs
- no new deployment steps that assume Python is the permanent hot path

Exit:

- docs/specs approved

### Phase 1: Python shadow on CLOB V2

Objective:

- prove the repo can observe and reason against V2 before rewriting the hot path

Required work:

- replace V1 SDK usage in the Python live path with a V2 adapter
- validate:
  - market WS subscription behavior
  - user WS events
  - `/book` fallback behavior
  - order create/cancel/status semantics in shadow or smallest safe live test
- keep `--execute` disabled for the whale-pair runner until collateral flow is reworked

Exit:

- whale-pair shadow runs cleanly on `clob-v2.polymarket.com`

### Phase 2: collateral and settlement readiness

Objective:

- make funded trading possible in V2 terms, not V1 terms

Required work:

- define pUSD funding procedure
- implement wrap/approval flow for API-operated wallets
- audit `clients/ctf_merger.py` and `clients/ctf_redeemer.py` against V2-era contracts and payout flow
- update balance, funding, and P&L docs away from pure USDC.e thinking

Exit:

- operator can fund, wrap, verify, and reconcile pUSD-backed inventory on demand

### Phase 3: observability and recovery before funding

Objective:

- replace log-grep-only operations with a real operating surface

Required work:

- structured logs for whale-pair order lifecycle
- metrics endpoint for freshness, submissions, fills, reconciliation, and disk durability
- alerting for disconnect, stale book, repeated rejects, journal failure
- startup reconciliation flow documented and exercised

Exit:

- node restart and venue disconnect are both operator-visible and rehearseable

### Phase 4: Rust execution plane

Objective:

- move live order submission off Python

Required work:

- Rust CLOB V2 client integration
- in-memory book state
- user WS-driven fill state
- local durable execution journal
- pair order lifecycle manager

Python remains responsible for:

- research
- analytics
- replay
- shadow comparison
- reporting

Exit:

- Rust service can run in shadow and match Python decisions closely enough for promotion review

### Phase 5: standby and promotion

Objective:

- tolerate host loss without improvisation

Required work:

- passive standby host in same metro/region class
- remote lease/control state
- promotion runbook
- reconciliation gate on promotion

Exit:

- standby can be promoted without split-brain risk

## Monitoring specification for whale-pair

The current `health.sh` is a starting point, not the final design.

### Keep

- container-up check
- recent-log check
- disk-free check
- basic clock-drift check

### Add before funding

- machine-readable metrics endpoint
- alert destination
- per-order latency telemetry
- explicit reconciliation state
- standby role visibility

Minimum whale-pair metrics:

- `whale_pair_book_age_ms{token}`
- `whale_pair_ws_connected`
- `whale_pair_http_fallback_total`
- `whale_pair_decision_total{reason}`
- `whale_pair_order_submit_total{status}`
- `whale_pair_fill_confirm_latency_ms`
- `whale_pair_merge_total{status}`
- `whale_pair_reconcile_dirty`
- `whale_pair_open_lots`
- `whale_pair_role`

## Standby specification

### Initial mode

Passive standby only.

Behavior:

- runs subscriptions
- computes shadow decisions
- persists its own journal
- never submits while passive

### Promotion rules

Promotion must require:

1. primary declared unavailable
2. leader lease transferred
3. startup reconciliation completed
4. operator acknowledgement

Automatic active/active is out of scope for this repo version.

## Recovery specification

Every restart must run:

1. local journal load
2. venue auth/bootstrap
3. balance/open-order/position fetch
4. comparison with local ledger
5. explicit clean/dirty status
6. only then enable submissions

Special recovery cases that must be tested:

- process kill during partial fill
- host restart during open pair inventory
- venue WS disconnect with HTTP fallback
- CLOB V2 cutover with order wipe

## Ordered blockers

1. `clients/polymarket.py` is still V1-locked and blocks all live V2 execution.
2. `config.py` and settlement helpers are still USDC.e-centric, while funded V2 trading is pUSD-centric.
3. Whale-pair live deploy scripts assume container/process health but not venue reconciliation health.
4. There is no Rust execution plane yet; Python still owns the hot path.
5. There is no control plane or safe promotion mechanism for standby.
6. Monitoring is not yet strong enough for unattended funded operation.

## Concrete next outputs

1. V2 adapter design for `PolymarketClient`
2. pUSD funding and settlement implementation spec
3. Rust execution service skeleton and interface contract
4. whale-pair metrics and alert schema
5. standby promotion runbook

