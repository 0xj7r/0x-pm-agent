# Handoff: Polymarket Multi-Coin Latency Arb

## Status (2026-04-03)
All 18 tasks from the restructure plan are complete. System is deployed and running.

## Live on Hetzner (188.34.177.202)

| Container | Strategy | Purpose |
|-----------|----------|---------|
| btc-sniper | skew(0.08, 0.02, 0.55) | Paper trading BTC (tight) |
| eth-sniper | skew(0.08, 0.02, 0.55) | Paper trading ETH (tight) |
| btc-relaxed | threshold(0.08, 0.75) | Paper trading BTC (relaxed) |
| eth-relaxed | threshold(0.08, 0.75) | Paper trading ETH (relaxed) |
| snapshot-collector | N/A | Recording BTC/ETH/SOL prices to Supabase |
| autoresearch | Grid search daily | Strategy discovery loop |

Dashboard: http://188.34.177.202:8080

## Data

**Supabase**: https://supabase.com/dashboard/project/vbvpaymtuozugylkuqmw
- snapshots: tick data (price, tokens, order book depth, spread, elapsed time)
- markets: metadata (slug, winner, volume, liquidity)
- trades: paper trade log
- strategy_results: validated strategies per coin

**Local**: SQLite on Hetzner at /opt/polymarket-agent/data/

## No Trades Yet

BTC has been in a tight range (~$66,700-$67,100) since deployment. The strategy needs sharp intra-5-min moves where the Polymarket book lags. This hasn't happened in the current low-volatility period. The bots are correctly scanning every window and rejecting signals that don't meet the threshold.

## Key Architecture Decisions
- Simple threshold detection (not Bayesian signal engine)
- Feature store: NumPy arrays, build once, backtest in seconds
- Separate DB per coin (no SQLite lock contention)
- Dual write: local SQLite + Supabase
- Candidates saved for human review (autoresearch does NOT auto-deploy)

## Next Steps
1. Wait for volatility and first paper trades
2. Codex review of data schema completeness
3. Build auto-paper-trade pipeline: autoresearch discovers → backtest validates → deploy as parallel paper trade
4. Explore on-chain Polymarket data for 3+ months of history
5. Build proper Next.js dashboard with Supabase real-time subscriptions
