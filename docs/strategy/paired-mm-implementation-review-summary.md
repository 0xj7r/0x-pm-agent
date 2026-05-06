# Paired-MM implementation review summary

Date: 2026-05-06
Branch: `feat/btc-5m-side-score-signals`
Worktree: `/private/tmp/polymarket-agent-side-score`

This document is a review handoff for the current BTC 5m paired market-making
implementation in `polymarket-exec`.

## 1. Strategy overview

The active strategy is `paired_mm` for Polymarket BTC 5-minute binary markets.
It is not a pure directional strategy and it is not primarily `pair_cost_arb`.

The strategy has two complementary entry modes:

1. Paired market making
   - Posts passive buy ladders on both outcome legs.
   - Attempts to earn maker edge through spread, rebates, and later merge/redeem.
   - Keeps the normal ladder mostly two-sided, with bounded signal-driven sizing tilt.

2. Late convex overlay
   - In the late part of a 5-minute bar, when one side is a strong favourite,
     it can load the favourite side and also buy ultra-cheap tail shares.
   - This mirrors the observed Bonereaper-style shape: favourite notional loading
     plus cheap-tail share-count convexity.
   - It remains risk-budgeted and bounded by max loss, book take percentage, and
     active-order limits.

The strategy intentionally avoids SELL unwind by default. Existing stranded
inventory should be handled through light-side repair, merge batching, redeem,
or hold-to-resolution decisions rather than sell loops.

## 2. Core runtime shape

Main strategy adapter:

- `polymarket-exec/src/strategies/paired_mm.rs`

Deep paired-MM facade:

- `polymarket-exec/src/market_making/paired_mm/engine.rs`

Paired ladder builder:

- `polymarket-exec/src/market_making/paired_mm/ladder_builder.rs`

The strategy adapter receives a `StrategyInput` containing:

- market descriptor
- paired market snapshot
- inventory snapshot
- open convex exposure
- pair-cost tracker
- fair-value estimate
- BTC regime
- BTC momentum
- order-book pressure
- current timestamp

`PairedMmEngine::decide` then evaluates:

1. hard policy
2. merge policy
3. capital recycle
4. rescue inputs
5. new reversal/book-sanity/side-score signals
6. normal paired ladder
7. late convex overlay
8. optional ladder suppression if late package fires

## 3. Key signals

### 3.1 Fair value

File:

- `polymarket-exec/src/signals/fair_value.rs`

Purpose:

- Estimate the physical probability that BTC finishes above or below strike.
- Outputs `FairValueEstimate` with `p_up`, `p_down`, `log_moneyness`,
  `sigma_remaining`, and `time_remaining_s`.

Usage:

- Anchors paired-MM reservation prices.
- Selects late convex favourite side.
- Feeds side-score fair-value edge.
- Feeds reversal distance-to-strike component.

Important review point:

- Fair value is bounded by book-mid anchoring in the paired ladder so the normal
  MM path does not become an unconstrained directional trader.

### 3.2 BTC momentum

File:

- `polymarket-exec/src/signals/momentum.rs`

Purpose:

- Compute multi-window BTC momentum with exponential decay.

Current output:

- `direction`
- `score`
- `strength`
- `latest_window_return_bps`
- `acceleration_bps`
- `window_returns_bps`

Usage:

- Existing ladder clip scale.
- Reversal deceleration and sign-flip components.
- Side-score momentum component.

Review point:

- `MomentumEngine` already implements the multi-window decay pattern, so this
  branch does not create a duplicate momentum engine.

### 3.3 Order-book pressure

File:

- `polymarket-exec/src/signals/order_book_pressure.rs`

Purpose:

- Summarize Polymarket book/taker-flow pressure between the two legs.

Current output:

- direction
- imbalance
- visible bid/ask notional per leg
- recent taker buy/sell quantities
- thin-book flag

Usage:

- Existing ladder clip scale.
- Late convex pressure bias.
- Reversal orderflow-against component.
- Side-score orderflow component.

### 3.4 Reversal signal

File:

- `polymarket-exec/src/signals/reversal.rs`

Purpose:

- Estimate whether the current fair-value favourite is vulnerable to reversal.

Formula:

```text
reversal_probability =
  deceleration_weight * deceleration_score
  + momentum_flip_weight * momentum_flip_score
  + distance_weight * distance_to_strike_score
  + orderflow_weight * orderflow_against_score
```

The weighted sum is normalized by total positive weight and clamped to `[0, 1]`.

Components:

- `deceleration_score`: previous momentum was strong and current absolute move
  has slowed.
- `momentum_flip_score`: latest and prior return windows have opposite signs.
- `distance_to_strike_score`: BTC is close to strike relative to
  `sigma_remaining`.
- `orderflow_against_score`: order-book pressure is against the current
  favourite.

Usage:

- Does not emit orders.
- Feeds side score as support for the reversal leg and penalty for the current
  favourite.
- Exposed in strategy notes for replay/backtest review.

### 3.5 Book sanity signal

File:

- `polymarket-exec/src/signals/book_sanity.rs`

Purpose:

- Convert raw book quality into per-leg soft penalties.

Inputs:

- best bid/ask
- spread
- top depth notional
- same-side queue notional
- opposite-leg ask for projected pair-cost sanity
- observed timestamps

Penalties:

- missing BBO
- crossed book
- stale quote
- wide spread
- thin top depth
- large queue depth
- poor projected pair cost

Usage:

- Feeds side-score book-sanity penalty.
- Exposed in strategy notes as `book_penalty_yes/no`.

Meaning of "book sanity":

- A book is sane when both legs have real BBO, non-absurd spread, enough top
  depth, non-stale observations, non-crossed prices, and a pair-cost context that
  does not imply we are quoting into obvious bad structure.

### 3.6 Side score

File:

- `polymarket-exec/src/signals/side_score.rs`

Purpose:

- Combine fair value, momentum, orderflow, terminal timing, reversal risk, and
  book sanity into bounded per-leg sizing multipliers.

Formula:

```text
side_score(leg) =
  fair_value_weight * fair_value_component
  + momentum_weight * momentum_component
  + orderflow_weight * orderflow_component
  + terminal_timing_weight * terminal_timing_component
  + reversal_risk_weight * reversal_component
  - book_sanity_weight * book_sanity_penalty
```

The score is normalized by total positive weights and clamped to `[-1, 1]`.

Outputs per leg:

- `score`
- component diagnostics
- `ladder_clip_scale`
- `late_convex_scale`

Neutral/default side-score output deliberately uses `1.0` sizing scales, not
zero, so default/test call sites preserve prior behaviour unless real signal
inputs are provided.
- `ladder_clip_scale`
- `late_convex_scale`

Usage:

- Normal paired ladder: side score is a bounded additional clip multiplier.
- Late convex overlay: side score scales favourite/tail notional budgets.
- Strategy notes: logs favourite side, confidence, yes/no score, reversal prob,
  and book penalties.

Important design constraint:

- Side score does not directly decide orders. It only produces bounded sizing
  and diagnostic inputs. The paired-MM and convex overlay engines still decide
  when to quote.

## 4. Normal paired ladder implementation

File:

- `polymarket-exec/src/market_making/paired_mm/ladder_builder.rs`

The ladder is anchored by:

- fair-value anchored reservation prices
- Stoikov inventory skew
- BTC regime and visible-depth-driven ladder shape
- inventory imbalance
- signal clip scaling
- side-score clip scaling

Signal scaling currently includes:

- momentum alignment/adversity
- order-book pressure alignment/adversity
- acceleration alignment/adversity
- side-score bounded ladder tilt

The normal ladder is still intentionally paired and two-sided. The side score
should tilt notional/clip sizes, not convert the workhorse MM path into a pure
directional strategy.

Review points:

- Confirm quantity normalization does not erase too much useful side-score tilt.
- Confirm side-score tilt remains bounded by `max_signal_clip_scale`.
- Confirm risk filtering still enforces inventory caps after sizing.

## 5. Late convex overlay implementation

File:

- `polymarket-exec/src/market_making/paired_mm/engine.rs`

The late convex overlay:

- only runs when enabled in YAML
- only runs after late-window timing gates
- selects favourite from fair value
- requires minimum favourite edge
- uses passive buy price with maker safety ticks
- optionally pairs favourite loading with ultra-cheap tail buying
- caps by max loss, book take pct, active order count, and min order size

This branch adds side-score scaling on top of existing pressure bias:

```text
effective_favorite_scale = pressure_favorite_scale * side_score.favorite.late_convex_scale
effective_tail_scale = pressure_tail_scale * side_score.tail.late_convex_scale
```

Intent reason strings now include:

- side score for favourite
- side score for tail
- reversal probability proxy
- book penalty
- pressure label
- regime label

Review points:

- Verify late convex package fires in curated late-window replay cases.
- Verify tail share count matches intended Bonereaper-style convexity.
- Verify caps do not suppress all meaningful favorite/tail orders.
- Verify whipsaw/reversal behavior is sane.

## 6. Configuration and tuning surfaces

Main YAML:

- `polymarket-exec/config/strategies/btc_5m_paired_mm.live.yaml`

Profile parser:

- `polymarket-exec/src/strategy_profile.rs`

Important sections:

### `quote`

Controls:

- ladder depth
- base/max clip
- min edge
- max spread
- min top depth
- maker safety ticks
- venue minimum order quantity

### `fair_value`

Controls:

- max model divergence from book mid
- model influence weight

### `inventory`

Controls:

- gross/leg/session caps
- open order caps
- side imbalance cap

### `convexity_overlay`

Controls:

- enabled
- late timing
- capital percentage
- fractional Kelly
- max loss
- max book take pct
- min order size
- active order limits
- favourite probability threshold
- favourite edge threshold
- tail enablement
- tail payoff/profit requirements

### `signals.reversal`

Controls:

- enabled
- deceleration weight
- momentum flip weight
- distance-to-strike weight
- orderflow-against weight
- strong momentum threshold
- flip deadband

### `signals.book_sanity`

Controls:

- max spread
- min top depth notional
- max staleness
- max queue depth
- max projected pair cost

### `signals.side_score`

Controls:

- fair value weight
- momentum weight
- orderflow weight
- terminal timing weight
- reversal risk weight
- book sanity weight
- max ladder tilt
- max late convex tilt
- minimum favourite confidence

## 7. What needs systematic review

Review these boundaries first:

1. `PairedMmEngine::decide`
   - Is signal computation deterministic and free of runtime side effects?
   - Are all decisions still typed and risk-filtered?

2. `SideScoreSignal::compute`
   - Are components signed correctly for YES/NO?
   - Does reversal support boost the underdog and penalize the current favourite?
   - Are weights normalized and bounded?

3. `BookSanitySignal::compute`
   - Are penalties too aggressive for normal 5m BTC books?
   - Does stale detection behave correctly for replay and live?
   - Does projected pair cost penalty double-count other pair-cost checks?

4. `build_ladder`
   - Does side-score scale survive paired quantity normalization?
   - Does it preserve the paired-MM workhorse shape?

5. `choose_convex_overlay`
   - Does side-score scaling interact correctly with pressure bias?
   - Does it still respect max loss and active order caps?
   - Does the reason text expose enough diagnostics for replay review?

6. `strategy_profile.rs`
   - Are YAML defaults backwards-compatible?
   - Are all new knobs sourced from YAML rather than hardcoded into deployment
     wrappers?

## 8. Validation still required

This branch implements the signal path but does not prove profitability.

Required validation:

1. Unit tests for new signals.
2. Cargo check/test for compile and regression safety.
3. Curated replay windows covering:
   - normal 50/50 paired MM
   - strong late favourite
   - cheap-tail convexity
   - whipsaw/reversal
   - one-sided stranded inventory
4. Full persisted replay journal with signal components.
5. AWS full-day backtest after ingestion pipeline is stable.
6. Config sweeps for side-score weights and max tilt.

## 9. Reviewer guidance

The intended behavior is conservative:

- Preserve paired-MM as the main revenue engine.
- Use side score to tilt sizing, not to suppress one side entirely.
- Use late convex overlay for Bonereaper-style favourite/tail packages.
- Keep all risk controls in Rust engine code and YAML config.
- Do not move strategy logic into shell wrappers or deployment scripts.

If a review finds that side-score tilt is too weak after quantity
normalization, prefer a targeted adjustment to paired quantity normalization or
clip allocation rather than adding hard directional gates.
