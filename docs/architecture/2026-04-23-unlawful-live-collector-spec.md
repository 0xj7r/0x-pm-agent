# Unlawful Live Collector Spec

Status: implementation handoff  
Date: 2026-04-23  
Primary target: collector service for `unlawful-shear`  
Secondary target: extend later to `Bonereaper`

## 1. Purpose

We need a live collector for `unlawful-shear` even while the paper sleeve is being built.

Reason:

- the local historical pack is useful but incomplete
- session timing is drifting relative to the earlier historical sample
- we need fresh book, trade, and market-context evidence to keep calibrating the runtime

This collector is not a copy-trading system.

It is a research feedback loop.

## 2. Collector Goals

The collector must answer:

1. Which BTC 5m windows is `unlawful` participating in today?
2. How early does he enter each window?
3. How long do trades continue?
4. How long do merges / cleanup continue after the main trading burst?
5. What price geometry is present at entry and during rebalance?
6. Which session blocks are actually active in live conditions?

## 3. Data Sources

### Required

- Polymarket `activity` API for the wallet
- Gamma market lookup by slug
- Polymarket market websocket for touched token IDs
- external BTC spot feed

### Nice to have

- rebate endpoint
- Polygon settlement / funding reconciliation

## 4. Collection Cadence

### Wallet activity polling

Poll:

- every `10s` during active collection

Keep:

- last seen transaction hash set
- last seen timestamp per slug

### Market websocket capture

Subscribe when:

- a new BTC 5m slug appears in wallet activity
- or when the scheduler predicts a likely active next window

Keep capture window:

- from `window_start - 30s`
- through `window_end + 180s`

This covers:

- pre-entry state
- entry burst
- immediate cleanup / merges

### BTC spot capture

Continuous rolling capture:

- `1s` buckets are sufficient for v1

## 5. Outputs

Write one directory per day, for example:

- `data/research/live_collectors/unlawful/2026-04-23/`

Per-window files:

- `activity.json`
- `book.jsonl`
- `market_ws.jsonl`
- `btc_spot.jsonl`
- `summary.json`

Daily summary:

- `session_summary.json`

## 6. Required Summary Fields

Per window:

- slug
- window start/end
- first trade timestamp
- last trade timestamp
- first merge timestamp
- last merge timestamp
- trade count
- merge count
- redeem count
- average up price
- average down price
- cheap leg average
- expensive leg average
- cheap/expensive notional ratio
- gap from previous window trade end
- overlap with previous window cleanup

Per day:

- active hours
- windows traded
- windows skipped
- windows with merges
- windows without merges
- avg entry lag
- avg cleanup lag

## 7. Runtime Relationship

The collector should be separate from `whale-pair-exec`.

It may write context artifacts the runtime consumes later, but it must not block the runtime loop.

## 8. Implementation Shape

Recommended v1:

- Python collector under `scripts/` or a dedicated `research/` module
- standard JSON/JSONL outputs
- separate cron or long-running process

Recommended later:

- small daemon with explicit state and retention policy

## 9. Immediate Scope

Implement first for:

- `unlawful-shear`

Then extend to:

- `Bonereaper`

Shared collector code should be written so the wallet address is configurable.

## 10. Why This Matters

Without this collector, the strategy build will drift toward stale assumptions:

- hard-coded session hours
- outdated price geometry
- incomplete cleanup assumptions

With the collector, we can keep recalibrating:

- session preferences
- entry timing
- merge timing
- rolling market concurrency
