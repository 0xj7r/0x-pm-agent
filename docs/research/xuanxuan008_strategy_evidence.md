# xuanxuan008 strategy evidence

Wallet:

- `0xcfb103c37c0234f524c632d964ed31f117b5f694`
- alias: `xuanxuan008`

This document summarizes the normalized BTC 5m evidence currently captured for
`xuanxuan008`.

## Sample coverage

- `3,923` distinct BTC 5m market windows in wallet activity
- `3,922` paired windows with both `Up` and `Down` buy activity
- `3,496` BTC 5m market rows with `price_to_beat` populated
- `50` closed-position rows currently captured
- `25` realized BTC 5m windows in the current closed-position artifact
- all `25` realized windows are paired

BTC 5m activity counts:

- `24,869` `TRADE / BUY / Up`
- `24,864` `TRADE / BUY / Down`
- `6,683` `MERGE`
- `3,305` `REDEEM`

Interpretation:

- this wallet is overwhelmingly two-sided
- merge/redeem behavior is material
- it looks structurally related to unlawful-shear, but much lower density per market

## Entry timing

- median first buy offset: `10s`
- p10 first buy offset: `4.0s`
- p90 first buy offset: `103.0s`
- median last buy offset: `254s`
- p10 last buy offset: `129.0s`
- p90 last buy offset: `288.0s`

Interpretation:

- like unlawful-shear, the wallet starts early and keeps trading through most of the 5m window
- it appears slightly less persistent to expiry than unlawful-shear

## Fill fragmentation

- median buy fills per market: `12`
- p90 buy fills per market: `20`
- median buy-span within market: `228s`
- p90 buy-span within market: `276s`

Illustrative windows:

1. `btc-updown-5m-1775478900`
- `18` buy rows
- `9` `Up` buy rows
- `9` `Down` buy rows
- `Up` buy cost: `$373.29`
- `Down` buy cost: `$158.05`

2. `btc-updown-5m-1775479800`
- `30` buy rows
- `15` `Up` buy rows
- `15` `Down` buy rows
- `Up` buy cost: `$284.08`
- `Down` buy cost: `$381.63`

Interpretation:

- this wallet slices, but nowhere near the density of unlawful-shear
- it looks more systematic and lower-footprint

## Cost and price geometry

- median `Up` buy cost per window: `$177.05`
- median `Down` buy cost per window: `$173.12`
- median hedge-cost ratio: `0.5474`
- p10 hedge-cost ratio: `0.2128`
- p90 hedge-cost ratio: `0.9012`
- median cheap-leg entry price: `0.3430`
- median expensive-leg entry price: `0.6275`

Interpretation:

- the hedge is still smaller on average, but the two legs are more balanced than unlawful-shear
- this looks like a cleaner two-sided recycler with less extreme cheap-tail geometry

## Realized-window behavior

- realized windows: `25`
- positive combined realized windows: `19`
- negative combined realized windows: `6`
- combined realized PnL total: `$273.46`
- median combined realized PnL per realized window: `$3.79`

Interpretation:

- combined-window outcomes are positive more often than negative
- but the edge per window appears much smaller than unlawful-shear

## Closed-position examples

### Positive combined example: `btc-updown-5m-1776840600`

- `Down`
  - realized PnL: `+$110.11`
  - total bought: `$960.90`
  - avg price: `0.08994`
- `Up`
  - realized PnL: `-$50.43`
  - total bought: `$960.90`
  - avg price: `0.661632`
- combined realized PnL: `+$59.67`

### Positive combined example: `btc-updown-5m-1776842100`

- `Down`
  - realized PnL: `-$96.49`
  - total bought: `$397.50`
  - avg price: `0.775534`
- `Up`
  - realized PnL: `+$150.32`
  - total bought: `$397.50`
  - avg price: `0.089034`
- combined realized PnL: `+$53.83`

### Negative combined example: `btc-updown-5m-1776843000`

- `Up`
  - realized PnL: `-$78.23`
  - total bought: `$1,300.49`
  - avg price: `0.61594`
- `Down`
  - realized PnL: `+$37.67`
  - total bought: `$1,310.49`
  - avg price: `0.343182`
- combined realized PnL: `-$40.56`

## Contract-anchor context

The wallet has the same gamma anchor fields available as unlawful-shear:

- `price_to_beat`
- `final_price`
- `event_start_time`
- `closed_time`

Example:

- `btc-updown-5m-1776902100`
  - `price_to_beat`: `78229.05702174953`
  - `final_price`: `78201.02631015582`

## Working interpretation

`xuanxuan008` looks like:

- a genuine two-sided BTC 5m trader
- much less execution-dense than unlawful-shear
- more balanced between legs
- lower per-window edge capture

This looks closer to a lower-intensity variant of the same broad family than to
a completely different strategy.
