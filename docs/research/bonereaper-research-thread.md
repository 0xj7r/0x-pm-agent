# Bonereaper Research Thread

Status: active  
Last updated: 2026-04-23  
Wallet: `0xeebde7a0e019a63e6b476eb425505b7b3e6eba30`  
Public profile: https://polymarket.com/@bonereaper

## Why This Wallet Matters

`Bonereaper` is the strongest second benchmark for the short-duration crypto MM lane.

It matters because:

- it shows the same smooth high-turnover return profile class as the other top whales
- it is clearly active in ETH 5m and ETH 15m, not just BTC 5m
- it confirms that the opportunity set is broader than one wallet or one market sleeve

## Current Evidence Base

Unlike `unlawful-shear`, there is no local research DB for `Bonereaper` in this repo right now.

So this thread is currently based on:

- live Polymarket activity API
- live Polymarket rebate endpoint
- live Polygon `USDC.e` token transfers via Etherscan V2

This should be treated as the canonical temporary thread until we pull a local DB for it.

## Confirmed Maker Activity

From live rebate checks in this session:

- April 20, 2026: about `$733.52`
- April 21, 2026: about `$647.79`
- April 22, 2026: about `$774.32`

That is enough to call the wallet materially maker-active.

## Recent Activity Shape

Fresh activity summary:

- `1000` recent rows
- `992 TRADE`
- `8 REDEEM`
- `992 BUY`
- `0 SELL`
- `12` markets
- `8` two-sided markets

This is a clean MM-style activity fingerprint:

- buy-heavy
- two-sided
- short-duration
- no obvious directional one-leg bias in the top market mix

## Market Mix

Recent dominant markets:

- `eth-updown-15m-1776961800`
- `eth-updown-5m-1776961500`
- `eth-updown-15m-1776960900`
- `eth-updown-5m-1776962100`
- `btc-updown-5m-1776962100`
- `btc-updown-15m-1776960900`
- `btc-updown-15m-1776961800`

Interpretation:

- `Bonereaper` is not just a BTC 5m whale
- it is clearly active across:
  - ETH 5m
  - ETH 15m
  - BTC 5m
  - BTC 15m

## Funding and Capital Recycling

### Earliest visible external funding

Before the first heavy exchange outflow, the wallet received from `0xf70da97812cb96acdf810712aa562db8dfa3dbef`:

- `$9.98` at March 25, 2026 06:25:55 UTC
- `$17,980.20` at March 25, 2026 06:33:05 UTC
- `$99.86` at March 25, 2026 07:17:31 UTC
- `$9,988.99` at March 25, 2026 07:19:59 UTC

This gives a visible starting seed of about `$28.1k` before serious exchange activity.

The same funding wallet added more that morning, bringing visible external funding to about `$88.0k` by March 25, 2026 08:42:43 UTC.

Important clarification:

- on Polygon, `0xf70d...dbef` is not a Safe or other contract wallet
- `eth_getCode` returned `0x`
- treat it as a shared treasury / operator EOA unless stronger evidence appears

### Recycling behavior

Live Polygon `USDC.e` transfer summary:

- outbound to CTF Exchange: about `$208.7k`
- inbound from CTF: about `$192.5k`

Interpretation:

- gross flow is much larger than seed capital because collateral is being recycled
- this is consistent with high-turnover short-duration MM behavior

## Current Classification

Best current call:

- confirmed maker-active wallet
- strong short-duration crypto MM-style participant
- especially relevant for ETH 5m / 15m

What this does not prove:

- every fill is maker-side
- every dollar of wallet PnL came from those exact markets

But it is more than enough to keep `Bonereaper` as a benchmark wallet for engine design.

## Why Bonereaper Matters For Our Build

`Bonereaper` strengthens the core thesis:

- this lane is real
- it exists across more than one asset family
- meaningful profits are possible with short-duration maker behavior

Operationally, it suggests:

- the engine should not be BTC-only in its design assumptions
- ETH 5m and ETH 15m should be easy extensions once the execution plane is right
- cross-sleeve market rotation matters

## Recommended Next Step For Bonereaper Research

To make this thread as reusable as `unlawful-shear`, we should collect a local DB with:

- raw activity
- market stream events
- orderbook snapshots
- window reconstructions
- accounting snapshots

Until that exists, this thread should be treated as:

- economically strong
- structurally useful
- microstructure-incomplete

## Useful Commands

Activity summary:

```bash
curl -sS 'https://data-api.polymarket.com/activity?user=0xeebde7a0e019a63e6b476eb425505b7b3e6eba30&limit=1000&offset=0' \
  | jq '{rows:length,
         type_counts:(group_by(.type)|map({k:.[0].type,n:length})),
         side_counts:(map(select(.side != null and .side != ""))|group_by(.side)|map({k:.[0].side,n:length})),
         market_count:(map(.slug)|unique|length),
         two_sided_markets:(map(select(.type=="TRADE"))|group_by(.slug)|map({slug:.[0].slug, outcomes:(map(.outcome)|unique)})|map(select((.outcomes|length)>1))|length)}'
```

Rebate check:

```bash
curl -sS 'https://clob.polymarket.com/rewards/current?maker=0xeebde7a0e019a63e6b476eb425505b7b3e6eba30&date=2026-04-22'
```

Funding / transfer summary:

```bash
curl -sS 'https://api.etherscan.io/v2/api?chainid=137&module=account&action=tokentx&address=0xeebde7a0e019a63e6b476eb425505b7b3e6eba30&page=1&offset=10000&sort=asc&apikey=$ETHERSCAN_API_KEY'
```

## Bottom Line

`Bonereaper` should remain in the research set because it confirms:

- short-duration crypto MM is not a one-wallet anomaly
- ETH 5m / 15m are real targets
- bankroll helps, but the structure still looks like fast inventory recycling and maker economics rather than impossible capital requirements
