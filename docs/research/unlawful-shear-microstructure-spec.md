# Unlawful-Shear Microstructure Spec

Status: implementation input  
Last updated: 2026-04-23  
Wallet: `0xb27bc932bf8110d8f78e55da7d5f0497a18b5b82`

## Purpose

This document extracts the parts of the `unlawful-shear` dataset that are directly useful for strategy reconstruction.

This is not a wallet-classification note. It is an implementation input for the Rust engine.

Use this when deciding:

- entry timing
- leg asymmetry
- clip sizing
- pair recycling
- late-window behavior
- what state the runtime must track

Primary local source:

- [wallet_research.db](/Users/jackreid/go/polymarket-agent/data/research/wallet_research/unlawful-shear/wallet_research.db)

## Ground Truth We Must Preserve

Two things are true at the same time:

1. At the wallet/economic level, `unlawful-shear` is maker-active.
2. At the intrawindow execution level, the behavior is active and inventory-shaped, not a trivial passive quoting bot.

Do not collapse those into one idea.

The engine we build should therefore be:

- maker-first economically
- inventory-aware behaviorally
- fast-pairing and merge-recycling operationally

## Data Coverage

What still exists locally:

- `wallet_activity_raw`: `241,243` rows
- `wallet_market_stream_events`: `265,556` rows
- `wallet_orderbook_snapshots`: `646` rows
- `wallet_window_reconstructions`: `531` rows
- `wallet_closed_positions_raw`: `137` rows
- `wallet_accounting_snapshots`: `20` rows

This is enough to reconstruct:

- per-window participation timing
- leg cost asymmetry
- clip statistics
- merge timing
- rough late-window behavior

## Top-Level Activity Shape

From `wallet_activity_raw`:

- trade rows: `230,702`
- buy rows: `230,702`
- sell rows: `0`
- distinct markets: `725`

Interpretation:

- local activity feed shows buy-side inventory formation only
- visible realization happens through merge/redeem/accounting rather than direct sell-heavy activity

This means the strategy logic should not be modeled as “buy one leg, then sell one leg later” as the primary path.

## Window-Level Participation

From `wallet_window_reconstructions`:

- reconstructed windows: `531`
- paired windows: `493`

Average paired-window behavior:

- buy rows per window: `374.71`
- merge rows per window: `14.04`

Windows with at least one merge:

- `488 / 493`

Windows with zero merges:

- `5 / 493`

Interpretation:

- merge is not optional behavior
- merge is part of the normal control loop
- the engine should expect repeated pair recycling inside one market, not just one terminal merge

## Entry Timing

Average first-trade lag from window start:

- `29.04s`

Average last-trade lag from window start:

- `446.66s`

Average time remaining after first trade:

- `270.96s`

Entry distribution:

- windows entering within `30s`: `430 / 493`
- windows entering within `60s`: `437 / 493`
- windows entering at or after `180s`: `29 / 493`

Interpretation:

- default behavior is early participation
- this is not primarily a late sniper strategy
- the strategy usually gets involved almost immediately, then stays active through the window

Design implication:

- the runtime must be ready before market open
- market discovery and context handoff cannot lag
- quotes or accumulation logic must engage in the first minute, often in the first `30s`

## Merge Timing

Using the first observed merge per paired window:

- windows with merge: `488`
- average first merge lag from start: `63.18s`
- average first merge lag after first trade: `34.44s`
- merges within `60s` of entry: `444 / 488`
- merges within `120s` of entry: `477 / 488`

Interpretation:

- paired inventory is recycled very quickly
- pair completion is expected soon after entry
- idle paired inventory is probably considered wasted capital

Design implication:

- pair ledger and merge planner must sit on the hot path
- merge candidates should be evaluated continuously
- free-cash pressure should favor fast recycle

## Cheap Leg / Expensive Leg Geometry

Across paired windows:

- average up-leg buy price: `0.5071`
- average down-leg buy price: `0.4901`
- average hedge-cost ratio: `0.4546`

Grouped by which leg was cheap:

### Cheap leg = `Down`

- windows: `262`
- average cheap price: `0.3054`
- average expensive price: `0.6995`
- average gap: `0.3941`
- average hedge ratio: `0.4574`

### Cheap leg = `Up`

- windows: `231`
- average cheap price: `0.2889`
- average expensive price: `0.6997`
- average gap: `0.4107`
- average hedge ratio: `0.4513`

Interpretation:

- whichever side is cheap, it is usually around `0.29-0.31`
- whichever side is expensive, it is usually around `0.70`
- the strategy is not trying to buy both sides symmetrically near `0.50`
- the strategy is deliberately building an expensive core and a much cheaper hedge

Design implication:

- the strategy model should be “core + hedge,” not “balanced straddle”
- the hedge is there for convexity protection and pairability, not equal capital allocation

## Clip Sizing

By leg, aggregated across paired windows:

### Cheap leg

- trades: `101,592`
- total notional: `$505,861.67`
- average clip notional: `$4.9793`
- average fill price: `0.3009`

### Expensive leg

- trades: `83,209`
- total notional: `$1,093,753.23`
- average clip notional: `$13.1447`
- average fill price: `0.6569`

Per paired window:

- average cheap-leg trades: `206.07`
- average expensive-leg trades: `168.78`
- average cheap-leg notional per window: `$1,026.09`
- average expensive-leg notional per window: `$2,218.57`
- average cheap-to-expensive notional ratio: `0.4716`

Interpretation:

- cheap hedge is built with more clips, but smaller clips
- expensive core is built with fewer clips, but much larger clips
- the hedge uses roughly `47%` of the expensive-leg notional on average

Design implication:

- the engine should have separate clip schedules for core and hedge
- the hedge sleeve should be finer-grained than the core sleeve
- the strategy should target a cheap/core notional ratio around `0.45-0.50` as a starting point, not `1.0`

## Fit To Current Paper Profile

Against the current `unlawful_shear` config:

- cheap leg `<= 0.38`: `349 / 493`
- expensive leg in `0.52-0.92`: `459 / 493`
- price gap `>= 0.12`: `429 / 493`
- hedge ratio in `0.20-0.60`: `254 / 493`

Current config:

- cheap hedge max: `0.38`
- core band: `0.52-0.92`
- min gap: `0.12`
- probe clip: `3`
- core clip: `20`
- hedge clip: `6`
- rebalance clip: `12`
- target hedge ratio: `0.20-0.60`
- salvage drawdown: `0.18`
- salvage bid floor: `0.05`

Interpretation:

- the current profile is directionally grounded
- it is not complete, but it is consistent with the DB

## Strategy Reconstruction

Best working reconstruction of `unlawful-shear`:

1. Engage early in the window, usually within the first `30-60s`.
2. Identify the cheap leg and expensive leg from current ask geometry.
3. Build the expensive leg as the main exposure.
4. Build the cheap leg as a smaller hedge.
5. Use many clips rather than a few large orders.
6. Continuously rebalance toward the target hedge ratio.
7. Convert completed pairs back into collateral via frequent merges.
8. Continue shaping inventory late in the window.
9. Use salvage / cleanup when one leg is fading or the window is near expiry.

This is not:

- pure passive midpoint quoting
- pure taker directional entry
- symmetric complement buying

It is:

- maker-first economically
- core/hedge asymmetric structurally
- clip-based operationally
- merge-driven from a capital-usage standpoint

## What The Engine Must Support

### Hot-path state

The runtime must track:

- market open / age / time remaining
- current cheap leg and expensive leg
- current expensive notional
- current cheap notional
- current hedge ratio
- number of clips already executed this window
- paired quantity
- first entry timestamp
- first merge timestamp
- last action timestamp

### Decision loop

For each active market:

1. Determine cheap and expensive side from live asks.
2. Decide whether expensive side qualifies as core.
3. Decide whether cheap side qualifies as hedge.
4. If no core, optionally place probe behavior only.
5. If core exists and hedge ratio is too low, add cheap hedge.
6. If hedge ratio is too high, add expensive core.
7. If pairable quantity is available, merge quickly.
8. If window is late, shift from accumulation to cleanup / salvage.

### Order behavior

The engine must support:

- separate clip sizes by role:
  - probe
  - core
  - hedge
  - rebalance
  - salvage / cleanup
- independent cooldowns
- role-tagged orders for attribution

## Initial Parameter Guesses For Paper

These are not final truths. They are grounded starting points:

- entry window: first `60s`, with strongest bias in first `30s`
- cheap-leg price zone: `<= 0.38`
- expensive-leg core zone: `0.52-0.92`
- initial cheap/core notional target: `0.45-0.50`
- cheap clip size smaller than core by about `2-3x`
- merge candidate evaluation every tick
- late-window transition before final minute

## Unknowns We Still Need To Calibrate

The DB does not fully resolve:

- exact passive quote placement vs aggressive crossing on every clip
- exact user-channel order lifecycle for each fill
- exact salvage trigger boundaries
- exact latency sensitivity

Those should be treated as live-calibration questions, not reasons to delay the framework.

## Practical Build Interpretation

For Rust implementation:

- use this spec to ground the strategy policy
- use [unlawful-shear-reconstruction-thread.md](/Users/jackreid/go/polymarket-agent/docs/research/unlawful-shear-reconstruction-thread.md) for the broader wallet narrative
- use the MM implementation spec for platform architecture

This document is the one to use when deciding how the strategy should actually behave inside a 5-minute market.
