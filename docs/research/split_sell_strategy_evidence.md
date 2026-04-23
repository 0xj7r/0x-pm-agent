# split-sell strategy evidence

Wallet:

- `0xe51b3d64da5b0b8a07a55f8bb3c3170237f73cad`
- alias: `split-sell`

This document summarizes the normalized BTC 5m evidence currently captured for
`split-sell`.

## Sample coverage

- `7,946` distinct BTC 5m market windows in wallet activity
- only `2` paired windows with both `Up` and `Down` buy activity
- `7,149` BTC 5m market rows with `price_to_beat` populated
- `67` closed-position rows currently captured
- `34` realized BTC 5m windows in the current closed-position artifact
- `33` realized windows are paired

BTC 5m activity counts:

- `7,947` `REDEEM`
- `7,723` `SPLIT`
- `3,282` `TRADE / SELL / Up`
- `2,992` `TRADE / SELL / Down`
- `723` `TRADE / BUY / Down`
- `625` `TRADE / BUY / Up`

Interpretation:

- this wallet is not behaving like unlawful-shear or xuanxuan008
- the dominant lifecycle is `SPLIT` and `REDEEM`, with substantial `SELL`, not heavy two-sided buy accumulation
- this looks more like a structured split/redeem or inventory-flip strategy than a two-sided recycler

## Entry timing

- median first buy offset: `178s`
- p10 first buy offset: `61.6s`
- p90 first buy offset: `279.2s`
- median last buy offset: `208s`
- p10 last buy offset: `99.4s`
- p90 last buy offset: `289.0s`

Interpretation:

- buys are later and much less continuous than unlawful-shear
- the wallet does not look like it is building large early-window two-sided books

## Fill fragmentation

- median buy fills per market: `4`
- p90 buy fills per market: `16`
- median buy-span within market: `14s`
- p90 buy-span within market: `59.4s`

Illustrative windows:

1. `btc-updown-5m-1773973800`
- `1` buy row
- `Down` only
- buy cost: `$39.60`
- buy offset: `273s`

2. `btc-updown-5m-1773981600`
- `8` buy rows
- `Down` only
- buy cost: `$24.67`
- buy offsets: `247s` to `261s`

Interpretation:

- execution is sparse and late
- this is a very different fill pattern from unlawful-shear

## Cost and price geometry

- median `Up` buy cost per window: `$109.15`
- median `Down` buy cost per window: `$150.22`
- median hedge-cost ratio: `0.2171`
- median cheap-leg entry price: `0.8033`
- median expensive-leg entry price: `0.8897`

Interpretation:

- these “cheap” and “expensive” labels are misleading for this wallet because both prices are already high
- this does not look like convex cheap-tail buying
- it looks more like late-window or structural inventory handling

## Realized-window behavior

- realized windows: `34`
- positive combined realized windows: `5`
- negative combined realized windows: `1`
- combined realized PnL total: `-$238.40`
- median combined realized PnL per realized window: `$0.00`

Important caution:

- many realized rows are highly stylized:
  - `total_bought` often fixed at `$1000.0`
  - `avg_price` often fixed at `0.5`
  - realized outcomes often show `+500 / -500` type symmetry

Interpretation:

- the current closed-position artifact for this wallet appears structurally different from unlawful-shear
- it may reflect a split/redeem accounting path rather than ordinary directional trade closure
- we should be careful not to interpret these rows as direct evidence of the same strategy family

## Closed-position examples

### Stylized paired example: `btc-updown-5m-1776938700`

- `Up`
  - realized PnL: `-$404.40`
  - total bought: `$1000.0`
  - avg price: `0.5`
- `Down`
  - realized PnL: `+$500.0`
  - total bought: `$1000.0`
  - avg price: `0.5`
- combined realized PnL: `+$95.60`

### Stylized paired example: `btc-updown-5m-1776929700`

- `Down`
  - realized PnL: `+$500.0`
  - total bought: `$1000.0`
  - avg price: `0.5`
- `Up`
  - realized PnL: `-$406.0`
  - total bought: `$1000.0`
  - avg price: `0.5`
- combined realized PnL: `+$94.0`

## Contract-anchor context

The wallet has the same gamma anchor fields available:

- `price_to_beat`
- `final_price`
- `event_start_time`
- `closed_time`

Example:

- `btc-updown-5m-1776902100`
  - `price_to_beat`: `78229.05702174953`
  - `final_price`: `78201.02631015582`

## Working interpretation

`split-sell` does **not** currently look like an unlawful-shear-style two-sided
recycler.

It looks more like:

- late entry
- sparse buy activity
- strong `SPLIT` / `REDEEM` lifecycle
- structural inventory transformation and sale behavior

This should be treated as a separate strategy family, not a simple variant of
the unlawful-shear pattern.
