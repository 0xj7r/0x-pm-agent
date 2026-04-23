# Unlawful-Shear wallet research pipeline

This pipeline is focused on wallet `0xb27bc932bf8110d8f78e55da7d5f0497a18b5b82`
(`unlawful-shear`), the BTC 5m wallet we are trying to reverse-engineer.

## What this pipeline stores

The first pass captures the datasets that matter most for strategy analysis:

- `wallet_activity_raw`
  - historical Polymarket wallet activity rows
  - trades, sells, merges, timestamps, prices, sizes
- `wallet_closed_positions_raw`
  - closed-position outcomes and realized PnL
- `wallet_accounting_snapshots`
  - accounting snapshot archive from Polymarket `data-api`
- `wallet_market_catalog`
  - market metadata for all slugs touched by the wallet
- `wallet_orderbook_snapshots`
  - current CLOB top-of-book/depth snapshots for token ids seen in touched markets
- `wallet_market_stream_events`
  - live CLOB WebSocket events for trade-adjacent market conditions
  - `book`, `price_change`, `best_bid_ask`, and `last_trade_price` updates
  - persisted with bid/ask/spread, last trade price, and book levels when present
- `wallet_btc_price_series`
  - BTC/USDT Binance bars for the wallet's active time range
- `wallet_window_reconstructions`
  - derived one-row-per-BTC-5m-window analysis

Schema source:

- [wallet_research_schema.sql](/Users/jackreid/go/polymarket-agent/research/dataops/wallet_research_schema.sql)

## Why these datasets matter

To get closer to replicating this wallet, we need the market state it saw when it
traded, not just the final PnL screenshot.

These tables let us answer:

- how early or late in the 5m window it enters
- whether it carries both sides in the same market
- whether one side is a cheap convex hedge and the other a large core leg
- whether losing legs are sold, merged, or held toward zero
- what the BTC path looked like during the window
- which windows are actually profitable when both sides are combined

## Run the collector

Backfill the wallet's historical activity range and build the local research DB:

```bash
python3 research/dataops/collect_wallet_research.py \
  --wallet 0xb27bc932bf8110d8f78e55da7d5f0497a18b5b82 \
  --start-date 2026-03-20 \
  --end-date 2026-04-23 \
  --bucket-hours 12 \
  --binance-interval 1m
```

Outputs are written under:

- `data/research/wallet_research/unlawful-shear/raw_capture/`
- `data/research/wallet_research/unlawful-shear/wallet_research.db`
- `data/research/wallet_research/unlawful-shear/collection_summary.json`

## Reconstruct BTC 5m windows

Build the correct unit of analysis for this wallet: one row per BTC 5m market
window, combining both sides.

```bash
python3 research/analysis/reconstruct_wallet_windows.py \
  --wallet 0xb27bc932bf8110d8f78e55da7d5f0497a18b5b82 \
  --family-prefix btc-updown-5m-
```

Output:

- `data/research/wallet_research/unlawful-shear/btc_5m_window_reconstruction.json`

## Start forward live capture

Historical backfill is not enough because the hard missing dataset is
trade-adjacent book state. Start this collector and leave it running:

```bash
python3 research/collector/wallet_live_capture.py \
  --wallet 0xb27bc932bf8110d8f78e55da7d5f0497a18b5b82 \
  --family-prefix btc-updown-5m- \
  --poll-seconds 15 \
  --accounting-seconds 300
```

Outputs:

- `data/research/wallet_research/unlawful-shear/live_capture/*.json`
- inserts into:
  - `wallet_activity_raw`
  - `wallet_market_catalog`
  - `wallet_market_stream_events`
  - `wallet_orderbook_snapshots`
  - `wallet_accounting_snapshots`

This is the mechanism that lets us keep monitoring unlawful-shear and build the
trade-adjacent market-state dataset we are currently missing historically.

The live collector now does two separate things for Polymarket market conditions:

- subscribes to touched BTC 5m token ids over the CLOB WebSocket and persists
  streaming quote/trade events into `wallet_market_stream_events`
- keeps taking periodic REST `/book` snapshots into `wallet_orderbook_snapshots`
  so we retain a full top-of-book view even if some stream events are missed

## Recommended next storage target

The collector currently writes to a local SQLite database because that is the
fastest way to get the historical pipeline working inside the repo.

The next deployment target should be AWS:

- `RDS Postgres` for the normalized tables above
- `S3` for raw payload archives such as:
  - activity windows
  - accounting snapshot zips
  - large market/book payloads

The intended sequence is:

1. get the SQLite/local pipeline working
2. move the same schema to Postgres on AWS
3. push raw capture artifacts to S3
4. run the live collector remotely so book/account snapshots continue without local dependence

The SQL schema is Postgres-friendly; it can be used as the base DDL for the AWS
database with only light type/index adjustments.

## Current limitations

- historical full-depth orderbook snapshots can only be captured going forward
  unless we already archived them elsewhere
- the collector backfills current market metadata cleanly, but historical
  trade-adjacent book state still needs live capture or a richer archived source
- BTC bars are currently pulled from Binance REST; if we need second-level
  resolution, add a dedicated trade/tick collector next

## Immediate follow-up

After backfill, the next analysis should measure:

- combined PnL by BTC 5m window
- hedge cost ratio between cheap and expensive legs
- loss crystallization rate
- sell vs merge vs hold-to-resolution patterns
- entry timing conditional on BTC movement and market spread
