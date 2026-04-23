# Unlawful-Shear Rust execution plan

Wallet:

- `0xb27bc932bf8110d8f78e55da7d5f0497a18b5b82`
- alias: `unlawful-shear`

This is the execution-facing spec for the first Rust implementation. It is not
an exact clone. It is the first parameterized strategy variant grounded in the
evidence we trust today.

## Core theses

1. `BTC 5m only`
- this wallet's repeatable edge is concentrated in recurring BTC 5m markets

2. `Two-sided default`
- paired participation is the norm, not an exception
- the executor should assume one core leg plus one hedge leg, not one-shot directional betting

3. `Expensive core + cheap convex hedge`
- the favored side is often the more expensive leg
- the opposite side is accumulated only when it is cheap enough to preserve convexity

4. `Dynamic rebalance matters`
- the wallet flips and reweights within the same window
- the executor must support repeated small clips rather than static entry once

5. `Loser salvage matters`
- the wallet often does not allow losing legs to decay to zero
- the executor must be able to trim paired losers while residual bid value still exists

## What the first Rust implementation can observe

The current runtime sees:

- paired best bid / ask
- current inventory
- open-order count
- immediate paper fill model

The current runtime does not yet see:

- `price_to_beat`
- BTC spot path
- explicit seconds-from-window-open
- merge mechanics in execution logic

So the first implementation uses paired-book geometry as a proxy for the
research signals we observed. This is deliberate and explicit.

## Strategy mode

Environment:

- `WHALE_PAIR_STRATEGY=unlawful_shear`

Supported modes now:

- `unlawful_shear`
- `goat_pair`
- `noop`

Optional runtime context:

- `WHALE_PAIR_EXEC_MARKET_CONTEXT_PATH=/abs/path/to/rust_market_context.json`

Exporter:

```bash
python3 research/dataops/export_live_gamma_runtime.py \
  --env-out data/research/wallet_research/unlawful-shear/rust_runtime.env \
  --context-out data/research/wallet_research/unlawful-shear/rust_market_context.json
```

## Implemented decision rules

### Entry

- identify the cheaper and more expensive outcome from paired best asks
- treat the expensive side as the `core` candidate when:
  - `price_gap >= min_price_gap`
  - `core_price_min <= expensive_ask <= core_price_max`
- treat the cheaper side as the `hedge` candidate when:
  - `cheap_ask <= cheap_hedge_price_max`

### Initial build

- if no core inventory exists and the expensive side qualifies, buy a core clip
- if no hedge inventory exists and the cheap side qualifies, buy a hedge probe

### Rebalance

- if paired inventory exists and hedge-cost ratio is below the target band, add hedge
- if paired inventory exists and hedge-cost ratio is above the target band, add core
- if price dislocation is large, add a rebalance clip toward the underweight side

### Salvage

- if paired inventory exists and one side's current best bid has fallen materially below its average price
- and residual bid is still above `salvage_bid_floor`
- then trim a fraction of that side with a `reduce_only` sell

## Parameters exposed via env

- `WHALE_PAIR_UNLAWFUL_SHEAR_CHEAP_HEDGE_PRICE_MAX`
- `WHALE_PAIR_UNLAWFUL_SHEAR_CORE_PRICE_MIN`
- `WHALE_PAIR_UNLAWFUL_SHEAR_CORE_PRICE_MAX`
- `WHALE_PAIR_UNLAWFUL_SHEAR_MIN_PRICE_GAP`
- `WHALE_PAIR_UNLAWFUL_SHEAR_PROBE_CLIP_USD`
- `WHALE_PAIR_UNLAWFUL_SHEAR_CORE_CLIP_USD`
- `WHALE_PAIR_UNLAWFUL_SHEAR_HEDGE_CLIP_USD`
- `WHALE_PAIR_UNLAWFUL_SHEAR_REBALANCE_CLIP_USD`
- `WHALE_PAIR_UNLAWFUL_SHEAR_TRIM_CLIP_FRACTION`
- `WHALE_PAIR_UNLAWFUL_SHEAR_MAX_GROSS_COST_USD`
- `WHALE_PAIR_UNLAWFUL_SHEAR_TARGET_HEDGE_RATIO_MIN`
- `WHALE_PAIR_UNLAWFUL_SHEAR_TARGET_HEDGE_RATIO_MAX`
- `WHALE_PAIR_UNLAWFUL_SHEAR_SALVAGE_DRAWDOWN_RATIO`
- `WHALE_PAIR_UNLAWFUL_SHEAR_SALVAGE_BID_FLOOR`
- `WHALE_PAIR_UNLAWFUL_SHEAR_MAX_OPEN_ORDERS_TOTAL`
- `WHALE_PAIR_UNLAWFUL_SHEAR_COOLDOWN_MS`

## Intended next refinements

1. inject Gamma metadata into the runtime:
- `price_to_beat`
- `final_price`
- market start time

2. inject BTC spot path into the runtime:
- threshold-aware core-side selection
- late-window acceleration logic

3. make merge handling explicit in paper/live accounting

4. replace immediate-taker paper fills with queue-aware partial-fill simulation

## Live-money bar

Do not move to live money because the code compiles or paper fills once.
Move only after:

1. paper behavior matches the intended two-sided geometry
2. trim / salvage logic behaves sensibly under rapid repricing
3. per-market gross exposure stays bounded
4. journals show stable, explainable order flow over a multi-day paper run
