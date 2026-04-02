# CLAUDE.md - Polymarket Multi-Coin Latency Arb

## Strategy

Simple threshold detection per coin. When price moves > X% on Binance and the Polymarket token is still <= $0.55, buy the directional side.

| Coin | Threshold | Max Entry | Win Rate |
|------|-----------|-----------|----------|
| BTC  | 0.08%     | $0.55     | 88%      |
| ETH  | 0.15%     | $0.55     | 98%      |
| SOL  | 0.08%     | $0.55     | 100%     |

## Rules

1. **Tests first, then code.** `python -m pytest tests/ -v` must pass.
2. **`from __future__ import annotations`** on line 1 or after docstring.
3. **Type hints** on all function signatures.
4. **Mock ALL external APIs** in tests.
5. **Do NOT `git push`** unless explicitly told to.
6. **Files under 400 lines.** Split by responsibility.
7. **OOP, modular, DRY.** No big if/else chains. Use dict dispatch.
8. **Use shared/ modules.** `shared.fees`, `shared.db`, `shared.constants`.

## Architecture

```
shared/          fees, db, constants (single source of truth)
strategies/      ThresholdStrategy (core strategy logic)
clients/         Binance WS, Polymarket CLOB, market scanner
core/            engine, resolution, risk, health, notifications
backtesting/     precompute, simulator, research, fetcher, projection
collector/       live snapshot recorder
```

## Key Patterns

- **ThresholdStrategy.check_signal()** returns "Up", "Down", "SKIP", or None
- **Fee model**: `shared.fees.taker_fee(price)` = 0.072 * p * (1-p)
- **Per-coin config**: `strategy_config.json` has `coins.{btc,eth,sol}` with thresholds
- **Health endpoint** on :8080
- **Slack notifications** on trades, resolutions, errors

## Testing

```bash
python -m pytest tests/ -v
python -m pytest tests/test_threshold.py -v
python tests/test_order_pipeline.py
```
