# Whale Analysis Artifacts
Captured: 2026-04-22T09:41:13.917249+00:00

## Wallets analyzed
- **Unlawful-Shear (w1)** — `0xb27bc932bf8110d8f78e55da7d5f0497a18b5b82` — https://polymarket.com/@0xb27bc932bf8110d8f78e55da7d5f0497a18b5b82
- **xuanxuan008 (w2)** — `0xcfb103c37c0234f524c632d964ed31f117b5f694` — https://polymarket.com/@0xcfb103c37c0234f524c632d964ed31f117b5f694
- **SPLIT/SELL (w3)** — `0xe51b3d64da5b0b8a07a55f8bb3c3170237f73cad` — https://polymarket.com/@0xe51b3d64da5b0b8a07a55f8bb3c3170237f73cad
- **penny-tail (w4)** — `0x7da07b2a8b009a406198677debda46ad651b6be2` — https://polymarket.com/@0x7da07b2a8b009a406198677debda46ad651b6be2

## Files

### `pnl_timeseries_<last6>.json`
Raw time-series P&L from Polymarket's internal endpoint:
```
GET https://user-pnl-api.polymarket.com/user-pnl?user_address=<wallet>&interval=all
```
Each point: `{"t": unix_ts, "p": pnl_usdc}`. Hourly fidelity. This is the same
series that powers the green/purple P&L chart on each profile page.

### `daily_pnl_<last6>.json`
End-of-UTC-day P&L snapshots derived from the time series above. Useful for
day-level comparison across wallets.

### `activity_<last6>.json`
Raw activity rows from `data-api.polymarket.com/activity?user=<wallet>`, up to
the 3500-row API cap (newest first). Each row is a TRADE / MERGE / SPLIT / REDEEM
event with conditionId, slug, price, size, usdcSize, outcome, transactionHash.

### `comparison.json`
Aggregate stats per wallet (current P&L, peak, drawdown, up/down days, best/worst days).

## Strategy labels inferred from activity

| Label | Pattern | Edge mechanism |
|-------|---------|----------------|
| Unlawful-Shear (w1) | spray BUYs on both sides < $0.50, MERGE paired fills | bid/ask spread capture via pair-merge arb |
| xuanxuan008 (w2) | same as w1, smaller per-window size | same |
| SPLIT/SELL (w3) | SPLIT $1 USDC → sell one side > $0.50, hold other to REDEEM | implied-prob spread when Up+Down > $1 |
| penny-tail (w4) | single-side BUYs at $0.01-$0.03 in last 60s of window | tail/convexity — buy cheap optionality |

Our live bot runs closest to w4's regime but with `max_entry=0.55` instead of ≤$0.03.
