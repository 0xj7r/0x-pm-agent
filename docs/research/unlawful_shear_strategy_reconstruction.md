# Unlawful-Shear strategy reconstruction

Wallet:

- `0xb27bc932bf8110d8f78e55da7d5f0497a18b5b82`
- Polymarket alias: `unlawful-shear`

This document is the current working reconstruction of the wallet's BTC 5m
strategy based on the data we already trust. It is not intended to claim a
perfect clone. It is intended to define a strategy family we can implement and
paper trade while the data pipeline keeps improving.

## What we know with high confidence

From the normalized wallet activity, market metadata, BTC path, and prior
joined execution artifacts:

- the wallet specializes heavily in `BTC Up/Down 5m`
- it trades both sides of the same market very often
- it fragments execution into many fills per market
- it mixes passive/improved fills with taker-like completion
- it frequently merges/redeems rather than behaving like a simple one-leg hold
- the objective looks like market-window payoff shaping, not raw directional hit rate

Current evidence:

- `526` BTC 5m windows in the reconstructed dataset
- `489` paired windows
- median hedge-cost ratio around `0.426`
- all `526` BTC 5m market rows have `price_to_beat` populated
- prior recent execution study:
  - median first buy offset `12s`
  - median last buy offset `294s`
  - roughly `1779` passive-classified buys vs `1504` taker-classified buys
  - naive negative-risk windows are rare

## Reconstructed strategy class

### 1. Market universe

- trade only `BTC Up/Down 5m` recurring markets
- ignore broader markets for the first implementation

### 2. Position structure

- two-sided participation is the default
- one leg is the `core leg`
- the opposite side is a `cheap convex hedge`
- the hedge is not symmetric inventory; it is usually smaller in cost

### 3. Entry timing

- first entries often appear near the start of the 5m window
- the wallet keeps trading through most of the window
- the strategy is not purely “open auction” nor purely “last-second”
- expected live behavior:
  - open small / probe early
  - scale into the preferred side through the window
  - add or resize the opposite hedge only when pricing is favorable

### 4. Execution style

- fragmented fills are normal
- mixed maker/taker behavior is normal
- interpretation:
  - passive when book quality is acceptable
  - taker when completion urgency rises or pricing dislocates briefly

### 5. Core market logic

The wallet appears to care about:

- current market price vs `price_to_beat`
- BTC path during the 5m interval
- relative cheapness of the opposite leg
- preserving market-window convexity

This does **not** look like:

- “bet direction once and hold”
- “buy only negative-risk sums”
- “always take symmetric both-side inventory”

### 6. Exit behavior

- not fully resolved yet from the current accounting logic
- but behavior strongly suggests a combination of:
  - selective sells
  - merge/redeem flows
  - paired inventory management

## First implementation variants

These are the variants we should implement and paper trade.

### Variant A: Core leg + cheap tail hedge

- choose the preferred side using live BTC path vs `price_to_beat`
- buy the core side in larger notional
- buy the opposite side only when it is sufficiently cheap
- objective: positive or bounded market-window payoff

### Variant B: Two-sided recycler

- maintain exposure on both sides when spread/price geometry is favorable
- recycle inventory through incremental fills
- merge/redeem when pair economics justify it

### Variant C: Late-window convexity buyer

- delay main sizing until late in the window
- buy cheap opposite tails aggressively when one side becomes overconfident
- smaller trade count, more selective windows

## Minimum live data required to improve the model

- continuous CLOB best-bid/ask capture for touched token ids
- trade-adjacent Polymarket book state
- better realized-PnL / close-method attribution

## What this is good enough for right now

- implementing Rust paper variants
- comparing our strategy family against the wallet's observed activity
- monitoring whether the wallet still behaves consistently with this model

## What this is not good enough for yet

- claiming we have matched the wallet's exact edge
- claiming exact realized PnL replication
- concluding whether the edge is mostly signal, microstructure, or queue priority
