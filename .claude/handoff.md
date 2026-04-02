# Handoff: Polymarket Multi-Coin Latency Arb

## What This Bot Does

Trades BTC/ETH/SOL 5-minute Up/Down binary options on Polymarket using latency arbitrage. When a coin moves on Binance and the Polymarket order book hasn't repriced yet, buy the directional token cheaply. Collect $1/share on resolution.

## Validated Strategies (from backtesting/strategy_results.json)

| Coin | Threshold | Max Entry | Win Rate | Trades/Day | Data |
|------|-----------|-----------|----------|------------|------|
| BTC | 0.08% | $0.55 | 88% | 5.8 | 3,912 markets w/snapshots |
| ETH | 0.15% | $0.55 | 98% | 3.7 | 2,627 markets w/snapshots |
| SOL | 0.08% | $0.55 | 100% | 2.3 | 308 markets w/snapshots |

Walk-forward validated: win rates stable across first/second halves. Sensitivity validated: results degrade gracefully (no cliff edges).

Fee model: Polymarket dynamic taker fee = 0.072 * p * (1-p). At entry $0.50 that's 1.8%.
Breakeven win rate: ~51%. All three coins have 37-49pp edge over breakeven.

## What Was Done (This Session)

### Strategy Discovery
1. Fetched 9K+ BTC, 8.9K ETH, 5.1K SOL market headers from PolyBackTest API (31 days max)
2. Fetched snapshots for qualifying markets (>0.05% price move): 3,912 BTC, 2,627 ETH, 308 SOL
3. Ran autoresearch grid search (2,656 strategy configs per coin, train/test split)
4. Found simple threshold beats all complex filters (volatility, acceleration, consistency)
5. Fixed look-ahead bias in elapsed_pct, fixed fee model (was flat 2%, now dynamic)
6. Walk-forward + sensitivity validation confirms strategies are not overfit

### Codebase Restructure (Phases 1-4 complete, Phase 5 partial)
Commits on branch `feat/btc-sniper-phase1`:
- `shared/fees.py` - Canonical taker fee function (was duplicated in 4 files)
- `shared/constants.py` - API keys, coin configs, slug patterns
- `shared/db.py` - DB connection factory, schema helpers
- `strategies/threshold.py` - ThresholdStrategy replacing old BayesianSignalEngine
- `strategies/strategy_config.py` - Per-coin ThresholdConfig (removed Bayesian weights)
- `strategy_config.json` - Version 3 with per-coin thresholds
- `core/engine.py` - Rewired to use ThresholdStrategy (net -58 lines)
- `core/btc_resolution.py` - Uses shared.fees
- `backtesting/precompute.py` - Feature extraction (from 523-line autoresearch.py)
- `backtesting/simulator.py` - Strategy simulation + grid builder
- `backtesting/research.py` - Autoresearch orchestrator
- `backtesting/fetcher.py` - Merged 4 fetcher files into CoinDataFetcher class
- `backtesting/projection.py` - Merged Monte Carlo modules
- `clients/binance_ws.py` - Parameterized for multi-coin
- `clients/market_scanner.py` - Supports BTC/ETH/SOL slug patterns
- `collector/snapshot_recorder.py` - Live data collector (new)
- Deleted: btc_sniper.py, autoresearch/ dir, researcher/ dir, 6 dead files, 3 dead test files
- Cleaned: config.py dead toggles removed

## What Still Needs Doing

### Remaining from plan (docs/superpowers/plans/2026-04-02-strategy-rebuild.md)

**Task 15: Supabase migration**
- Create Supabase tables (markets, snapshots, strategy_results)
- Write shared/supabase_client.py
- Sync script: local SQLite → Supabase
- Env vars: SUPABASE_URL, SUPABASE_KEY

**Task 16: Hetzner deployment**
- Update Dockerfile for new structure (shared/, strategies/, collector/)
- docker-compose with: paper trading engine, snapshot collector, autoresearch
- Deploy to 188.34.177.202 (SSH key: ~/.ssh/polymarket_hetzner)
- Repo: https://github.com/0xj7r/polymarket-btc-sniper

**Task 17: Autoresearch loop**
- New program.md targeting threshold strategy optimization (not old Bayesian weights)
- runner.py: loads current best, proposes mutations, backtests, saves candidates
- Candidates saved for human review (not auto-deployed)
- Runs via Claude trigger on Hetzner
- Should explore: adaptive thresholds per volatility regime, new feature filters, cross-coin signals

**Task 18: Final cleanup**
- Run full test suite, fix remaining broken tests (test_btc_engine.py, test_btc_integration.py, test_btc_risk.py still reference old SignalConfig/ExecutionConfig)
- Verify paper trading starts: `python main.py --config strategy_config.json --coin btc`

### Data Collection (Not Urgent)
- PolyBackTest only has 31 days of history
- Build own collector (collector/snapshot_recorder.py exists, needs Hetzner deployment)
- Explore Polymarket Gamma API for older data
- After 3 months of collection, rerun autoresearch on richer dataset

### ETH Autoresearch Grid Issue
The autoresearch grid tested thresholds [0.01, 0.02, 0.03, 0.05, 0.08]. ETH's optimal is 0.15% (higher volatility). The grid should be adaptive per coin's volatility. Fix: in backtesting/simulator.py's build_strategy_grid(), scale move_thresholds based on typical coin volatility.

## Key Files

```
shared/
  fees.py                  # taker_fee(), taker_fee_usd()
  constants.py             # API keys, COIN_CONFIGS, SLUG_PATTERNS
  db.py                    # get_connection(), init_coin_db(), load helpers

strategies/
  threshold.py             # ThresholdStrategy (the actual trading logic)
  strategy_config.py       # ThresholdConfig, RiskConfig, StrategyConfig

core/
  engine.py                # BTCTradingEngine (main loop, uses ThresholdStrategy)
  btc_resolution.py        # P&L calculation on resolution

clients/
  binance_ws.py            # Binance WS (parameterized symbol)
  market_scanner.py        # Gamma API market discovery (multi-coin)
  polymarket.py            # CLOB client (order placement)
  polymarket_ws.py         # CLOB WS (live token prices)

backtesting/
  precompute.py            # Feature extraction from snapshots
  simulator.py             # Strategy simulation + grid search
  research.py              # Autoresearch orchestrator
  fetcher.py               # CoinDataFetcher (PolyBackTest API)
  projection.py            # MonteCarloSimulator
  visualize.py             # HTML fan chart
  strategy_results.json    # Validated per-coin strategy params
  btc.db / eth.db / sol.db # Historical data (SQLite)

collector/
  snapshot_recorder.py     # Live data collector

strategy_config.json       # Runtime config (per-coin thresholds + risk)
config.py                  # Env vars (Polymarket keys, etc.)
```

## Infrastructure
- Hetzner: 188.34.177.202, SSH key ~/.ssh/polymarket_hetzner
- Bot: Docker, Binance WS + Polymarket CLOB WS, health :8080
- Repo: https://github.com/0xj7r/polymarket-btc-sniper
- Monitoring: hourly Claude trigger with Slack
- PolyBackTest API keys: see shared/constants.py

## Monte Carlo Projections (from backtesting, $100 start, 6mo)

| Scenario | BTC Median | ETH Median | SOL Median |
|----------|-----------|-----------|-----------|
| Observed WR | $5.6M | $5.0M | $842K |
| Degraded -10pp | $3.9M | $3.7M | N/A |
| Degraded -20pp | $2.6M | $2.4M | N/A |

Note: these are liquidity-capped. Once bankroll > ~$30K, growth is linear at ~$3K/trade * trades/day. The astronomical numbers come from the compounding phase in the first week.
