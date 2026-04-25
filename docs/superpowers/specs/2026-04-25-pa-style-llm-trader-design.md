# PA-Style LLM Trader on Polymarket: System Design

_Date: 2026-04-25_
_Status: Approved design, pending implementation plan_

## Motivation

Build a new strategy on Polymarket inspired by Prediction Arena's methodology
(`predictionarena.ai`): an LLM agent that runs a periodic trading cycle
(receive market data, review portfolio, research, decide, trade) on
long-horizon event markets. Adapt PA's Kalshi-on-baseline-tools approach by
(a) targeting Polymarket via existing Rust execution rails and (b) giving the
agent Polymarket-specific alpha tools (wallet research, on-chain whale
activity, resolution-rule analysis) that PA lacks.

This is a new alpha sleeve, not a benchmark. It runs alongside the existing
`unlawful_shear` / `goat_pair` BTC 5m strategies; capital and execution are
isolated.

## Scope

**In scope:**

- Long-horizon Polymarket event markets (politics, sports, entertainment,
  weather, macro, crypto event markets resolving over hours to weeks).
- Single agent (one LLM model) trades real capital. Optional shadow paper
  models on the same prompts/tools for comparison (deferred to v2).
- TS orchestrator (Claude Agent SDK) owns the cycle loop, prompt, tools,
  notes, and web search. Rust daemon owns venue I/O, recon, risk gating, and
  persistence.
- 1 hour baseline cadence with event-triggered re-runs (large book moves on
  watchlist, whale entries, news webhooks).
- Phased rollout: paper (3-5 days, operational shakedown) -> tiny-live ($200-500,
  2-4 weeks) -> scale ($1-5k, 4-8 weeks) -> further based on Sharpe / max DD.

**Explicitly out of scope (MVP):**

- Short-horizon BTC 5m markets (covered by existing `unlawful_shear`).
- Multi-model competition (PA-style leaderboard with capital split).
- Resting / limit orders past the cycle (immediate-fill semantics for MVP).
- Cross-market arbitrage automation (`cross_market_correlation` tool deferred).
- Self-reflection / "devil's advocate" sub-LLM calls (deferred).

## Architecture

```
+------------------------------------------------------------------+
| polymarket-pa/  (NEW, TS, Claude Agent SDK)                      |
|  - cycle scheduler  (1h baseline + event triggers)               |
|  - prompt assembly  (PA-adapted system prompt + dynamic ctx)     |
|  - tools/           (8 MVP tools: 6 daemon + web_search + notes) |
|  - notes store      (sqlite)                                     |
|  - event listener   (book moves >5pp, whale events, news hooks)  |
|  - safeguard hooks  (PreToolUse: bounds-check, sizing sanity)    |
+------------------------------------------------------------------+
                            | HTTP/JSON-RPC
                            v
+------------------------------------------------------------------+
| polymarket-pa-daemon/  (NEW, Rust binary)                        |
|  RPC: list_markets, get_market_book, get_portfolio,              |
|       place_order, cancel_order, get_recent_settlements,         |
|       get_open_orders, healthz                                   |
|  + dashboard (read-only HTTP for operator)                       |
|  + authoritative server-side risk gate (FAIL CLOSED)             |
+------------------------------------------------------------------+
                            | depends on
                            v
+------------------------------------------------------------------+
| polymarket-exec-core/  (NEW, Rust library, EXTRACTED)            |
|  wire/{clob_v2, user_ws, market_ws, relayer, execution_adapter,  |
|        live_auth}                                                |
|  core/{book, types, inventory, risk}                             |
|  runtime/{state machine, persistence, recon, lifecycle,          |
|           merge_redeem}                                          |
+------------------------------------------------------------------+
                            ^ depends on (existing unchanged)
                            |
+------------------------------------------------------------------+
| polymarket-exec/  (EXISTING, refactored to thin binary)          |
|  + signals/, market_context, market_making/, strategy.rs         |
|  (unlawful_shear, goat_pair, noop). Behavior unchanged.          |
+------------------------------------------------------------------+
```

### Component responsibilities

**`polymarket-exec-core`** (new Rust library): venue-agnostic execution
primitives extracted from current `polymarket-exec`. No strategy logic, no
BTC-specific code, no `signals/`, no `market_context`, no `market_making/`,
no `strategy.rs`. Contains only:

- `wire/`: CLOB v2 client, user WS, market WS, relayer, execution adapter,
  live auth.
- `core/`: book, types, inventory, risk module (pure invariants: solvency,
  no negative cash, no negative position, valid market ID format).
- `runtime/`: state machine, persistence, recon, order lifecycle (open,
  rejected, cancelled, filled), merge / redeem.

**`polymarket-exec`** (existing crate, refactored): becomes a thin binary
depending on `polymarket-exec-core` plus its existing strategy-specific
modules (`signals/`, `market_context`, `market_making/`, `strategy.rs`).
Behavior must be unchanged after refactor; existing test suite is the gate.

**`polymarket-pa-daemon`** (new Rust binary): long-lived process exposing
HTTP/JSON-RPC for the TS agent. Owns market data subscriptions, portfolio
recon, order submission/cancel, journal, dashboard. Makes no decisions;
all logic comes from the TS agent. Hosts the authoritative server-side risk
gate (concentration cap, per-cycle spend cap, daily-loss kill switch,
slippage cap, resolution-near blackout) which fails closed: agent cannot
bypass.

**`polymarket-pa`** (new TS project, top-level dir): Claude Agent SDK loop.
Contains cycle scheduler, prompt assembly, tool definitions (each tool is a
thin client to either the daemon or external services), notes store
(sqlite), event-trigger listener. Owns all decision logic. PreToolUse
hooks validate tool-call arguments before reaching the daemon.

## Data flow (per cycle)

1. Cycle scheduler fires (cron 1h, or event listener trigger).
2. TS assembles user prompt from:
   - Top-N markets by volume (Polymarket Gamma API call from the daemon).
   - Current portfolio (`daemon.get_portfolio`).
   - Last 10 settlements + closed trades (`daemon.get_recent_settlements`).
   - Prior cycle's reasoning + agent notes (sqlite).
   - System prompt (PA-adapted trading playbook).
3. Claude Agent SDK runs the cycle with 8 MVP tools (see Tool Inventory).
   Typical agent path: `search_markets` -> `get_market_book` on 1-3
   candidates -> optional `web_search` -> `place_order` (or no-op).
4. Each `place_order` JSON-RPC call hits `polymarket-pa-daemon`. Daemon
   validates against authoritative risk gate. On pass, submits via
   `polymarket-exec-core` execution adapter (paper or live by config).
5. Fill events flow back via existing user WS plumbing -> daemon journal ->
   next cycle's prompt context.
6. Cycle ends; agent's reasoning + decisions persisted to notes/journal.

## Tool inventory

### MVP tools (8)

Tools that talk to the daemon over JSON-RPC:

1. `search_markets({query?, category?, min_volume?, resolves_within?})`
   returns top-N matching markets with current bid/ask/spread/volume/
   resolves-at.
2. `get_market_book({market_id})` returns top 5 levels each side (better
   than PA's bid/ask only).
3. `get_portfolio()` returns positions (with cost basis), cash, unrealized
   PnL by position.
4. `place_order({market_id, side, size, limit_price})` submits an
   immediate-fill order. Side is one of `buy_yes`, `sell_yes`, `buy_no`,
   `sell_no`. Polymarket CLOB supports direct sells (unlike Kalshi's
   reciprocal-netting-only model that PA uses). The existing
   merge / redeem path in `polymarket-exec-core` is reused for closing
   paired inventory when an explicit sell is unavailable due to
   liquidity.
5. `cancel_order({order_id})` for cleanup.
6. `get_recent_settlements({limit?})` returns last N settled markets +
   realized PnL plus last N closed-by-netting trades.

External / local tools:

7. `web_search({query})` Anthropic web search tool.
8. `manage_notes({op, key?, value?, query?})` persistent memory across
   cycles, sqlite-backed. Capped at 50 entries, ~1200 chars each (matches PA).

### Deferred to v2 (in priority order)

- `wallet_research({market_id_or_condition_id, top_n?})` returns top-N
  wallets by position size in the market and a summary of their position
  history across other markets ("what does this whale's portfolio say
  about their thesis?"). Highest-value v2 add.
- `resolution_rules({market_id})` Polymarket resolution criteria text plus
  active UMA disputes. Critical for avoiding ambiguous markets.
- `edge_calculator({p_estimate, market_price, current_position, bankroll})`
  returns Kelly fraction, fractional Kelly (1/4 default), max-allowed size
  after risk caps. Forces explicit edge articulation; deterministic sizing.
- `whale_activity_feed({since_ts, min_size_usd?})` recent large entries /
  exits across the watchlist.
- `cross_market_correlation({market_id})` correlated markets pricing
  differently (arb signal).
- `price_analysis({asset_or_market})` external spot / feed data + recent
  levels for crypto-event markets.
- `review_past_decisions({lookback_days})` structured fetch of past N
  decisions + outcomes grouped by category.
- `devils_advocate({thesis, market_id})` sub-Claude argues opposite side
  before large trades.

## Safeguards (defence in depth)

| Layer | Where | Enforces |
|-------|-------|----------|
| Agent reasoning | Claude system prompt | Articulates `p_estimate`, edge, sizing logic explicitly |
| TS PreToolUse hook | Claude Agent SDK | Bounds-check tool args (size > 0, price in [0.01, 0.99], market_id format valid) |
| Daemon RPC validator | Rust daemon | Concentration cap (15% per market, cost basis), per-cycle spend cap, daily-loss kill switch, slippage cap from touch, resolution-near blackout window |
| Execution-core risk | `polymarket-exec-core::core::risk` | Solvency, no negative cash / position, valid market ID, fee-aware solvency |

The daemon validator fails closed. Agent cannot bypass: RPC returns a
structured error, agent sees it and adapts (or no-ops the cycle).

## Error handling

- LLM tool errors: structured response back to agent. Agent retries or
  abandons within the cycle.
- Daemon RPC errors: TS retries with exponential backoff (3 attempts
  max). On total failure, cycle ends with logged error; next cycle proceeds
  normally.
- Polymarket venue errors: handled by `polymarket-exec-core` recon paths
  (already battle-tested in `unlawful_shear`).
- Cycle crash: scheduler catches, waits for next interval. Circuit breaker
  trips after N consecutive failures (default N=3) and pages the operator.
- LLM cost runaway: per-cycle token cap (100k input + 20k output by
  default). Hard kill if exceeded.
- Stale market data: daemon flags markets with last-update older than
  threshold; TS filters them from the prompt.

## Testing strategy

- **`polymarket-exec-core`**: existing unit tests migrate over. New tests
  for any extracted public boundaries.
- **`polymarket-exec` (existing strategies)**: existing test suite must
  pass unchanged after the refactor. This is the gate proving extraction
  was non-breaking.
- **`polymarket-pa-daemon`**: contract tests against mock CLOB + Data API
  fixtures (reuse `polymarket-exec-core` test fixtures).
- **TS agent**: unit tests for prompt assembly + each tool wrapper.
  Integration test runs one full cycle against a mocked daemon.
- **E2E paper**: 3-5 day paper run is the integration test for the agent
  loop itself. Success = zero crashes, every tool path exercised, no
  nonsense orders, agent reasoning logs read sensibly, bounded cost
  per cycle.

## Phased build sequence

1. **Extract `polymarket-exec-core`**. Refactor; existing test suite
   gates the merge. Pure code-organization change.
2. **Build `polymarket-pa-daemon` skeleton**. Stubbed RPC handlers,
   contract tests against mock fixtures.
3. **Build TS agent loop**. Mock daemon. Full cycle integration test
   green.
4. **Wire real daemon**. Run one supervised cycle in paper mode;
   eyeball reasoning + tool calls.
5. **Run 3-5 day paper validation**. Small fixed watchlist (~30
   markets, mixed timescales). Operational shakedown only, not PnL signal.
6. **Promote to tiny-live** ($200-500). Iteratively add alpha tools
   (`wallet_research` first; biggest unique edge).

## Default knobs (MVP)

- Concentration cap: **15%** of bankroll per market (matches PA).
- Per-cycle spend cap: **$100** in paper, **5% of bankroll** in live.
- Slippage cap: **3 cents** from touch (~50 bps on a $0.50 market).
- Daily loss kill: **-10% of bankroll**, requires operator reset.
- Cycle cadence: **1 hour** baseline. Event triggers: book move >5pp on
  watchlist OR whale entry >$25k OR news webhook.
- Watchlist size: **30 markets** for MVP (fits one prompt without truncation).
- LLM: **Claude Sonnet 4.6**. Revisit Opus 4.7 with extended thinking if
  Sonnet underperforms on settled-PnL.
- Token cap per cycle: **100k input + 20k output**.
- Notes store: **sqlite**, 50 entries, 1200 chars each.

## Open questions / followups

- Capital allocation across the new sleeve and existing `unlawful_shear`
  inventory: separate Polymarket proxy wallet per sleeve, or shared wallet
  with virtual sub-bankrolls? **Defer to implementation plan.**
- Dashboard: extend existing `polymarket-exec` dashboard with a PA tab, or
  separate read-only HTTP on the daemon? **Defer to implementation plan.**
- Event-trigger source for "news webhooks": pick a provider (Tavily news,
  Exa, custom RSS aggregator) once we observe what the agent actually
  looks for in `web_search`. **Defer to v2.**

## Non-goals

- Replacing `unlawful_shear`. PA-style sleeve runs alongside.
- Beating PA's leaderboard. Goal is real Polymarket alpha, not benchmark.
- General-purpose multi-agent orchestration. One agent, one role.
- Sophisticated execution (TWAP, iceberg, resting orders). Immediate-fill
  only for MVP.
