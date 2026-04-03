# Codex Review: Data Collection Completeness

## Task
Review whether the data we're collecting in Supabase is sufficient for autoresearch and future model development. Identify any gaps.

## Current Schema

### snapshots table (1 row per second per coin per active market)
- coin (btc/eth/sol)
- market_id
- time (timestamptz)
- price (underlying from Binance trades)
- price_up, price_down (Polymarket token prices from CLOB WS)
- bid_price, ask_price (Binance best bid/ask from bookTicker)
- bid_size, ask_size (Binance top-of-book depth)
- spread (price_up + price_down - 1.0)
- elapsed_s (seconds into the 5-min window)

### markets table
- market_id, coin, slug, market_type
- start_time, end_time
- price_start, price_end (underlying at open/close)
- winner (Up/Down)
- final_volume, final_liquidity
- question (market name)

### trades table (paper trades from the bot)
- id, coin, strategy, market_id
- direction, token_price, size_usd, shares
- won, pnl_usd, paper
- btc_price, move_pct
- created_at, resolved_at

### strategy_results table
- coin, strategy_name, params (JSONB)
- train_win_rate, test_win_rate, total_trades

## What We Use It For
1. **Autoresearch**: grid search over threshold/skew/volatility filters to find optimal entry conditions
2. **Feature computation**: move_pct, velocity, consistency, volatility, token_skew, acceleration from raw snapshots
3. **Monte Carlo projections**: observed trade distributions for stochastic simulation
4. **Live paper trading validation**: comparing backtest predictions to actual live signal frequency and fills

## Current Strategy
Simple threshold detection: when coin moves > X% on Binance and Polymarket token price is still cheap (skew <= 0.02, entry <= $0.55), buy the directional token.

## Questions for Review
1. Are we missing any data fields that would enable discovering better strategies?
2. Should we capture Polymarket order book depth (not just Binance)?
3. Would storing trade-by-trade Binance data (instead of 1/sec snapshots) help?
4. Is the 1-second snapshot interval optimal, or should it be faster/slower?
5. Any fields we're storing that are redundant or wasteful?
6. For future ML models, what additional features would we want?
7. Should we store the full Polymarket order book (multiple price levels) not just best prices?

## Supabase Details
- URL: https://vbvpaymtuozugylkuqmw.supabase.co
- Data flowing since Apr 3, 2026 ~09:50 UTC
- ~3 rows/second (1 per coin) = ~260K rows/day
