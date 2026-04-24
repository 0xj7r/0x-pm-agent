# Polymarket Execution Engine Handoff

Status: source-of-truth handoff as of 2026-04-24.

This document defines the behavior we expect from the Rust execution engine,
the venue mechanics it must respect, the live incidents already observed, and
the checks another agent should use to sense-check future work.

## Operating Principle

The engine must be venue-native. Strategy logic is only valid if local state,
venue state, and wallet/account state agree.

For live or tiny-live trading, the engine must never assume:

- a submitted order is working until the venue confirms it;
- a cancelled order is unfilled until user trades and positions are reconciled;
- local inventory is flat just because the local order store has no active
  order;
- a fill is ours unless the user websocket or filtered maker/taker REST trade
  proves it;
- min order size, tick size, token mapping, or fees can be hardcoded forever.

If venue truth and local truth diverge, the engine must fail closed: cancel
known working orders, block fresh exposure in the affected market, reconcile,
and only resume once the state is understood.

## Product Goal

We are building a BTC 5m Polymarket market-making/execution system that can run
paper, sandbox-like, tiny-live, and eventually larger live sleeves.

The near-term strategy shape is:

- maker-first, post-only quoting;
- paired or paired-intent entry across complementary Up/Down tokens;
- small live probes while execution quality is being validated;
- dynamic sizing based on venue constraints, liquidity, inventory, and risk;
- buy/merge preferred over sell-based churn where paired inventory exists;
- strict reconciliation against order, fill, cash, and position truth;
- continuous comparison to observed whale behavior, especially unlawful-shear.

The goal is not to blindly clone a wallet. The goal is to rebuild a robust,
venue-aware engine that can express similar market-making behavior while making
its own decisions from observable microstructure and risk state.

## Current Immediate Priority

Deprioritize third-party simulator/sandbox work for now.

Do not connect the real wallet to third-party simulators. If external sandbox
testing returns later, it must use either API-only access or a burner wallet
with no funds, no production approvals, and no connection to the production
Polymarket account.

The immediate priority is live-runtime correctness:

1. CLOB V2 readiness.
2. User websocket and REST trade reconciliation correctness.
3. Venue-authoritative position reconciliation.
4. Dynamic venue-metadata-driven sizing.
5. Paired-entry and one-leg rescue behavior.
6. Explicit merge/redeem inventory mechanics.
7. Maker/taker and rebate attribution.

## Venue Mechanics We Must Respect

### API Surfaces

Polymarket has three distinct surfaces that must not be blurred:

- Gamma API: market/event discovery and metadata.
- Data API: user positions, trades, activity, holder data, PnL, profiles.
- CLOB API: orderbook, pricing, order placement, cancellation, open orders,
  trades, balances, and trading operations.

Public market data does not prove account state. Account state must come from
authenticated user WS, authenticated CLOB queries, Data API positions, or
onchain/relayer inventory operations.

### Authentication

The live path uses:

- L1 private-key signing for local order signing and deriving API credentials.
- L2 API credentials for order management, user WS, cancellations, open orders,
  and authenticated CLOB requests.
- `POLYMARKET_SIGNATURE_TYPE` for EOA/proxy/Safe behavior.
- `POLYMARKET_FUNDER_ADDRESS` where the funder/profile wallet differs from the
  signer.

Startup must log signer address, funder address, signature type, and CLOB URL,
but never secrets.

### CLOB V2

Polymarket CLOB V2 cutover is a hard compatibility risk. V2 changes exchange
contracts, collateral semantics, fee handling, order payload fields, and SDK
expectations.

Before any live run after the cutover:

- verify the Rust `polymarket-client-sdk` version supports V2;
- verify order signing uses V2 fields and exchange domain;
- verify fee handling is venue/SDK-driven, not static;
- verify pUSD/USDC.e assumptions in docs and inventory operations;
- run a no-trade auth/reconcile probe against the active CLOB URL.

If V2 compatibility is uncertain, live trading stays off.

### Orders

All order lifecycle states must be explicit.

Expected behavior:

- Submit post-only maker bids only when price does not cross the current book.
- Use GTD/GTC according to venue support and internal TTL.
- Treat venue submit timeout/uncertain outcome as `NeedsReconcile`.
- Do not retry blindly; reconcile first if venue outcome is unknown.
- Do not combine post-only with marketable/FOK/FAK semantics.
- On post-only cross rejection, reprice once only if the book still supports
  maker edge; otherwise suppress.
- On cancel ack, keep the order/fill window open long enough to catch late
  fills.
- On late fill for an order local state marked cancelled/rejected, correct the
  terminal state and apply inventory.

### User Websocket

Authenticated user WS should become the primary lifecycle truth.

Expected behavior:

- Parse order events for placement/update/cancellation.
- Parse trade events as immediate fill evidence.
- Treat `MATCHED` as execution evidence; later mining/confirmation status is
  settlement finality, not the first inventory signal.
- Resolve fills by venue order id where client order id is absent.
- Continue REST backfill if user WS disconnects or lags.

### REST Trades

Never use broad/unfiltered CLOB trades as account fill truth.

Expected behavior:

- Query maker-address trades for the actual funder/trading address.
- Query taker-address trades for the same address.
- Filter maker orders inside returned trades to our address.
- Deduplicate by venue order id, market id, asset id, side, price, quantity,
  and timestamp/match id where available.
- Ignore unrelated market trades.

### Positions

Cash-only balance sync is insufficient.

Expected behavior:

- Reconcile venue/account positions by market and token.
- Compare venue positions against local inventory every reconcile cycle.
- If local says flat but venue has inventory, enter risk-off for that market.
- If local has paired inventory, evaluate merge.
- If local has stranded inventory, hedge/rescue according to strategy and risk.
- If position source is unavailable, do not treat missing data as flat.

### Inventory Operations

Split, merge, and redeem are first-class operations.

Expected behavior:

- Split creates equal complementary token inventory from collateral.
- Merge consumes equal YES/NO quantities and returns collateral.
- Redeem after resolution converts winning tokens to collateral and losing
  tokens to zero.
- Merge/redeem events must not be treated as normal buy/sell fills.
- Buy/merge should be preferred over sell cleanup when paired inventory can
  release capital with positive expected value.

## Strategy Behavior

### Paired Entry

Flat entry should be paired-or-nothing by default.

Expected behavior:

- If there is no existing inventory, submit both complementary bids together.
- If only one side can be quoted safely, do not create new flat-entry exposure
  unless an explicit single-leg mode is enabled.
- If one submit is accepted and the other is rejected/uncertain, either cancel
  the accepted side before fill or immediately reconcile and hedge/rescue.
- If one leg fills, the next decision must prioritize the opposite-leg hedge or
  merge path, not normal fresh entry.

### Dynamic Sizing

Sizing must be dynamic but explainable.

Inputs:

- venue minimum order size;
- venue tick size;
- available cash;
- reserved open-order exposure;
- max per-order and max gross limits;
- best bid/ask and spread;
- visible depth/notional;
- top-of-book imbalance;
- BTC movement and volatility;
- existing inventory and paired/stranded inventory;
- phase/window timing;
- strategy-specific target edge.

Avoid:

- unexplained constants like fixed `5.0` or `6.5` shares without logs;
- size multipliers that silently override configured dollar clips;
- hardcoded fee coefficients as live truth;
- submitting the same clip size regardless of price, liquidity, and inventory.

The engine can use defaults as fallbacks, but live decisions should log which
venue metadata and strategy parameters produced the final size.

### One-Leg Rescue

One-sided exposure is not automatically a bug. Unobserved or unintended
one-sided exposure is a bug.

Expected behavior:

- If one leg fills, mark inventory immediately.
- Keep useful opposite-side bids if they are still safe and maker-valid.
- If opposite bid is missing, submit a hedge/rescue order if edge/risk allows.
- If hedge cannot be submitted, mark the market risk-off or stranded.
- Do not let local state stay flat while UI/account shows a position.
- Do not sell by default if the intended play is buy/merge, unless configured as
  an explicit risk-reduction action.

### Exit, Merge, Redeem

The strategy should distinguish:

- normal maker entry;
- paired inventory eligible for merge;
- stranded inventory needing hedge/rescue;
- resolved market needing redeem;
- late-window risk reduction;
- emergency flatten/risk-off.

Selling losing inventory to avoid a -100% UI mark is not necessarily the right
behavior. The correct behavior depends on whether the position is paired,
mergeable, hedgeable, or genuinely stranded.

## Historical Issues Identified

### Unpaired Live Position

Observed:

- UI showed an unpaired `Down 80c x 6.5` BTC 5m position.
- Local state had no active open order and did not recognize corresponding
  inventory correctly.
- Venue cash dropped by the expected cost, proving this was not just a UI
  artifact.

Failure class:

- local order lifecycle and venue/account lifecycle diverged;
- cancel/reject state was interpreted too optimistically;
- fill sync was not authoritative enough;
- position sync was missing.

Required guard:

- apply late fills even after cancelled/rejected local state;
- reconcile filtered user trades and positions;
- block fresh entries if venue inventory exists and local inventory disagrees.

### Broad Trades Endpoint Contamination

Observed:

- A no-trade live reconcile probe returned many fills from a broad CLOB trades
  endpoint.
- Those trades were not necessarily ours.

Failure class:

- unfiltered trade sync can create false local fills or hide real missing fills.

Required guard:

- maker/taker address filters only;
- maker-order-level address filtering;
- dedupe and verify against venue order ids where possible.

### Fixed 5/6.5 Share Confusion

Observed:

- UI showed repeated 5-share and later 6.5-share trades.
- `6.5` came from `venue_min_order_quantity * entry_min_size_multiplier`.

Failure class:

- sizing was technically config-driven but not sufficiently transparent or
  venue-metadata-driven.

Required guard:

- live min size comes from venue metadata;
- entry multipliers are explicit and logged;
- final quantity logs include min-size floor, dollar clip, price, depth cap,
  and risk cap.

### One-Leg Live Quotes

Observed:

- We placed or ended up with one-sided positions when intent was paired
  market-making.

Failure class:

- strategy/reconciler did not enforce paired-or-nothing strongly enough across
  submit, cancel, fill, and reconcile races.

Required guard:

- both legs accepted or neither leg remains exposed at flat entry;
- one-leg fill triggers hedge/merge/rescue immediately;
- opposite useful bid must not be cancelled just because the quote reconciler
  recalculated normal desired quotes.

### Sell-Based Cleanup Drift

Observed:

- Some cleanup paths introduced sell/reduce-only behavior.
- Whale pattern appears more buy/merge-heavy than sell-heavy.

Failure class:

- execution cleanup can drift from intended strategy mechanics.

Required guard:

- model buy/merge as first-class;
- sell only as explicit configured risk reduction;
- distinguish merge/redeem from trade fills in accounting.

### Paper Fill Optimism

Observed:

- Paper produced many tiny partial fills and may not reflect real queue/fill
  quality.

Failure class:

- paper execution quality can be overstated if queue position, top-level size,
  maker/taker distinction, and post-only behavior are not modeled.

Required guard:

- paper fill model must be conservative;
- paper reports expected edge, realized edge, maker/taker assumption, and
  slippage;
- do not promote paper results to live without live smoke validation.

## What To Avoid

- Do not restart tiny-live after compile success alone.
- Do not treat no open orders as no exposure.
- Do not treat cancel ack as proof of no fill.
- Do not connect production wallet to third-party simulators.
- Do not keep adding static knobs instead of reading venue metadata.
- Do not let `NeedsReconcile` permanently block unrelated markets, but do not
  allow fresh exposure in the unresolved market.
- Do not run live through any path that bypasses signed CLOB SDK/API order
  construction.
- Do not let dashboard/UI PnL drive strategy decisions without local venue
  reconciliation evidence.
- Do not ignore CLOB V2 compatibility.

## Acceptance Checks For Future Agents

Any future agent changing execution/risk/strategy should answer these questions
before merging:

1. Can the engine prove whether each visible position is known locally?
2. Does the engine know whether each fill was maker, taker, merge, redeem, or
   unknown?
3. Can local flat and venue non-flat happen silently?
4. If one leg fills, what exact code path creates hedge/merge/rescue behavior?
5. If one leg is accepted and the other is rejected, what prevents naked
   exposure?
6. Where do min size, tick size, fee, and token metadata come from?
7. What happens if user WS drops for 30 seconds?
8. What happens if REST trades returns unrelated trades?
9. What happens if cancel ack arrives before fill event?
10. What happens if CLOB API request times out after the venue accepted it?
11. What happens at market close and after resolution?
12. What evidence proves CLOB V2 compatibility?
13. What test reproduces the most recent live incident?
14. What logs would explain the final submitted size and price?
15. Can the system stop trading without losing track of pending venue state?

If the answer to any of these is "not sure", the change is not live-ready.

## Minimum Live Readiness Bar

Before restarting tiny-live:

- CLOB V2 status verified.
- User WS connected and parsed.
- Filtered REST fill sync verified.
- Position sync implemented or an explicit safe substitute exists.
- Startup no-trade reconcile passes.
- Open orders, recent fills, cash, and positions are logged.
- Paired-entry invariant tests pass.
- Late-fill-after-cancel tests pass.
- One-leg rescue tests pass.
- Venue metadata-driven size/tick path exists.
- Kill switch is active.
- Rebate polling can be missing, but maker/taker fill attribution must exist.

Before scaling beyond tiny-live:

- merge/redeem implemented and tested;
- per-market audit bundle complete;
- conservative paper/live execution quality comparison available;
- whale-relative diagnostics available;
- maker rebate endpoint integrated;
- operational runbook tested on the deployment host.

## Useful Repo Pointers

- `polymarket-exec/src/wire/execution_adapter.rs`: live CLOB adapter, submit,
  cancel, open-order sync, balance sync, fill sync.
- `polymarket-exec/src/wire/user_ws.rs`: authenticated user websocket parsing.
- `polymarket-exec/src/runtime/runner.rs`: live/paper runner and reconciliation
  loop.
- `polymarket-exec/src/runtime/mod.rs`: runtime state machine, strategy
  acceptance, inventory updates.
- `polymarket-exec/src/runtime/order_store.rs`: persisted order lifecycle.
- `polymarket-exec/src/core/inventory.rs`: cash and position accounting.
- `polymarket-exec/src/market_making/pair_ledger.rs`: paired/stranded
  inventory and merge candidates.
- `polymarket-exec/src/market_making/merge_executor.rs`: local merge accounting.
- `polymarket-exec/src/strategy.rs`: strategy decisions and sizing.
- `polymarket-exec/env/btc_5m_mm_tinylive.env`: tiny-live config surface.
- `polymarket-exec/ops/systemd/common.env.example`: host-level operational
  defaults and endpoint configuration.

## Current Known Docs

- `polymarket-exec/docs/venue_mechanics_reset.md`: detailed venue/API reset
  checklist.
- `docs/architecture/2026-04-24-polymarket-venue-reset.md`: endpoint and
  venue-parity reset notes.
- `docs/architecture/2026-04-24-polymarket-sandbox-endpoint-runbook.md`:
  sandbox endpoint notes; currently deprioritized.
