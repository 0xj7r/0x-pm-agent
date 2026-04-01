# Handoff: Polymarket BTC Sniper — Strategy Rebuild

## Critical Insight

The current signal engine (weighted Bayesian log-odds) does NOT work. 50 autoresearch iterations, zero positive configs. The fundamental problem: **cheap tokens are on the LOSING side** (market already repriced), and our signal agrees with the market (both say UP), so we'd buy UP at 95c not DOWN at 5c.

The profitable wallets (0x8dxd, 0xd9e0aa...) use a simpler approach:

```
1. BTC moves on Binance
2. Buy the WINNING side on Polymarket before the book fully reprices
3. Collect $1 per share on resolution
```

No complex signal. Pure latency arbitrage. They enter at 25-55c (mid-range, reasonable prices) BEFORE the Polymarket book reflects the Binance move. The edge is SPEED, not prediction.

## What To Build

Replace the signal engine with:
```python
btc_move = (current_btc - window_open_btc) / window_open_btc * 100
if abs(btc_move) > MOVE_THRESHOLD:  # e.g., 0.03%
    direction = "UP" if btc_move > 0 else "DOWN"
    token_price = get_live_price(direction_token)
    if token_price < MAX_ENTRY_PRICE:  # e.g., 0.55
        buy(direction_token, kelly_size(token_price))
```

One parameter to tune (MOVE_THRESHOLD). Entry price determines payoff. Speed determines what price you get.

## Backtest Validation Needed

Using PolyBackTest snapshot data (10 markets in DB, can fetch 50 on free tier):
- For each market, find the moment BTC first moved >0.03% from open
- What was the winning token price at that moment?
- Would buying it have been profitable after fees?

This is the core validation. If the winning token is still at 50-55c when we detect the move, the strategy works (buy at 55c, collect $1, minus fees).

## Infrastructure (all working)
- Hetzner: 188.34.177.202, SSH key ~/.ssh/polymarket_hetzner
- Bot: Docker, Binance WS (trades + bookTicker), Polymarket CLOB WS, health :8080
- Data: 10 markets + 25,868 snapshots in backtesting/historical.db
- Repo: https://github.com/0xj7r/polymarket-btc-sniper
- Monitoring: hourly Claude trigger with Slack
- PolyBackTest API key: ***POLYBACKTEST_KEY_REMOVED***
- Anthropic API key on Hetzner: ***ANTHROPIC_KEY_REMOVED***

## Key Files
```
core/engine.py              — main loop (keep, replace entry logic)
strategies/btc_sniper.py    — signal engine (rewrite to simple threshold)
clients/binance_ws.py       — Binance WS with bookTicker (keep)
clients/polymarket_ws.py    — CLOB WS real-time prices (keep)
clients/market_scanner.py   — slug-based discovery (keep)
backtesting/historical_data.py — PolyBackTest fetcher (keep)
backtesting/btc_backtest.py — snapshot replayer (rewrite for new strategy)
strategy_config.json        — simplify to just move_threshold + max_entry_price
```

## Reference Wallets
- 0x8dxd: https://polymarket.com/profile/%400x8dxd — $313 to $2.38M, 98% win rate
- 0xd9e0aa: https://polydata.org/portfolio/0xd9e0aaca471f489be338fd0f91a26e8669a805f2 — $62 to $721K, 100% win rate
- Both use latency arb on BTC 5m/15m markets, entering at 25-55c

## How to Continue
1. Fetch 50 markets from PolyBackTest: `python backtesting/historical_data.py --limit 50`
2. Validate: for each market, when BTC first moves >0.03%, what's the winning token price?
3. If profitable: rewrite signal engine to simple move threshold
4. Backtest the new strategy
5. Deploy to Hetzner
6. Paper trade for 48h
7. Go live
