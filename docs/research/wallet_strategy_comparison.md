# Wallet strategy comparison

Tracked wallets:

- `unlawful-shear` (`0xb27bc932bf8110d8f78e55da7d5f0497a18b5b82`)
- `xuanxuan008` (`0xcfb103c37c0234f524c632d964ed31f117b5f694`)
- `split-sell` (`0xe51b3d64da5b0b8a07a55f8bb3c3170237f73cad`)

This document compares the normalized BTC 5m behavior of the three wallets to
separate shared structure from wallet-specific tactics.

## Comparison matrix

| wallet | BTC 5m windows | paired windows | paired share | buy up | buy down | sells | merges | redeems | splits | median first buy offset | median last buy offset | median fills/market | median market span | median hedge ratio | realized windows | realized + | realized - | realized total |
|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| `unlawful-shear` | `533` | `495` | `0.9287` | `92,984` | `92,206` | `0` | `6,963` | `518` | `0` | `9s` | `279s` | `298` | `258s` | `0.4256` | `71` | `48` | `23` | `$10,643.26` |
| `xuanxuan008` | `3,923` | `3,922` | `0.9997` | `24,869` | `24,864` | `0` | `6,683` | `3,305` | `0` | `10s` | `254s` | `12` | `228s` | `0.5474` | `25` | `19` | `6` | `$273.46` |
| `split-sell` | `7,946` | `2` | `0.0003` | `625` | `723` | `6,274` | `0` | `7,947` | `7,723` | `178s` | `208s` | `4` | `14s` | `0.2171` | `34` | `5` | `1` | `-$238.40` |

## What is shared

### unlawful-shear and xuanxuan008

Shared characteristics:

- both are overwhelmingly two-sided in BTC 5m
- both start buying very early in the window
- both keep trading through most of the 5m interval
- both show meaningful merge/redeem behavior
- both look like market-window payoff managers rather than one-shot directional bettors

This strongly suggests a shared strategy family:

- two-sided BTC 5m
- paired inventory
- core leg plus hedge leg
- intrawindow scaling
- payoff shaping at the market level

### split-sell

Not shared with the above pair:

- almost no paired buy windows
- dominant lifecycle is `SPLIT` / `REDEEM` / `SELL`
- buy activity is sparse, late, and short-lived

This should be treated as a separate family.

## Key differences

### unlawful-shear vs xuanxuan008

`unlawful-shear`:

- far denser execution
- median `298` buy fills per market
- stronger realized PnL on the currently captured realized subset
- hedge ratio more asymmetric

`xuanxuan008`:

- same broad two-sided structure
- much lower density: median `12` fills per market
- more balanced legs: hedge ratio median `0.5474`
- smaller realized edge in the current sample

Interpretation:

- `xuanxuan008` looks like a lower-intensity, lower-footprint cousin of the unlawful-shear family

### split-sell vs the other two

`split-sell` is structurally different:

- median first buy offset is much later (`178s`)
- median fills per market only `4`
- median buy-span only `14s`
- huge `SPLIT` / `REDEEM` counts
- substantial `SELL` activity

Interpretation:

- this is not the right basis for the first paper-trade strategy family
- it may still be valuable later as a separate split/redeem or structural inventory strategy

## Implementation implications

### Family 1: unlawful-shear / xuanxuan008

This should be the first Rust paper-trading family.

Core properties:

- BTC 5m only
- two-sided default
- early start, late persistence
- fragmented intrawindow execution
- market-window payoff objective

Candidate variants:

1. `unlawful-core`
- heavy fragmentation
- stronger asymmetry
- higher activity density

2. `xuanxuan-lite`
- same broad structure
- lower execution density
- more balanced leg sizing

### Family 2: split-sell

Do **not** put this into the first Rust paper strategy family.

Instead, treat it as a separate future investigation:

- split/redeem dominant
- likely very different inventory lifecycle
- likely different execution and PnL mechanics

## Current recommendation

Prioritize implementation in this order:

1. `unlawful-shear` style high-density two-sided recycler
2. `xuanxuan008` style lower-density two-sided recycler
3. later: separate `split-sell` family if still interesting after the first paper results
