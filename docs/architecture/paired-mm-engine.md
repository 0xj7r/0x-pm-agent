# Paired MM engine architecture

## Decision

The Polymarket timed-binary execution stack separates three concepts:

1. `markets/*`: market descriptors. BTC 5m, BTC 15m, ETH 5m, and ETH 15m
   describe what is traded.
2. `market_making/paired_mm/*`: the reusable paired market-making algorithm.
   This is the strategy brain: ladder generation, Stoikov skew, pair-cost
   accounting, merge policy, rescue EV, and hard paired-MM policy.
3. `execution/*` and `runtime/*`: venue adapters, reconciliation, risk approval,
   persistence, and order submission.

`btc_5m_mm` is therefore not the strategy. It is the current market/profile
that applies the paired-MM strategy to BTC 5m Up/Down markets.

## Ladder invariant

The paired entry ladder only emits maker BUY intents on both legs. It never
posts sells. Selling, buying the opposite leg for merge, redeeming, and merging
are close-side actions owned by rescue/merge modules.

The ladder is signal-driven:

- anchor: Stoikov reservation price,
- shape: regime, visible book depth, time remaining, and inventory imbalance,
- size: fractional-Kelly-style base clip with hard caps,
- final prefilter: gross-cost and entry-notional caps.

## Rescue invariant

Rescue is EV-first and close-side only.

- Hold value for a stranded leg is its model win probability.
- Sell rescue is valid when best exit bid beats hold value after buffer.
- Buy-opposite rescue is valid when stranded average cost plus opposite ask plus
  fees is below one after buffer.
- Pair-cost below one is a merge/convexity signal, not a global prerequisite for
  every rescue type.

## Fill automation invariant

Every fill updates automatic paired-MM state before new entries are considered.
The fill automation path has two phases:

1. `record_fill_by_leg`: update FIFO lot state and asymmetric-fill cooldown.
2. `evaluate_snapshot`: when a paired book snapshot is available, decide whether
   the one-sided fill should be held, rescued by buying the opposite leg, or
   rescued by selling the stranded leg.

Fill automation does not place orders. It returns typed rescue suggestions. The
concrete strategy still owns venue-specific order construction through
`RescueIntentBuilder`, including CLOB depth walking, race-buffer ticks, and
IOC/FAK semantics.

The immediate effect in the legacy BTC 5m path is:

- fills are recorded into the new paired-MM auto-fill state,
- stranded-exposure EV uses the new rescue engine,
- the existing venue-safe rescue intent builders remain the actuator.

## Volatility and whipsaw invariant

High realized vol and extreme short-horizon BTC returns suppress new paired
entry. Close-side operations remain available:

- merge can still recycle already-balanced inventory,
- capital recycle can buy the light side when pair cost is favorable,
- rescue can unwind stranded exposure when EV beats hold.

This prevents whipsaw regimes from repeatedly filling fresh ladders while still
allowing the engine to reduce existing risk.

## Data and calibration invariant

Every production decision should be replayable from durable rows:

- order intents,
- fills,
- book levels,
- spot ticks,
- journal events,
- operator calibration rows.

The code writes through `data::sink::DataSink`. Local runs can use JSONL; the
AWS path should implement the same interface with buffered Parquet files on S3
partitioned by dataset, market, date, and hour.

## Merge invariant

Merge is evaluated before new entry. It is preferred when paired quantity,
notional, expected gain, and gas/batching constraints pass.

## Capital recycling invariant

Merge is the routine capital-recycling operation in normal paired-MM flow, not
an occasional cleanup. When inventory is mildly imbalanced and there is enough
time left in the bar, the engine may buy the light side if projected pair cost
remains favorable:

```text
projected_pair_cost = heavy_side_avg_cost + light_side_buy_price + fees
```

This is a normal `CapitalRecycle` decision, not a defensive `Rescue`. The
expected next action after light-side inventory is acquired is merge.

## Hold-vs-rescue EV invariant

Sell rescue uses forward-looking EV only. Sunk entry cost is not part of the
sell-vs-hold decision:

```text
hold_value_per_share = P_model(win)
sell_value_per_share = best_exit_bid
sell_rescue_delta = sell_value_per_share - hold_value_per_share - buffer
```

Sell only when the delta is positive beyond the configured buffer. Buy-opposite
for merge is evaluated by projected pair cost, not by the sell-rescue formula.

## Hard risk boundary

Strategy modules propose typed decisions. Runtime/account risk remains the hard
boundary before venue submission. Strategy code must not bypass cash, open
order, gross exposure, side-imbalance, or drift controls.

## Polygon RPC invariant

Most trading actions use the off-chain CLOB, but EOA merge/redeem/wrap actions
are Polygon transactions. Polygon RPC is therefore production infrastructure,
not a launcher concern.

- `POLYGON_RPC_URL` is the primary endpoint and the only endpoint used for
  `eth_sendTransaction`. This avoids duplicate broadcasts when a provider times
  out after accepting a transaction.
- `POLYGON_RPC_FAILOVER_URLS` is a comma-separated list used for read/preflight
  calls such as `eth_call` and `eth_blockNumber`.
- `POLYGON_RPC_REQUEST_TIMEOUT_MS` controls preflight latency budget.
- `POLYGON_RPC_RECEIPT_TIMEOUT_MS` controls direct EOA CTF receipt wait time.
- `POLYGON_RPC_GAS_LIMIT` controls the direct EOA contract-call gas limit.

Provider URLs are redacted before logging so API keys embedded in URLs do not
leak to journals or CloudWatch.

## Migration plan

1. Add market descriptors, paired-MM modules, strategy registry, and infra/data
   seams.
2. Route the legacy BTC 5m paired ladder through `paired_mm::ladder_builder`
   while preserving runtime submission and reconciliation.
3. Move rescue construction behind `RescueIntentBuilder`.
4. Move the remaining BTC 5m state machine from `strategy.rs` into focused
   modules.
5. Delete legacy string-prefix decision classification once typed decisions are
   the only path.
