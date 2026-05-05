# BTC 5m paired-MM live incident and repair plan - 2026-05-04

## Summary

On 2026-05-04 the BTC 5m paired-MM live sleeve lost materially more than expected for a tiny-live validation run. The loss was not explained by normal strategy variance alone. The run exposed a combination of hostile market regime, execution-state defects, and one strategy branch that did not fit the paired-MM design.

The intended strategy is paired market making with merge recycling and a small convex accumulation arm. It is not a sell-unwind strategy. The engine should prefer paired maker fills, buy the light side only when pair-cost is acceptable, merge paired inventory, and otherwise let residual one-sided inventory resolve rather than realizing losses through sells.

## Intended engine contract

1. Quote both legs only while inventory is balanced enough for paired entry.
2. If one leg fills, enter repair-first mode for that market.
3. In repair-first mode, do not add more of the already-heavy leg.
4. If the opposite leg can be bought at acceptable projected pair cost, emit a capital-recycle buy.
5. If only passive repair is acceptable, quote the light side only.
6. Merge as soon as paired inventory exists and venue/on-chain state allows it.
7. Never emit paired-MM sell rescue.
8. If pairing is not viable, hold residual exposure or let the market resolve.

## Observed live behavior

### Sell rescue branch caused direct strategy violation

A sell-rescue branch was added to paired-MM after stranded inventory appeared. In live execution it began realizing losses and also attempted repeated/oversized sells when venue state lagged local state. This turned a maker/merge strategy into an active unwind strategy.

The branch was removed from paired-MM and `sell_unwind_enabled` was disabled in the live YAML. The generic rescue math may still support sell fallback for other contexts, but paired-MM must not emit sell intents.

### Deployment race kept old sell-capable binary live

After the no-sell change was committed, an interrupted remote deploy left the live symlink pointing at an older sell-capable binary. The UI continued showing sells because the old binary was still running/restarted. This was an operational failure, not a strategy decision.

Corrective action: live deploys must not leave an auto-restarting wrapper in the background, and the active binary symlink must be verified before restarting the service.

### Maker fills were real but adverse

Logs showed paired ladders being generated on both legs and maker orders resting long enough to fill. Merges were observed after both YES and NO inventory existed. However, fills were often one-sided during BTC whipsaws, leaving stranded inventory that later resolved against us.

Maker status alone is insufficient if quotes are stale or if the engine keeps adding inventory on the leg being picked off.

### Merge path worked but did not cover one-sided bleed

The engine successfully submitted multiple merges after paired inventory formed. This validates the basic merge path. It does not solve one-sided accumulation. Merge can only recover inventory that exists on both legs.

One merge transaction reverted. The runtime blocked an identical duplicate without global risk-off, which is directionally correct, but merge/reconcile state remains an area for hardening.

### Stranded inventory appeared repeatedly

Venue reconciliation repeatedly reported stranded inventory. Some repetition is expected while a market is open, but the frequency showed that paired-MM was spending too much time with one-sided exposure and not enough time in a clean repair-first state.

### Quote lifecycle is too crude

The current quote lifecycle relies heavily on fixed max age and suppression cancels. This can churn orders in stable markets and leave stale quotes live during whipsaw. The correct policy should preserve quotes while edge is still valid and cancel quickly when BTC/book/fair value invalidate them.

## Bugs or defects identified

1. Paired-MM sell-rescue emission existed and should not have.
2. Runtime did not initially guard reduce-only sell reservations against already-open sell reservations.
3. Interrupted deploy could leave a stale binary active or restart it later.
4. Paired-MM could continue emitting fresh same-leg bids while inventory was already one-sided.
5. Repair mode was implicit, not an explicit market state.
6. Merge/reconcile latch can still be confusing after reverted or mined transactions.
7. Logs showed decision labels, but PnL attribution by market was not yet good enough to explain loss quickly.

## Fixes already attempted

1. Added reduce-only sell reservation guard in runtime acceptance.
2. Removed paired-MM sell-rescue intent emission.
3. Disabled `rescue.sell_unwind_enabled` in the paired-MM live profile.
4. Added signal engines for momentum/order-book pressure/cheap-leg decisions in earlier work.
5. Added fair-value anchoring to keep model fair from dragging quotes too far from book mid.
6. Added/kept merge-first behavior when paired inventory is available.
7. Added repair-first filtering so fresh paired-MM quotes cannot add more of the already-heavy leg once inventory imbalance is meaningful.
8. Added book-aware stale live order handling: aged orders are preserved only if the current book still supports them as maker-safe quotes; otherwise they are cancelled with a book-derived reason.

## Fixes required before next live deployment

1. Same-leg accumulation brake.
   Implemented at the strategy adapter seam: when inventory is meaningfully one-sided, fresh entry intents on the heavy leg are removed.

2. Repair-before-fresh-entry mode.
   Partially implemented as repair-first filtering. A fuller runtime market-state model should make this an explicit state instead of deriving it each tick.

3. Signal-aware quote lifecycle.
   Partially implemented with book-aware stale order cancellation. Remaining work: include BTC move and fair-value drift, not just top-of-book position.

4. Merge/reconcile hardening.
   A mined merge should clear pending merge after venue reconciliation. A reverted merge should block only the exact duplicate and should not poison future valid merges.

5. Per-market PnL attribution.
   Every market should report fills, average costs, merges, residual inventory, rebate estimate, realized result, and mark/resolution result.

6. Backtest/replay acceptance tests.
   Replay should assert no sells, no same-leg over-accumulation, merge when paired inventory exists, and quote cancellation in whipsaw.

## Backtesting requirements

The backtester must validate strategy behavior, not only final PnL.

Required metrics:

- gross buy notional by market and side
- fill count by side
- maker/taker inference where possible
- average fill price by side
- paired quantity acquired
- merge quantity and timing
- residual stranded quantity
- residual outcome value
- estimated rebate fees
- order lifetime distribution
- cancel reason distribution
- same-leg accumulation count
- time spent in balanced quoting vs repair mode
- quote edge at placement and at fill
- BTC move after quote placement
- quote adverse-selection score

Required assertions:

- paired-MM emits no sell orders
- paired-MM does not add heavy-leg exposure in repair mode
- light-side repair is attempted only when projected pair cost is acceptable
- fresh paired ladders resume after inventory is balanced or market rolls
- merge is attempted when paired inventory exceeds minimum merge quantity
- failed merge blocks exact duplicate only
- stale quotes cancel when BTC/book moves materially
- stable quotes are not churned unnecessarily

New local attribution tool:

```bash
python3 scripts/analyze_paired_mm_incident.py \
  --db /path/to/orders.sqlite \
  --pretty
```

This script summarizes submitted/filled orders by market and flags sell activity and same-leg accumulation risk. It is intentionally read-only and should be used after live/paper runs to compare behavior before and after repair-first changes.

## AWS event persistence requirement

The local JSONL journal and SQLite order store are useful during live debugging, but they are not sufficient as the long-term source of truth. We need an append-only AWS event lake for all runtime events, commands, fills, order lifecycle updates, merge/redeem attempts, and strategy decisions.

Recommended shape:

1. Runtime emits the canonical event stream already written to `JournalWriter`.
2. The same event stream is teed to Firehose.
3. Firehose writes partitioned S3/Parquet.
4. Glue/Athena provides the queryable database layer.
5. Optional DynamoDB/Postgres tables hold only recent operational state and rollups, not the full raw event stream.

Why not RDS-first:

- trading events are append-only and high-volume;
- replay/backtest wants immutable history more than transactional updates;
- S3/Parquet is cheaper and easier to retain;
- Athena can answer market/day/run incident questions without loading the live engine.

Minimum event schema:

- `run_id`
- `strategy`
- `market_id`
- `condition_id`
- `instrument_id`
- `event_type`
- `client_order_id`
- `venue_order_id`
- `side`
- `price`
- `quantity`
- `notional_usd`
- `quote_level_tag`
- `decision_label`
- `runtime_status`
- `btc_spot`
- `btc_regime`
- `book_bid`
- `book_ask`
- `book_mid`
- `book_age_ms`
- `reason`
- `ts_ms`

This is a deployment/infrastructure requirement before scaled live trading. The engine should own the emitted event schema; AWS is only the durable sink/query layer.

## Deployment rule after this incident

Do not restart live paired-MM at production clip sizes until replay/backtest shows that the same-leg brake and repair-first mode would have materially reduced the 2026-05-04 one-sided bleed.
