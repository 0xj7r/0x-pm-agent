# Handoff: Polymarket BTC Sniper — Phase 2

## Current State
- Branch: `feat/btc-sniper-phase1` (pushed to `main` on `0xj7r/polymarket-btc-sniper`)
- Last commit: `76bcb63 fix: scanner uses slug generation to find 5-min BTC markets via Gamma events API`
- Uncommitted changes: none
- Tests: 70 passing, 0 failing
- Paper trading: LIVE on Hetzner 188.34.177.202 (Docker, port 8080 health endpoint)
- Monitoring: hourly Claude scheduled trigger (trig_01BfchzYWTbNqDSoCDMqmY4S) checks health + Slack
- Autoresearch cron: set for every Tuesday 2am UTC starting April 8 on Hetzner

## What's Been Done (Phase 1)
- Bayesian signal engine (`strategies/btc_sniper.py`) — set_state (snapshot, not accumulate) to prevent runaway log_odds
- Binance WebSocket client (`clients/binance_ws.py`) — real-time BTC/USDT trades with auto-reconnect
- Market scanner (`clients/market_scanner.py`) — discovers 5-min markets via slug pattern `btc-updown-5m-{unix_ts}` on Gamma /events endpoint
- Asymmetric Kelly sizing (`core/risk.py`) — aggressive sizing for cheap tokens (2-5c) with capped downside
- Trading engine (`core/btc_engine.py`) — orchestrates signal → entry check → paper trade → resolution
- Resolution checker (`core/resolver.py`) — polls Gamma for resolved markets, calculates P&L with taker fees
- Health endpoint (`core/health.py`) — HTTP :8080 JSON status for remote monitoring
- Slack notifications (`core/notifier.py`) — trades, resolutions, errors, hourly status via webhook
- Autoresearch harness (`autoresearch/program.md` + `run_backtest.py`) — Karpathy pattern with domain knowledge, composite scoring, stuck-recovery
- Docker deployment + Hetzner skill (`~/.claude/skills/hetzner/`)
- Code review: 2 of 4 "critical" findings were false positives (balance math and Kelly formula both correct). Real fixes: bounded signal, adversarial backtest data, entry window gating, taker fees, SQLite WAL mode

## What's Left (Phase 2 — execute in parallel)

### 1. Real Backtester (HIGHEST PRIORITY)
Current backtester uses synthetic data. Need to fetch historical resolved BTC 5-min markets from Gamma + historical Binance klines, replay the signal engine against actual outcomes. Required before autoresearch produces meaningful results on April 8.
- Fetch resolved markets: `GET https://gamma-api.polymarket.com/events?slug=btc-updown-5m-{ts}` for past timestamps
- Fetch Binance klines: `GET https://api.binance.com/api/v3/klines?symbol=BTCUSDT&interval=1m&startTime=X&endTime=Y`
- Replay signal engine with real price_delta and OFI from kline data
- Compare signal direction against actual resolution (Up/Down)
- Files: `backtesting/btc_backtest.py` (modify), `backtesting/historical_data.py` (create)

### 2. Live CLOB Order Execution
Wire up real order placement when `paper=False`. The `place_order` method exists in `clients/polymarket.py`. Need to:
- Call it from `btc_engine.py` when `cfg.paper.enabled is False`
- Handle partial fills, order confirmation, error cases
- Requires funded wallet (USDC on Polygon) + private key in `.env`
- Human gate before enabling

### 3. Polymarket Order Book WebSocket
Currently token prices come from Gamma at scan time (every 30s). Stale tokens at 2c may exist for only seconds. Need real-time CLOB order book data.
- Polymarket CLOB WebSocket for live bid/ask
- Update `current_window.up_price` / `down_price` in real-time
- Detect when cheap tokens appear and trigger entry immediately
- Files: `clients/polymarket_ws.py` (create), wire into `btc_engine.py`

### 4. Microprice Signal
The `w2_microprice` weight is permanently multiplied by zero (hardcoded `microprice_dev = 0.0` in btc_engine.py:93). Need to:
- Subscribe to Binance `@bookTicker` stream for best bid/ask + sizes
- Compute microprice: `(bid_size * ask + ask_size * bid) / (bid_size + ask_size)`
- Feed deviation from mid into signal engine
- Files: `clients/binance_ws.py` (extend), `core/btc_engine.py` (wire microprice)

### 5. Historical Data Pipeline
Automated collection of resolved markets + outcomes for ongoing backtesting.
- Cron job that fetches yesterday's resolved BTC 5-min markets
- Stores: market_id, slug, start_time, end_time, resolution (Up/Down), token prices at various times
- Binance kline data for each window
- Files: `backtesting/historical_data.py` (create), cron on Hetzner

### 6. Autonomous Researcher Deployment
Code exists (`researcher/researcher.py` + `run_research.py`) but untested end-to-end. Need to:
- Verify Claude API call works for edge scanning
- Test the Slack/Twitter/GitHub scanning prompts
- Set up daily cron on Hetzner
- Wire findings into autoresearch context

## Key Decisions
- Signal engine uses `set_state` (snapshot) not `+= delta` (accumulate) — prevents O(N²) runaway with thousands of Binance trades per window
- Markets are 5-minute windows with slug pattern `btc-updown-5m-{unix_timestamp}` — round down to nearest 5-min boundary
- Balance accounting: `balance += cost + pnl` at resolution (mathematically verified correct, reviewer was wrong)
- Kelly formula `(p - c) / (1 - c)` is correct for binary bets (also verified, reviewer was wrong)
- Confidence threshold lowered to 0.75 for faster data collection during paper phase
- Autoresearch cadence: weekly (Tuesdays 2am UTC), not nightly — need statistical mass before tuning
- Backtest synthetic data includes adversarial cases (reversals, repriced books) — 15% of large moves are reversals

## Key Files
```
core/btc_engine.py          — main orchestrator (300 lines)
strategies/btc_sniper.py    — signal engine
clients/market_scanner.py   — 5-min market discovery via slug generation
clients/binance_ws.py       — Binance WS (extend for bookTicker)
core/resolver.py            — paper trade resolution
core/risk.py                — Kelly sizing (asymmetric_kelly_size method)
strategy_config.json        — tunable params (autoresearch mutates this)
autoresearch/program.md     — autoresearch agent instructions
autoresearch/run_backtest.py — backtest CLI
```

## Infrastructure
- Hetzner: 188.34.177.202, SSH key `~/.ssh/polymarket_hetzner`, user root
- Hetzner skill: `~/.claude/skills/hetzner/config.json` has API token + project config
- Repo: https://github.com/0xj7r/polymarket-btc-sniper
- Docker: `docker compose up -d` in `/opt/polymarket-agent/`
- Claude CLI installed on Hetzner for autoresearch

## How to Continue
Start all 6 Phase 2 tasks in parallel. Tasks 1+5 are the critical path (real backtester needs historical data). Tasks 3+4 improve signal quality. Task 2 is independent (live execution wiring). Task 6 is lowest priority.
