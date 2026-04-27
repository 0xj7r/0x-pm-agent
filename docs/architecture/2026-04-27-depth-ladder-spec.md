# Depth-Ladder Quoting (Task #51)

## Problem

We currently emit ONE post-only paired bid per leg per tick. Single price
level. Whales like `unlawful_shear` emit **3-5 price levels per leg**
(verified: `attempt-1777278786` had 10 trades same second across Up at
$0.62/$0.63 and Down at $0.36/$0.37/$0.39 from a single retail market
order sweeping their resting depth).

Result: we miss most of the multi-level taker flow. Every market sweep
that crosses 3 of unlawful's price levels nets him 3 fills + 3 rebates
where we get 0 (we weren't quoting at the deeper level).

## Goal

Place N (default 3) post-only orders per leg at progressive price levels:

```
Level 0 (top of book):     50% of clip @ best_bid
Level 1 (1 tick deeper):   30% of clip @ best_bid - 1 tick
Level 2 (2 ticks deeper):  20% of clip @ best_bid - 2 ticks
```

A single market order that sweeps 1 tick = fills our top level.
Sweep 2 ticks = fills 2 levels. Sweep 3 ticks = fills 3 levels.
**Per-market fill density goes from 1× to 3×.**

## Implementation

### New config

```rust
pub struct Btc5mMmConfig {
    // ... existing ...
    /// Number of price levels per leg in the depth ladder. 1 = current
    /// behavior (backwards compat). Recommend 3.
    pub ladder_levels: usize,
    /// Size weighting per level, summing to 1.0. Front-loaded by default
    /// since top-of-book is most likely to fill. e.g. [0.5, 0.3, 0.2].
    /// Length must equal ladder_levels.
    pub ladder_size_weights: Vec<f64>,
}
```

Defaults: `ladder_levels = 3`, weights = `[0.5, 0.3, 0.2]`.

### New helper

Replace single-bid build with ladder fan-out:

```rust
fn build_ladder_intents_for_leg(
    &self,
    market_id: &MarketId,
    instrument_id: &InstrumentId,
    quote: &QuoteSnapshot,
    fair: f64,
    leg_cost: f64,
    gross_cost: f64,
    total_quantity: f64,  // total clip across all levels
    edge_bps: f64,
    venue_rules: Option<&VenueMarketRules>,
    quote_level_tag_prefix: &str,
    reason_prefix: &str,
    pair_id: &str,
    now_ms: EpochMillis,
) -> Vec<OrderIntent>
```

Returns 0..N intents (some levels may not satisfy venue_min_order_size
after weight-split; skip those rather than fail the whole leg).

Per-level logic:
1. Compute level price: `top_price - i * tick`
2. Compute level qty: `total_quantity * weights[i]`
3. Skip if `level_qty < venue_min_order_size` (can't fit)
4. Build OrderIntent with `quote_level_tag = format!("{prefix}-l{i}")` so
   the quote_reconciler treats each level as a distinct line item
5. All intents share the same `pair_id`

### Replace call site

In `on_market_snapshot::(false, false)` paired-entry branch, replace
the two `build_bid_intent_for_quantity` calls with:

```rust
let left_ladder = self.build_ladder_intents_for_leg(...);
let right_ladder = self.build_ladder_intents_for_leg(...);
intents.extend(left_ladder);
intents.extend(right_ladder);
```

Each emitted intent inherits `IntentKind::Entry` (no rescue bypass).

### Risk caps interaction

- `max_leg_cost_usd` and `max_gross_cost_usd` apply to TOTAL across all
  levels, not per level. Sum the level notionals before calling
  build_bid_intent.
- `max_open_orders_per_market` may need bumping (3 levels × 2 legs = 6
  open orders per market vs current 2).
- `max_submit_per_window` rate cap: 6 fresh submits per pair-attempt
  cycle; current cap is `max_submit_per_window=6` so we can do 1
  pair-attempt per window. Probably needs bumping to 12-18.

### Quote reconciler interaction

Already keys on `(market, instrument, side, reduce_only, quote_level_tag)`.
Different `level_tag` per level = each level treated as independent. No
churn between levels because each level's quote price is stable.

When the strategy re-emits the ladder on a fresh tick, the reconciler
will Keep existing-level intents (same price/qty) and only Submit any
new levels. Replace happens when book moves (level price changes).

### Test plan

1. Unit test: 3-level ladder with $5 clip + 0.5/0.3/0.2 weights at
   best_bid=$0.49 → 3 intents at $0.49 (qty=2.5), $0.48 (1.5), $0.47 (1.0).
2. Unit test: weights that produce sub-min levels → those levels are
   skipped, top remains.
3. Unit test: ladder + gross cap = top level gets full clip if cap
   would otherwise be violated by deeper levels.
4. Live: deploy with `ladder_levels=3`, observe (a) more fills per
   sweep, (b) trade history shows multiple `:l0/:l1/:l2` levels filling
   simultaneously, (c) bumped `max_submit_per_window` doesn't blow rate
   limit.

### Rollout

1. Default `ladder_levels = 1` initially (no behavior change).
2. Ship + verify ladder produces correct intents in tests.
3. Bump deploy script: `WHALE_PAIR_BTC_5M_MM_LADDER_LEVELS=3`.
4. Observe 1h. If post-only crosses-book rate stays low, leave at 3.
5. Try `ladder_levels=5` for more aggressive depth.

## Expected impact

- **Per-market fill rate**: 2-3x (sweeps catch multiple levels)
- **Rebate accrual**: scales with maker volume → 2-3x rebates
- **Capital utilization**: more dollars sitting in the book per market →
  more pulled at any sweep
- **Risk**: more fills concentrated at sweep events. Stranded inventory
  from one-leg-fills also scales (need rescue path operational, which
  it now is post drift-bypass fix).

## Estimated complexity

- `build_ladder_intents_for_leg`: ~50 LOC
- Config additions: ~20 LOC
- Caller refactor: ~30 LOC
- Tests: ~100 LOC
- Total: ~200 LOC, single file (strategy.rs), no cross-cutting changes.

## Out of scope (future)

- **Adaptive ladder depth**: scale ladder_levels by recent vol or fill rate
- **Adaptive size weights**: Bayesian update per-level fill probability
- **Multi-tick spacing**: skip tick levels that have low book depth
