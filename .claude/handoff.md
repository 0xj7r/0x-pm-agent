# Handoff: Polymarket Multi-Coin Latency Arb

## What Was Done (2026-04-02)

Major codebase restructure from a broken Bayesian signal engine to a proven simple threshold strategy. 11 of 18 planned tasks completed.

### Completed
1. **shared/ package**: `fees.py` (canonical Polymarket fee calc), `constants.py` (API keys, coin configs), `db.py` (SQLite helpers)
2. **Dead code removed**: Bayesian engine (`btc_sniper.py`), old autoresearch loop, researcher stubs, dead tests (~1,700 lines deleted)
3. **ThresholdStrategy**: `strategies/threshold.py` replaces BayesianSignalEngine. Simple: if coin moves > threshold% and token price <= max_entry, buy.
4. **Strategy config updated**: `strategy_config.json` v3 with per-coin thresholds. `SignalConfig` replaced with `ThresholdConfig`.
5. **Engine rewired**: `core/engine.py` uses ThresholdStrategy. Removed all Bayesian signal logic (OFI, microprice, acceleration, log-odds). Net -58 lines.
6. **Backtesting consolidated**: `autoresearch.py` (523 lines) split into `precompute.py`, `simulator.py`, `research.py`. Four fetcher files merged into `fetcher.py`. Monte Carlo files merged into `projection.py`.
7. **Multi-coin support**: `binance_ws.py` parameterized for any symbol. `market_scanner.py` supports BTC/ETH/SOL slugs. Engine accepts `coin` param.
8. **Config cleanup**: Dead toggles removed from `config.py`.
9. **CLAUDE.md updated**: Still needs updating (current version references old Bayesian patterns).

### Remaining Tasks (from docs/superpowers/plans/2026-04-02-strategy-rebuild.md)

**Task 12 (partially done)**: Multi-coin paper trading. The agent committed multi-coin changes but they need verification. Check that `core/engine.py` properly passes `coin` to BinanceWSClient and MarketWindowScanner.

**Task 13**: Order placement pipeline test. `tests/test_order_pipeline.py` exists but imports may need updating for new scanner API.

**Task 14**: Snapshot collector. `collector/snapshot_recorder.py` was created by the agent but needs verification.

**Task 15**: Supabase migration. Create `shared/supabase_client.py`, SQL schema, sync script. Supabase URL/key from env vars.

**Task 16**: Hetzner deployment. Update Dockerfile for new structure. Deploy multi-coin bot + collector.

**Task 17**: Autoresearch loop. Build `autoresearch/runner.py` with candidate review flow (saves proposals, doesn't auto-deploy). New `autoresearch/program.md` targeting threshold optimization.

**Task 18**: Final cleanup. Update CLAUDE.md, run full test suite, verify startup.

## Validated Strategies

From `backtesting/strategy_results.json` (validated on 18+ days of PolyBackTest data):

| Coin | Threshold | Max Entry | Win Rate | Trades/Day | Edge over Breakeven |
|------|-----------|-----------|----------|------------|---------------------|
| BTC  | 0.08%     | $0.55     | 88%      | 5.8        | +37pp               |
| ETH  | 0.15%     | $0.55     | 98%      | 3.7        | +47pp               |
| SOL  | 0.08%     | $0.55     | 100%     | 2.3        | +49pp               |

Walk-forward validation confirmed: both halves of the dataset show consistent win rates. Sensitivity analysis shows smooth gradient (no cliff edges).

## Architecture

```
shared/              Single source of truth for fees, DB, constants
strategies/          ThresholdStrategy (check_signal returns Up/Down/SKIP/None)
clients/             Binance WS (any symbol), Polymarket CLOB, market scanner (any coin)
core/                Engine (parameterized by coin), resolution, risk, health
backtesting/         precompute, simulator, research, fetcher, projection, visualize
collector/           Live snapshot recorder (builds historical data over time)
tests/               test_fees, test_threshold, test_strategy_config, test_btc_resolution, etc.
```

## Key Files
```
strategies/threshold.py          ThresholdStrategy class (the core strategy)
strategies/strategy_config.py    ThresholdConfig, RiskConfig, StrategyConfig
strategy_config.json             Per-coin thresholds and risk params (v3)
backtesting/strategy_results.json  Autoresearch output (validated strategies)
shared/fees.py                   taker_fee(price) = 0.072 * p * (1-p)
shared/constants.py              API keys, COIN_CONFIGS, SLUG_PATTERNS
backtesting/research.py          run_autoresearch() with train/test split
backtesting/projection.py        MonteCarloSimulator for P&L projections
backtesting/fetcher.py           CoinDataFetcher for PolyBackTest API
```

## Data

SQLite DBs per coin in backtesting/:
- `btc.db`: 9,085 markets, ~1,348 with snapshots (18.6 days)
- `eth.db`: 8,900 markets, ~721 with snapshots
- `sol.db`: 5,103 markets, 308 with snapshots
- `historical.db`: old combined DB (can be removed, data migrated to per-coin DBs)

PolyBackTest API has 31 days max history. Need our own collector for longer-term data.

## Infrastructure
- Hetzner: 188.34.177.202, SSH key ~/.ssh/polymarket_hetzner
- Health: :8080
- Repo: https://github.com/0xj7r/polymarket-btc-sniper
- Monitoring: hourly Claude trigger with Slack

## Fee Model
Polymarket dynamic taker fee: `0.072 * p * (1-p)`. At p=0.50 that's 1.8%. Breakeven win rate at $0.50 entry is ~51%.

## Monte Carlo Results (6 months, $100 start)

| Scenario | Month 1 | Month 6 | Max DD (P95) |
|----------|---------|---------|--------------|
| BTC 88% WR | $256K | $5.6M | 29% |
| ETH 98% WR | $680K | $5.0M | 21% |
| SOL 100% WR | $76K | $842K | 0% |

All projections include: dynamic fees, slippage model, liquidity cap (20% of market depth).

## How to Continue

1. Verify Tasks 12-14 work (multi-coin, order pipeline, collector)
2. Set up Supabase (Task 15)
3. Deploy to Hetzner (Task 16)
4. Build autoresearch loop (Task 17)
5. Paper trade BTC + SOL for 48h, then go live
