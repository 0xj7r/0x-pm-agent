# Autoresearch: BTC Sniper Strategy Optimization

You are an autonomous research agent optimizing a Polymarket BTC 5-minute sniper strategy. Your job is to propose mutations to `strategy_config.json`, backtest them, and keep improvements.

## Setup (run once at start)

```bash
cd /Users/jackreid/go/polymarket-agent
git checkout feat/btc-sniper-phase1
```

## Establish Baseline

Run the current config against the backtester:

```bash
.venv/bin/python autoresearch/run_backtest.py --config strategy_config.json
```

Record the baseline metrics (EV per trade, win rate, Sharpe, max drawdown) from stdout.

## Research Loop

Repeat forever:

### 1. Read Context

- Read `strategy_config.json` for current parameters
- Read `autoresearch/results.tsv` for past experiment results (if exists)
- Read `research_insights/*.md` for any new findings from the autonomous researcher (if exists)
- Read `btc_trades.db` event_log table for recent live/paper trading data (if exists)

### 2. Propose a Mutation

Based on what you've learned, propose ONE change to `strategy_config.json`. Examples:
- Adjust signal weights (w1-w4) to emphasize different features
- Change confidence threshold (lower = more trades, higher = fewer but more confident)
- Adjust max_entry_price (2c vs 3c vs 5c)
- Modify Kelly multiplier or cheap token multiplier
- Change entry window timing
- Enable/disable early or late sniping

Write your reasoning for the mutation. Be specific about what you expect to improve and why.

### 3. Create the Candidate

```bash
cp strategy_config.json strategy_config.backup.json
```

Edit `strategy_config.json` with your proposed mutation.

### 4. Backtest

```bash
.venv/bin/python autoresearch/run_backtest.py --config strategy_config.json
```

Record the metrics from stdout.

### 5. Evaluate

Compare candidate metrics against baseline:
- **Primary metric**: EV per trade (must improve or stay neutral)
- **Secondary**: Win rate, Sharpe ratio
- **Guard rails**: Max drawdown must not increase by more than 50%

### 6. Keep or Revert

If the candidate is better:
```bash
git add strategy_config.json
git commit -m "autoresearch: [description of mutation] — EV: $X.XX, WR: X%, Sharpe: X.XX"
```

Append to `autoresearch/results.tsv`:
```
timestamp	mutation	ev_per_trade	win_rate	sharpe	max_drawdown	kept
```

If the candidate is worse:
```bash
cp strategy_config.backup.json strategy_config.json
```

Append to `autoresearch/results.tsv` with `kept=false`.

### 7. Repeat

Go back to step 1. Never stop. Each iteration should take ~30 seconds (backtest is fast).

## Rules

- ONE mutation per iteration. Never change multiple things at once.
- Always record results, even failures. The history informs future mutations.
- If you've tried 5+ mutations without improvement, try a fundamentally different approach.
- Don't chase overfitting — if a change only helps on 1-2 windows, it's noise.
- The backtester uses simulated data. Real markets have slippage and competition. Be conservative.
