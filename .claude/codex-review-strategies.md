# Codex Review: Alternative Strategy Evaluation

## Task
Evaluate which of these alternative strategies should be trialled alongside our current latency arb approach. Consider feasibility, edge quality, data requirements, and risk profile. Recommend which to implement first.

## Current Strategy (baseline)
**Latency arbitrage**: When coin moves > 0.08% on Binance and the Polymarket token hasn't repriced (skew <= 0.02, entry <= $0.55), buy the directional token. Backtest: 87-98% win rate. Currently paper trading but no trades have fired in 16 hours due to low BTC volatility.

## Candidate Strategies

### 1. Mean Reversion / Contrarian
**Idea**: When the token price overshoots (e.g. UP at $0.85 after a small BTC move of only 0.03%), bet against it. The market overreacts to small moves and BTC often reverses within the 5-min window.

**Entry**: Token directional price > $0.75 but BTC move < 0.05%. Buy the cheap side.
**Exit**: Token resolves to $1 (win) or $0 (loss).
**Edge thesis**: Polymarket participants chase small moves, creating overpriced tokens. BTC mean-reverts within 5 minutes.
**Risk**: If BTC continues in the same direction, you lose your entire entry.
**Data needed**: Already have it (price, token prices, elapsed time).

### 2. Liquidity Provision / Market Making
**Idea**: Place limit orders on both sides (e.g. buy UP at $0.45, buy DOWN at $0.45). When both fill, total cost is $0.90, guaranteed $1 payout = $0.10 profit.

**Entry**: Limit orders, not market orders. Need both sides to fill.
**Exit**: Resolution.
**Edge thesis**: Spread capture. No directional bet.
**Risk**: One-sided fill leaves you with directional exposure. Inventory risk.
**Data needed**: Order book depth on Polymarket side (we don't have this yet, only Binance book). Would need Polymarket CLOB order book data.
**Implementation**: Requires the CLOB client to place limit orders (we have this via py-clob-client).

### 3. Resolution Sniping (Late Window)
**Idea**: In the last 30 seconds of a window, BTC direction is 95%+ certain. Buy the winning token even at $0.85-$0.95. Small profit but very high win rate.

**Entry**: elapsed_s > 270 (last 30 seconds). Buy direction matching current BTC move.
**Exit**: Resolution 30 seconds later.
**Edge thesis**: Direction is known with near-certainty. You're just collecting the remaining premium.
**Risk**: BTC reverses in the last seconds. But at 270+ seconds, the move has been established.
**Data needed**: Already have it. Our backtesting feature store has elapsed_pct.
**Concern**: At $0.90 entry with 2% fee, you need 93%+ win rate to break even. Tight margin.

### 4. Cross-Market Arbitrage
**Idea**: If UP + DOWN < $1.00, buy both. Guaranteed profit on resolution.

**Entry**: When price_up + price_down < 0.98 (accounting for fees).
**Exit**: Resolution. One side pays $1, the other $0. Net = $1 - total_cost.
**Edge thesis**: Risk-free arbitrage from spread mispricings.
**Risk**: Execution risk (filling both sides before the mispricing closes). Fee drag.
**Data needed**: We already record price_up + price_down. Can check spread column in Supabase for historical frequency of mispricings.
**Concern**: These mispricings are likely rare and small. Other bots probably arb this already.

### 5. Flow Prediction (Copy Trading)
**Idea**: Monitor on-chain trades from known profitable wallets (0x8dxd, 0xd9e0aa). When they buy, follow immediately.

**Entry**: Detect on-chain buy from a tracked wallet. Mirror the trade.
**Exit**: Resolution.
**Edge thesis**: Profitable wallets have proven edge. Following them captures some of that edge.
**Risk**: Latency (on-chain detection is slow, 2-5 seconds). The profitable wallets may be using latency arb themselves, so by the time you detect their trade, the opportunity is gone.
**Data needed**: Polygon RPC for real-time trade detection. Wallet watchlist.
**Implementation**: New module. Not trivial.

### 6. Multi-Window Momentum
**Idea**: If BTC has moved consistently up across the last 3-5 windows, bias the next window toward UP. Enter earlier at better prices because you have a directional view before the window starts.

**Entry**: At window open, if trailing 3-window trend is consistent, buy the trending direction at $0.50-$0.52.
**Exit**: Resolution.
**Edge thesis**: Short-term BTC momentum persists. If BTC has been going up for 15-25 minutes, the next 5 minutes are more likely up.
**Risk**: Momentum breaks. Mean reversion kicks in. You're buying at $0.50 with only a slight edge.
**Data needed**: Already have it. Cross-window BTC price data from our collector.
**Concern**: Momentum in 5-min windows is weak. Our backtesting showed 64% direction agreement which is below breakeven for mid-priced entries.

## Context
- We're currently paper trading the latency arb strategy but getting zero trades due to low volatility
- We have a snapshot collector running on Supabase recording BTC/ETH/SOL prices + Binance order book + Polymarket token prices every second
- We have a feature store that can backtest strategies in seconds
- We have autoresearch running daily to discover new strategies
- Market structure: 5-minute binary options on BTC/ETH/SOL, tokens cost $0-$1, winner gets $1

### 7. 15-Minute / 1-Hour Market Extension
**Idea**: Apply the same latency arb strategy to 15-min and 1-hour BTC/ETH/SOL markets. Longer windows = larger BTC moves = more opportunities that clear the threshold.

**Entry**: Same threshold logic but calibrated for longer windows (threshold might be higher, e.g. 0.15% for 15-min).
**Edge thesis**: Same structural edge (Polymarket book lags Binance), just on longer timeframes. Potentially more liquidity in these markets, allowing larger position sizes.
**Risk**: Longer windows mean more time for the book to reprice. The latency edge may be weaker.
**Data needed**: PolyBackTest has 15-min market data (3,032 BTC markets available). We already fetched some. Our collector could also record 15-min markets.
**Implementation**: Minimal. Just add slug patterns for 15m markets (`btc-updown-15m-{ts}`) and calibrate thresholds.
**Key question**: Does the book lag persist in 15-min markets, or do market makers reprice faster when the window is longer?

## Questions for Codex
1. Which of these 7 strategies has the best risk-adjusted edge?
2. Which can be backtested with the data we already have?
3. Which should we trial first as a parallel paper trade?
4. Are there strategies we haven't considered?
5. For strategy #3 (resolution sniping), what win rate do we need at $0.90 entry to be profitable after fees?
6. For strategy #4 (cross-market arb), how often does price_up + price_down < 0.98 occur in our historical data?
7. For strategy #7 (15-min markets), should we calibrate thresholds differently? What does the PolyBackTest data show for 15-min BTC markets?
8. All strategies should be evaluated for BTC, ETH, AND SOL independently. ETH/SOL may have different optimal strategies due to higher volatility and thinner books.
