# Unlawful-Shear strategy evidence

Wallet:

- `0xb27bc932bf8110d8f78e55da7d5f0497a18b5b82`
- alias: `unlawful-shear`

This document grounds the strategy reconstruction in the wallet activity and
market data currently loaded in the repo. It is the empirical companion to
[unlawful_shear_strategy_reconstruction.md](/Users/jackreid/go/polymarket-agent/docs/research/unlawful_shear_strategy_reconstruction.md).

## Sample coverage

Current BTC 5m sample from the normalized wallet DB:

- `531` distinct BTC 5m market windows in wallet activity
- `493` paired windows with both `Up` and `Down` buy activity
- `526` BTC 5m market rows with `price_to_beat` populated in market metadata
- `137` closed-position rows currently captured
- `71` realized BTC 5m windows in the current closed-position artifact
- `66` of those realized windows have both legs present

BTC 5m activity counts:

- `92,773` `TRADE / BUY / Up`
- `92,032` `TRADE / BUY / Down`
- `6,934` `MERGE`
- `516` `REDEEM`

Interpretation:

- two-sided BTC 5m participation is the normal case, not an exception
- merge/redeem activity is material, so this is not just a naked one-leg hold strategy

## Entry timing

Measured across BTC 5m windows with buy activity:

- median first buy offset: `9s`
- p10 first buy offset: `7.0s`
- p90 first buy offset: `93.4s`
- median last buy offset: `279s`
- p10 last buy offset: `238.0s`
- p90 last buy offset: `302.0s`

Interpretation:

- the wallet usually starts trading almost immediately after the 5m window opens
- it usually keeps trading almost until the end of the window
- this is not an “open once” or “last-second only” strategy; it is continuous intrawindow execution

## Fill fragmentation

Measured across BTC 5m windows with buy activity:

- median buy fills per market: `298`
- p90 buy fills per market: `690`
- median buy-span within market: `258s`
- p90 buy-span within market: `292s`

Illustrative windows:

1. `btc-updown-5m-1774006800`
- `732` buy rows
- `407` `Up` buy rows
- `325` `Down` buy rows
- `Up` buy cost: `$7,869.44`
- `Down` buy cost: `$2,183.43`
- first buy offset: `15s`
- last buy offset: `279s`

2. `btc-updown-5m-1774007100`
- `817` buy rows
- `431` `Up` buy rows
- `386` `Down` buy rows
- `Up` buy cost: `$2,570.15`
- `Down` buy cost: `$8,154.09`
- first buy offset: `7s`
- last buy offset: `285s`

3. `btc-updown-5m-1774050300`
- `1,109` buy rows
- `415` `Up` buy rows
- `694` `Down` buy rows
- `Up` buy cost: `$7,654.99`
- `Down` buy cost: `$5,169.49`
- first buy offset: `207s`
- last buy offset: `299s`

Interpretation:

- execution is highly sliced
- side preference changes by market, but two-sided participation remains common
- some windows engage immediately; others delay meaningful sizing until late in the interval

## Cost and price geometry

Measured across BTC 5m windows with both sides bought:

- median `Up` buy cost per window: `$1,286.36`
- median `Down` buy cost per window: `$1,207.79`
- median hedge-cost ratio: `0.4256`
- p10 hedge-cost ratio: `0.1487`
- p90 hedge-cost ratio: `0.8362`
- median cheap-leg entry price: `0.2951`
- median expensive-leg entry price: `0.6958`

Interpretation:

- the opposite leg is usually meaningfully smaller in cost, but not always tiny
- there is a broad distribution of hedge sizing, so the structure is asymmetric rather than fixed-ratio
- the cheap/expensive leg framing is empirically supported

## Realized-window behavior

Across the currently realized BTC 5m windows:

- realized windows: `71`
- positive combined realized windows: `48`
- negative combined realized windows: `23`
- combined realized PnL total: `$10,643.26`
- median combined realized PnL per realized window: `$137.56`

Interpretation:

- on the realized subset currently captured, market-window combined PnL is positive more often than negative
- this supports the view that the strategy is designed around window-level payoff shaping rather than per-leg correctness

## Closed-position examples

These examples are the clearest evidence of core-leg / cheap-hedge behavior.

### Positive combined example: `btc-updown-5m-1776930600`

- `Up`
  - realized PnL: `+$1,466.63`
  - total bought: `$6,749.67`
  - avg price: `0.020429`
- `Down`
  - realized PnL: `-$77.28`
  - total bought: `$6,820.62`
  - avg price: `0.912721`
- combined realized PnL: `+$1,389.34`

### Positive combined example: `btc-updown-5m-1776931200`

- `Up`
  - realized PnL: `+$1,530.94`
  - total bought: `$5,930.59`
  - avg price: `0.073178`
- `Down`
  - realized PnL: `-$554.60`
  - total bought: `$6,096.22`
  - avg price: `0.705505`
- combined realized PnL: `+$976.34`

### Negative combined example: `btc-updown-5m-1776882000`

- `Down`
  - realized PnL: `-$1,926.30`
  - total bought: `$6,588.54`
  - avg price: `0.601152`
- `Up`
  - realized PnL: `+$1,481.81`
  - total bought: `$5,570.75`
  - avg price: `0.475738`
- combined realized PnL: `-$444.49`

### Negative combined example: `btc-updown-5m-1776881100`

- `Up`
  - realized PnL: `+$459.42`
  - total bought: `$3,263.78`
  - avg price: `0.960527`
- `Down`
  - realized PnL: `-$950.33`
  - total bought: `$3,249.76`
  - avg price: `0.765401`
- combined realized PnL: `-$490.91`

Interpretation:

- the cheap leg can more than offset the expensive leg when convexity pays off
- losses still happen, but the structure is clearly designed to reduce the frequency of outright one-leg ruin

## Contract-anchor context

Example BTC 5m anchor rows from market metadata:

1. `btc-updown-5m-1776902100`
- `price_to_beat`: `78229.05702174953`
- `final_price`: `78201.02631015582`
- `event_start_time`: `2026-04-22T23:55:00Z`
- `closed_time`: `2026-04-23 00:00:52+00`

2. `btc-updown-5m-1776901800`
- `price_to_beat`: `78301.56000000001`
- `final_price`: `78229.05702174953`

3. `btc-updown-5m-1776901500`
- `price_to_beat`: `78278.09`
- `final_price`: `78301.56000000001`

Interpretation:

- we now have the exact market threshold and final resolution value for every BTC 5m market row in the sample
- this means strategy logic can be expressed in terms of the actual contract anchor, not just generic BTC direction

## Execution classification evidence

From the prior joined execution artifact:

- roughly `1,779` passive-classified buy rows
- roughly `1,504` taker-classified buy rows
- visible `ask_sum < 1` rows are rare

Interpretation:

- the wallet is not passive-only and not taker-only
- it appears to complete aggressively when needed, while still taking passive/improved fills when available
- the edge is not just naive negative-risk scanning

## Implementation implications

What this evidence supports implementing now:

1. BTC 5m only
2. two-sided default
3. asymmetrical core-leg / hedge-leg sizing
4. intrawindow scaling rather than one-shot entries
5. execution model that supports fragmented fills
6. market-window payoff objective instead of per-leg hit-rate objective

What still needs more work:

1. exact exit attribution: `sell` vs `merge` vs `redeem`
2. historical trade-adjacent order book state at every entry
3. more complete realized-window accounting coverage
