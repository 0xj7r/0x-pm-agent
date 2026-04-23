# Unlawful-Shear Reconstruction Thread

Status: active  
Last updated: 2026-04-23  
Wallet: `0xb27bc932bf8110d8f78e55da7d5f0497a18b5b82`

## Why This Wallet Matters

`unlawful-shear` is still the most important benchmark for the BTC 5m engine rebuild because:

- at the wallet/economic level it is clearly maker-active
- locally we still have real microstructure data for it
- the current paper strategy in `whale-pair-exec` is already partly modeled around its execution geometry

This thread is the canonical local summary of what we still know and what we can re-derive.

Implementation handoff artifacts:

- [unlawful-shear-microstructure-spec.md](/Users/jackreid/go/polymarket-agent/docs/research/unlawful-shear-microstructure-spec.md)
- [unlawful-shear-signal-pack-spec.md](/Users/jackreid/go/polymarket-agent/docs/research/unlawful-shear-signal-pack-spec.md)
- [unlawful_signal_pack.json](/Users/jackreid/go/polymarket-agent/data/research/wallet_research/unlawful-shear/unlawful_signal_pack.json)

## Surviving Local Data

Primary source:

- [wallet_research.db](/Users/jackreid/go/polymarket-agent/data/research/wallet_research/unlawful-shear/wallet_research.db)

Tables still populated:

- `wallet_activity_raw`
- `wallet_orderbook_snapshots`
- `wallet_market_stream_events`
- `wallet_window_reconstructions`
- `wallet_closed_positions_raw`
- `wallet_accounting_snapshots`
- `wallet_market_catalog`
- `wallet_btc_price_series`
- `collection_runs`
- `wallets`

Current local row counts:

- `wallet_activity_raw`: `241,243`
- `wallet_market_stream_events`: `265,556`
- `wallet_orderbook_snapshots`: `646`
- `wallet_window_reconstructions`: `531`
- `wallet_closed_positions_raw`: `137`
- `wallet_accounting_snapshots`: `20`

Time coverage:

- raw activity: March 20, 2026 11:40:15 UTC to April 23, 2026 12:47:18 UTC
- orderbook snapshots: April 23, 2026 10:54:08 UTC to April 23, 2026 12:47:42 UTC
- market stream events: April 23, 2026 10:54:08 UTC to April 23, 2026 12:31:39 UTC

## Confirmed Wallet-Level Economics

From live API / chain work done in this session:

- the wallet is maker-active on Polymarket
- public rebate endpoint showed material daily rebates on:
  - April 20, 2026: about `$5,347.40`
  - April 21, 2026: about `$6,368.89`
  - April 22, 2026: about `$6,603.64`
- Polygon `USDC.e` flow showed:
  - outbound to CTF Exchange `0x4bfb41d5...`: exchange spend / order matching
  - inbound from Conditional Tokens `0x4d97dcd9...`: settlement / merge recycling

Interpretation:

- this wallet is not a pure taker replica target
- its edge includes maker economics
- but that does not mean its intrawindow behavior is “passive only”

## Funding / Starting Capital

Observed shared external funder:

- `0xf70da97812cb96acdf810712aa562db8dfa3dbef`

Important clarification:

- on Polygon, this address is not a Safe or other contract wallet
- `eth_getCode` returned `0x`
- so treat it as a shared treasury / operator EOA unless proven otherwise

Visible early external funding into `unlawful-shear` before serious exchange flow:

- `$9.93`
- `$298.76`
- `$497.95`
- `$995.90`

Visible seed before heavy exchange use: about `$1.80k`

Total external funding from that shared wallet across fetched history:

- about `$2.11k`

Other inbound flow is mostly operational:

- from CTF Exchange: small execution-linked inflows
- from FeeModule: tiny fee-linked inflows
- from Conditional Tokens: settlement / merge recycling

Interpretation:

- `unlawful-shear` appears to have started with very little visible seed capital
- the wallet then relied heavily on rapid recycle through exchange and CTF settlement flows
- this is one of the strongest reasons not to over-index on bankroll as the bottleneck

## Core Intrawindow Findings

### Trade shape

Across local activity history:

- `230,702` trade rows
- `230,702` buy rows
- `0` sell rows
- `725` distinct markets

This is consistent with buy/buy inventory formation rather than visible sell-side quote realization in the local activity feed.

### Window reconstruction

From `wallet_window_reconstructions`:

- `531` reconstructed windows total
- `493` true paired windows with both outcomes bought

Average across paired windows:

- buy rows per window: `374.71`
- merge rows per window: `14.04`
- average up-leg buy price: `0.5071`
- average down-leg buy price: `0.4901`
- average hedge-cost ratio: `0.4546`

This is not one-shot directional entry. It is repeated clip execution with meaningful pair recycling.

### Cheap leg / expensive leg geometry

Grouped by which side was cheaper:

- when `Down` was the cheap leg:
  - windows: `262`
  - average cheap price: `0.3054`
  - average expensive price: `0.6995`
  - average gap: `0.3941`
  - average hedge ratio: `0.4574`
- when `Up` was the cheap leg:
  - windows: `231`
  - average cheap price: `0.2889`
  - average expensive price: `0.6997`
  - average gap: `0.4107`
  - average hedge ratio: `0.4513`

Interpretation:

- the cheap leg is usually around `0.29-0.31`
- the expensive leg is usually around `0.70`
- the wallet does not behave like a symmetric 50/50 complement buyer
- the wallet behaves like expensive-core plus cheap-hedge inventory shaping

### Fit to the current paper profile

Against paired windows:

- cheap leg `<= 0.38`: `349 / 493`
- expensive leg between `0.52` and `0.92`: `459 / 493`
- expensive-cheap gap `>= 0.12`: `429 / 493`
- hedge ratio between `0.20` and `0.60`: `254 / 493`

This is why the current `unlawful_shear` paper profile is directionally reasonable rather than arbitrary.

## Working Reconstruction

Best current reconstruction:

1. Maker-first at the wallet level.
2. Two-sided by default at the market level.
3. Within a market, builds an expensive core leg and a cheaper hedge leg.
4. Executes in many clips, not one entry.
5. Rebalances the pair as relative leg cost drifts.
6. Merges paired inventory repeatedly to recycle capital.
7. Accelerates behavior late in the window.
8. Salvages loser inventory rather than holding everything blindly.

This means the wallet is likely:

- complement-maker or maker-first inventory accumulator
- with active inventory shaping layered on top
- not a “quote both sides and do nothing else” bot

## Current Local Engine Mapping

The current Rust paper implementation already encodes this reconstruction in rough form.

References:

- [whale-pair-exec/README.md](/Users/jackreid/go/polymarket-agent/whale-pair-exec/README.md)
- [whale-pair-exec/src/strategy.rs](/Users/jackreid/go/polymarket-agent/whale-pair-exec/src/strategy.rs)
- the current `unlawful_shear` runtime path is best treated as built-in Rust defaults plus env overrides; the older JSON profile path is no longer canonical on this branch

Current encoded profile:

- cheap hedge max: `0.38`
- core band: `0.52-0.92`
- min gap: `0.12`
- probe clip: `3`
- core clip: `20`
- hedge clip: `6`
- rebalance clip: `12`
- target hedge ratio band: `0.20-0.60`
- salvage drawdown ratio: `0.18`
- salvage bid floor: `0.05`
- cooldown: `400 ms`

README summary of the intended behavior:

- two-sided default participation
- expensive-core / cheap-hedge accumulation
- repeated clip-based rebalance
- late-window acceleration
- paired loser salvage with `reduce_only` sells

## Where The Real Microstructure Still Lives

If we want to rebuild the engine properly, the most valuable surviving local sources are:

- `wallet_market_stream_events`
  - event-level book / trade stream context
- `wallet_orderbook_snapshots`
  - explicit book state for captured periods
- `wallet_window_reconstructions`
  - window-level paired geometry

These are the tables to use to infer:

- clip cadence
- passive versus aggressive interaction against the book
- how often he steps up price
- how inventory changes through the 5m window
- when merges happen relative to pair completion
- what “late-window acceleration” actually means in timing terms

## What We Still Do Not Know Exactly

We do not have:

- every historical hidden quote placement
- authenticated user-channel history for the wallet
- perfect fill-level maker/taker labeling on every trade row

So the correct goal is:

- reconstruct the strategy class and decision geometry
- then calibrate the hidden execution details using our own tiny-live deployment

## Recommended Reverse-Engineering Order

1. Rebuild a canonical per-window dataset from:
   - `wallet_window_reconstructions`
   - `wallet_market_stream_events`
   - `wallet_orderbook_snapshots`
2. Infer clip sizing and cadence.
3. Infer pair-completion and merge timing.
4. Infer late-window transition rules.
5. Compare those findings against the current `unlawful_shear` paper profile.
6. Promote the economic layer into maker-first live execution once the execution plane is ready.

## Useful Commands

Trade shape:

```bash
sqlite3 -json data/research/wallet_research/unlawful-shear/wallet_research.db \
  "select count(*) as trades,
          sum(case when side='BUY' then 1 else 0 end) as buy_rows,
          sum(case when side='SELL' then 1 else 0 end) as sell_rows,
          count(distinct slug) as markets
   from wallet_activity_raw
   where activity_type='TRADE';"
```

Window summary:

```bash
sqlite3 -json data/research/wallet_research/unlawful-shear/wallet_research.db \
  "select paired_outcomes,
          count(*) as windows,
          round(avg(buy_rows),2) as avg_buy_rows,
          round(avg(merge_rows),2) as avg_merge_rows,
          round(avg(up_avg_buy_price),4) as avg_up_price,
          round(avg(down_avg_buy_price),4) as avg_down_price,
          round(avg(hedge_cost_ratio),4) as avg_hedge_ratio
   from wallet_window_reconstructions
   group by paired_outcomes
   order by paired_outcomes desc;"
```

Cheap/core threshold fit:

```bash
sqlite3 -json data/research/wallet_research/unlawful-shear/wallet_research.db \
  "select count(*) as paired_windows,
          sum(case when cheap_leg_avg_price <= 0.38 then 1 else 0 end) as cheap_leq_038,
          sum(case when expensive_leg_avg_price between 0.52 and 0.92 then 1 else 0 end) as expensive_core_band,
          sum(case when (expensive_leg_avg_price - cheap_leg_avg_price) >= 0.12 then 1 else 0 end) as gap_ge_012,
          sum(case when hedge_cost_ratio between 0.20 and 0.60 then 1 else 0 end) as hedge_ratio_band
   from wallet_window_reconstructions
   where paired_outcomes=1;"
```

## Bottom Line

`unlawful-shear` remains the best local reconstruction target because:

- maker economics are confirmed
- local microstructure data still exists
- the current paper strategy already encodes a large part of the observed geometry

The right mental model is:

- maker-first wallet
- with active inventory shaping and recycle behavior
- not a simple passive MM and not a simple taker clone
