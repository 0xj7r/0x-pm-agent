# Market Intelligence: Polymarket 5-Min Crypto Markets

Synthesis of external research on Polymarket 5-minute crypto prediction markets. Used to inform our strategy design and edge hypotheses.

## Market Overview

**Product**: "BTC/ETH/SOL Up or Down" 5-minute binary prediction markets on Polymarket. Resolves to "Up" if underlying price at window end > price at window start, else "Down."

**Launch**: Product launched approximately **early March 2026** (polybacktest has ~9,200 BTC markets total, corresponding to ~30 days back from April 6).

**Volume**: [Yahoo Finance / April 2026] 5-minute Bitcoin contracts generate up to **$60 million in daily turnover**, vastly exceeding longer-duration markets which produce <$1M daily.

**Resolution**: Chainlink BTC/USD price feed. Market creates new window every 5 minutes.

## Key Facts from News Coverage

### MEXC News (1007359)
- Bot "woshfq19" made ~**$64,334 profit in 4 days** exclusively on 5-min contracts
- Single standout trade (April 3, 7:30-7:35 PM ET): turned $16 into >$16,000 (~9,900% gain)
- Method: accumulated 16,262 shares at near-zero prices (e.g. $0.001), contract repriced sharply as market normalized
- This is classic liquidity imbalance arbitrage, not directional betting
- Warning: "such opportunities are rare and quickly disappear as competition increases"

### Other Documented Bots
- One bot turned **$313 → $550,000+** exploiting Binance ↔ Polymarket latency
- Another completed ~**9,000 trades for $150,000** via small spreads
- The edge is in speed and microstructure, not fundamental analysis

### Yahoo Finance Analysis
- Polymarket introduced **execution delays** to level the field:
  - 500ms delay originally for market makers only
  - Later 250ms **universal delay** applied to all orders
- Analyst insight: these ultra-short instruments could provide "a more precise and cost-efficient way to hedge exposures held elsewhere"
- Key quote: "Fractions of a second could influence outcomes, suggesting that speed and infrastructure may increasingly shape who ultimately captures the gains"

## Strategy Examples Found

### 1. Our Current Approach (Latency Arbitrage)
- Subscribe to Binance WS for BTC/ETH/SOL
- When underlying moves >0.08% within a 5-min window, buy the matching Polymarket token if still cheap (<$0.55)
- Relies on Polymarket book lagging Binance
- Historical WR ~73-87%, decaying as competition rises

### 2. Composite Technical Signal (Archetapp Gist)
- Uses 7 weighted indicators: **window delta** (dominant), micro-momentum, acceleration, EMA 9/21 cross, RSI 14, volume surge, real-time tick trend
- **Key insight: "window delta is king"** - longer-term indicators are unreliable in 5-min windows
- Enters at **T-10 seconds before window close** (when direction is locked but tokens haven't fully repriced)
- Three modes: Safe (25% bankroll, 30% confidence), Aggressive (risk profits only), Degen (all-in)

### 3. Liquidity Imbalance (MEXC Bot)
- Buy extreme-cheap tokens ($0.001) in bulk
- Wait for repricing as the market becomes more efficient
- Very low hit rate but massive upside on the winners
- **Our threshold strategy blocks this** because we have `max_entry = $0.55` floor

## Data Sources

### PMXT Archive (https://archive.pmxt.dev) ❌ NOT USEFUL FOR OUR STRATEGY
**Status**: Free, updated hourly, parquet format
- Covers Polymarket, Kalshi, Opinion markets
- Schema: `{timestamp_received, timestamp_created_at, market_id (conditionId), update_type, data (JSON)}`
- Update types: `price_change` (99%) and `book_snapshot` (1%, has full bids/asks arrays)
- ~11,960 unique markets per hour of data
- Goes back to ~**Feb 23, 2026**
- Hourly files average ~232 MB each
- GitHub: https://github.com/pmxt-dev/pmxt (1,407 stars)

**VERIFIED: PMXT does NOT capture our 5-minute BTC/ETH/SOL crypto markets.**
- Downloaded `polymarket_orderbook_2026-04-07T09.parquet`
- Checked all 13 consecutive BTC 5-min markets active during that UTC hour
- **0 of 13 matched** any condition_id in the PMXT data
- PMXT apparently filters to longer-duration "main" markets (politics, sports, crypto-price-at-end-of-month)
- 5-min crypto markets generate a firehose of events (2016 new markets/week × 3 coins) that PMXT chose not to capture

**Conclusion**: PMXT is not a data source for us. Our existing polybacktest/live collector remains the only viable pipeline for 5-min crypto markets.

### polybacktest API (https://api.polybacktest.com) - what we use
- Has ~9,200 BTC markets, ~9,000 ETH, ~5,100 SOL
- Covers March 5, 2026 → present
- `price_start`/`price_end` (underlying) + snapshot timeseries of token prices
- Requires API key
- **Limitation**: no order book depth, only best bid/ask

### Kaggle Dataset (debayan31415/polymarket-5-minutes-btc-up-down-data)
- 113,245 rows, 1,191 unique BTC markets
- Snapshots every ~2 seconds
- Includes `ask_YES`, `bid_YES`, `ask_NO`, `bid_NO`, `btc_strike`, `btc_current`, `btc_gap`, `winner`
- Last updated **March 14, 2026**
- CC BY-NC-SA 4.0 license
- **Much smaller than our existing data** (1,191 vs 9,085 markets)
- Worth downloading as a sanity-check dataset, not a primary source

## Key Insights for Our Strategy

1. **The edge is real but decaying.** Multiple sources confirm latency arbitrage works on this market. Our 3-day data (87% → 64% WR decay) matches the pattern of edge compression as competition rises.

2. **Speed matters more than we're investing in.** 250ms execution delay means anyone trading with >250ms latency is effectively blind. Our bot is on the right side of this if Binance WS → decision → Polymarket order is sub-250ms. Worth measuring.

3. **"Window delta is king"** (from the Archetapp gist). Matches our observation: the primary signal is "how much did BTC move from window open". Secondary signals (skew, vol) add marginal value.

4. **Late entry (T-10s) is a valid variant.** Our bot enters as soon as the threshold hits. A variant that waits until T-10s might avoid some losing trades where the move reverses mid-window.

5. **Order book depth is a gap.** We don't have it. PMXT archive provides it. Upgrading would let us:
   - Simulate realistic fills (our paper trades assume perfect fills at best ask)
   - Detect the liquidity imbalance opportunities that the MEXC bot exploits
   - Better validate backtests

6. **Volume concentration in 5m products.** $60M/day in 5m markets vs <$1M in longer markets means:
   - Liquidity is good enough for retail-size trades
   - But also that sophisticated players are there
   - Edge at scale is limited by slippage + competition

## Recommended Follow-Up Actions

### Immediate
- Download PMXT Polymarket historical archive for BTC/ETH/SOL
- Compare PMXT coverage vs our existing data
- Prototype backtest with PMXT order book data (realistic fills)

### Medium-term
- Measure our current Binance→Polymarket order latency (target: <200ms)
- Experiment with late-entry variant (T-10s) as a parallel config
- Track edge decay rate weekly to predict when strategy becomes unprofitable

### Long-term
- Integrate with PMXT unified SDK for cross-market arbitrage (Polymarket ↔ Kalshi)
- Consider the "liquidity imbalance" strategy as a separate track (needs order book depth data)

## References

- [MEXC: Bot Generates $64K in 4 Days](https://www.mexc.co/en-PH/news/1007359)
- [Yahoo Finance: Polymarket's 5-Minute BTC Bets](https://finance.yahoo.com/news/polymarkets-5-minute-bitcoin-bets-123329673.html)
- [GitHub Gist: 5-min BTC Trading Bot Guide (Archetapp)](https://gist.github.com/Archetapp/7680adabc48f812a561ca79d73cbac69)
- [Kaggle: Polymarket 5-min BTC Data](https://www.kaggle.com/datasets/debayan31415/polymarket-5-minutes-btc-up-down-data)
- [PMXT GitHub (1.4k stars)](https://github.com/pmxt-dev/pmxt)
- [PMXT Archive](https://archive.pmxt.dev)
- [PMXT Data Ingestion Tool](https://github.com/clarkpalmer/pmxt-data-ingestion)
- [Reddit r/algotrading: "Stop paying for Polymarket data"](https://www.reddit.com/r/algotrading/comments/1rdhw2n/stop_paying_for_polymarket_data_pmxt_just/)
