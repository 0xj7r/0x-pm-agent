# Champion / Challenger Pattern

How we decide when to change strategies and how much capital to allocate per config.

## The Problem

We run 6 sniper containers in parallel, each with a different strategy config:

| Container | Strategy | Role |
|---|---|---|
| `btc-sniper` | strict (skew, move=0.08, max_entry=0.55) | **Champion** |
| `eth-sniper` | strict (skew, move=0.08, max_entry=0.55) | **Champion** |
| `btc-relaxed` | threshold, max_entry=0.75 | Challenger |
| `eth-relaxed` | threshold, max_entry=0.75 | Challenger |
| `btc-low-thresh` | threshold, move=0.05 | Challenger |
| `eth-low-thresh` | threshold, move=0.05 | Challenger |

This is an A/B test: we want to compare configs against each other without risking the whole bankroll on an unproven variant.

## Rules

### 1. Champion keeps full size

- `strategy_config.json` (used by `btc-sniper`, `eth-sniper`) uses **full position sizing**: `max_position_pct = 0.10`, `max_position_usd = 50.0`.
- This is the "production" strategy. It's what we trust with real capital.

### 2. Challengers get 25% size

- `strategy_config_relaxed.json` and `strategy_config_low_thresh.json` use **reduced sizing**: `max_position_pct = 0.025`, `max_position_usd = 12.50`.
- Challengers gather data without putting the bankroll at risk.
- If a challenger is wrong, the damage is capped at 25% of what the champion would have done.

### 3. Promotion requires 150+ trades AND significance

A challenger can only replace the champion if ALL of these are true:
- **≥150 resolved trades** at the challenger sizing
- **Test Sharpe > 1.0** (risk-adjusted return is meaningful)
- **Test win rate above 95% CI lower bound of 70%** (statistically above break-even after fees)
- **Positive PnL on last 50 trades** (not just an old hot streak)

These thresholds match the autoresearch validation rigor (`MIN_TRAIN_TRADES=100`, `MIN_TEST_TRADES=30`, `MIN_SHARPE=0.5`) but are tighter because we're promoting to production.

### 4. No automatic promotion

Changing a champion config is a **manual decision** made by reviewing autoresearch candidates in `autoresearch/candidates/`. Autoresearch runs weekly (Sundays) and proposes but never deploys.

Rationale: automated promotion based on backtests was how we ended up deploying the "volatility" ETH config that bombed (1 test trade, 97.7% WR backtest → 25% WR live).

### 5. Demotion rules

A champion is demoted (back to challenger sizing) if ANY of these trigger:
- **Win rate drops below 60% over last 50 trades** (edge decay)
- **Drawdown circuit breaker trips more than once per week** (too volatile)
- **Consecutive loss streak > 10** (something structural broke)

When demoted, the best-performing challenger is tested for promotion criteria.

### 6. Kill switches

Regardless of champion status, **all containers** respect:
- **Kill balance**: halt if bankroll drops below $10
- **Daily loss limit**: halt for today if daily PnL drops below -25% of starting bankroll
- **Drawdown circuit breaker**: halt if balance drops 40% from today's peak (auto-resets next day or after 6h)
- **Rate limiter**: halt if >20 trades entered per hour

## Why this works

The champion/challenger split protects against two failure modes:

1. **Overfit validation**: a strategy looks good on backtest but fails live. By giving it 25% size until proven, the maximum damage is capped.

2. **Edge decay**: a strategy that worked yesterday stops working today. The demotion rules catch this within ~50 trades, not 500.

## How to audit

```bash
# Current strategy per container
grep -r "strategy" strategy_config*.json

# Per-strategy P&L from Supabase
python3 -c "
from shared.supabase_client import SupabaseClient
supa = SupabaseClient()
stats = supa.load_trade_stats()
# ... group by strategy
"

# Autoresearch candidates waiting for human review
ls -lt autoresearch/candidates/ | head
```

## When to revisit

- **Weekly** (Sundays): review the newest autoresearch candidate, decide whether to promote
- **After any live P&L anomaly**: review demotion rules, run loss analysis
- **Before any real-money deployment**: all of the above, plus slippage model validation
