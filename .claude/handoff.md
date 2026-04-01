# Handoff: Polymarket BTC Sniper — Phase 2B

## Current State
- Branch: `feat/btc-sniper-phase1` (pushed to `main` on `0xj7r/polymarket-btc-sniper`)
- Last commit: `fc2d8e0 feat: Strategy B (midrange directional), Polymarket CLOB WebSocket, rename engine`
- Tests: 89 passing
- Paper trading: LIVE on Hetzner 188.34.177.202 but ZERO trades executed (signal never confident enough)
- Bot DB is empty — not logging signal data between trades

## Critical Discovery: PolyBackTest API

We found the missing data source: `api.polybacktest.com` provides sub-second snapshots of BTC price + token prices (price_up, price_down) throughout live 5-minute windows. This is the data we need for proper backtesting.

API key: `pdm_CSreRRkeODfhuBa7rbTqZmnyTliLtixT` (free tier, 50 markets)
Auth: `X-API-Key` header
Docs: https://docs.polybacktest.com/api-reference/endpoint/get-market-by-slug
Base: `https://api.polybacktest.com`

Key endpoints:
- `GET /v2/markets?coin=btc&market_type=5m` — list resolved markets
- `GET /v2/markets/{market_id}/snapshots?coin=btc` — sub-second token price history
- Snapshot fields: `time`, `btc_price`, `price_up`, `price_down`
- Free tier: 50 most recent 5m markets, ~2500 snapshots each

## URGENT Tasks (do these first)

### 1. Log signal data every tick
The bot runs but logs NOTHING to the DB between trades. Add periodic logging (every 60s) of: P(UP), log_odds, btc_price, up_token_price, down_token_price, window_id. This is essential for autoresearch.
- File: `core/engine.py` — in the `tick_count % 600` block, add `self.memory.save_event()`

### 2. Rebuild backtester on PolyBackTest data
Replace `backtesting/historical_data.py` to pull from PolyBackTest API instead of Gamma + Binance klines separately. The snapshot data includes BOTH btc_price and token prices at each moment — exactly what we need.

New flow:
```
PolyBackTest /v2/markets → list of resolved 5m markets
PolyBackTest /v2/markets/{id}/snapshots → sub-second price history
For each market: replay signal engine against snapshot timeseries
Compare signal direction vs actual outcome
Also check: were cheap tokens available? At what timestamp?
```

### 3. Two strategies running in parallel
Strategy A (snipe): entry at <= 5c tokens, needs strong move
Strategy B (midrange): entry at <= 55c tokens, needs 80% confidence, fee-aware Kelly
Both are wired into `core/engine.py._check_entry()`. Strategy A fires first, Strategy B is fallback.

## What's Running
- Hetzner: 188.34.177.202, SSH key `~/.ssh/polymarket_hetzner`
- Docker: both Binance WS (trades + bookTicker) and Polymarket CLOB WS connected
- Health: :8080 endpoint
- Monitoring: hourly Claude trigger with Slack
- Autoresearch cron: Tuesdays 2am UTC (needs real data first)

## Key Files
```
core/engine.py              — main orchestrator (both strategies)
strategies/btc_sniper.py    — signal engine
strategies/strategy_config.py — config with Strategy B params
clients/polymarket_ws.py    — CLOB WebSocket (real-time token prices)
clients/binance_ws.py       — combined trade + bookTicker stream
backtesting/historical_data.py — needs rewrite for PolyBackTest
backtesting/btc_backtest.py — needs rewrite for real snapshot data
autoresearch/program.md     — needs --strategy A|B flag
```

## Infrastructure
- Hetzner skill: `~/.claude/skills/hetzner/config.json`
- Repo: https://github.com/0xj7r/polymarket-btc-sniper
- PolyBackTest key stored at: needs adding to .env on Hetzner

## How to Continue
1. Add signal logging to the live bot (task 1 above), deploy
2. Rewrite backtester to use PolyBackTest API (task 2)
3. Run the real backtester against 50 markets
4. If signal accuracy > 55%, the strategy has edge
5. Then let autoresearch optimize weights
