# Day Analysis — 2026-04-27

Comparison of our bot vs whale `unlawful_shear` over the last 24h on
btc-updown-5m markets. Data pulled from `data-api.polymarket.com/activity`.

## Executive summary

**We are 2× as asymmetric as unlawful** (0.33 vs 0.58 fill-symmetry ratio)
and trade 1/45th the volume per market. The combination produces a
**different failure mode than unlawful's**: ours is adverse selection on
asymmetric fills; his is occasional direction-wrong calls offset by huge
balanced-fill wins.

| Metric | Us | Unlawful | Multiplier |
|---|---|---|---|
| Markets traded (24h) | 204 | 9 | — |
| Total cost deployed | $2,125 | $6,821 | 3.2× |
| **Per-market cost** | **$10.40** | **$758** | **73× more concentrated** |
| Total NET P&L | **-$33.87** | **+$1,021.80** | — |
| Per-dollar return | **-1.6%** | **+15%** | — |
| Avg trades per market | 4.6 | **210** | **45× more activity** |
| **Avg fill symmetry** | **0.33** | **0.58** | **1.8× more balanced** |
| Win-rate (markets) | 54% | 44% | — |
| Worst single-market loss | -$29 | -$306 | He absorbs much bigger losses |
| Best single-market gain | +$29 | +$1,230 | His winners are 40× bigger |

**Unlawful is making 30× more dollars per dollar-deployed** despite
losing on a higher % of markets. He has POSITIVE skew (big wins offset
losses). We have NEGATIVE skew (small wins, comparable losses).

## Pattern 1: Asymmetric fills (the systematic bleed)

**Our 5 worst markets today**:

| Market | Net | Cost | Up qty | Down qty | Symmetry | Trades |
|---|---|---|---|---|---|---|
| 1777284600 | -$29.10 | $34.10 | 46 | 5 | **0.11** | 10 |
| 1777290300 | -$25.95 | $35.95 | 77 | 10 | **0.13** | 10 |
| 1777310100 | -$22.15 | $27.15 | 46 | 5 | **0.11** | 10 |
| 1777301700 | -$21.31 | $37.00 | 61 | 16 | **0.26** | 16 |
| 1777282500 | -$20.55 | $25.55 | 52 | 5 | **0.10** | 13 |

**ALL 5 have fill-symmetry under 0.30** — meaning Up filled 4-9× more than Down (or vice versa). Pattern:

1. Strategy posts paired bids on both legs
2. **Trending market** — favored leg has 5-10× more sellers wanting to dump than the unfavored leg has buyers willing to sell
3. Favored leg fills 4-9× more than unfavored
4. After merge consumes MIN(left, right), residual stranded on one side
5. Market resolves against the residual → bleed

Unlawful's worst losses don't have this pattern — his asymmetric losses are 0.40-0.90 symmetry. Even when he loses, his fills are ~2× more balanced than ours.

## Pattern 2: Capital concentration vs fragmentation

**Unlawful concentrates capital**: 9 markets × $758 avg = he picks markets and goes deep. 45× more trades per market = more chances to capture flow on both sides over the 5-min bar lifetime.

**We fragment capital**: 204 markets × $10.40 avg = we touch many markets but go shallow. 4.6 trades per market = barely enough to get one paired bid + one rescue attempt. We don't have time to RE-ENTER and re-balance after the first asymmetric fill.

**Math of why this matters**:
- With $10 per market and 4 trades, every fill is large relative to position. One asymmetric fill = near-permanent imbalance.
- With $758 per market and 210 trades, no single fill matters much. Asymmetric streaks get balanced over time as flow rotates.

## Pattern 3: Skew of returns

**Our distribution** (over 204 markets):
- Best market: +$29
- Worst market: -$29
- Net: -$33

We have SYMMETRIC outcomes — wins and losses cap at ~$30 each. Slightly negative net.

**Unlawful's distribution** (over 9 markets):
- Best market: +$1,230
- Worst market: -$306
- Net: +$1,021

Unlawful has POSITIVE SKEW — best win is 4× the worst loss. The single +$1,230 market more than covers all 5 of his losing markets combined.

**The difference**: when a market goes hard in one direction and resolves favorably for the side he ends up holding (after asymmetric fills), unlawful HOLDS HIS POSITION instead of trying to rescue at break-even. We rescue compulsively, capping our upside at "balanced position + merge release". His +$1,230 came from a market where he had 618 Up + 0 Down (sym=0.00, fully one-sided) and Up resolved as winner — gross profit on the unhedged exposure.

## Root causes

### RC1: Tight per-market clip ($1.10 base) doesn't allow re-balancing
- Unlawful does 210 trades per market because he has hundreds of dollars committed per market. Even modest-clip refresh = lots of attempts.
- We do 4.6 trades per market because $10 / $1.10 base = ~5 attempts max before risk caps stop us. After 1 asymmetric fill we've spent the budget on the wrong side.
- **Fix**: per-market budget allocated PROPORTIONAL to capital × market activity, not flat $1.10.

### RC2: Rescue compulsion caps upside
- When asymmetric fill happens, we rescue (buy opposite leg) + merge → break-even outcome.
- Unlawful HOLDS asymmetric positions when the market is moving HIS way. The +$1,230 market = no Down, all Up, hold to resolution.
- **Fix**: regime-aware rescue. If trend is in favor of our stranded leg, HOLD. If trend is against, rescue. Currently we always rescue.

### RC3: We don't read directional flow before deciding to rescue
- Whale sees 5 minutes of flow across 200 trades and infers "Up sellers dominant = market thinks Down will win". He positions accordingly.
- We see 4 trades and rescue.
- **Fix**: directional momentum signal at fill time (read book skew + recent trade tape) → if signal favors our stranded side, hold; else rescue.

### RC4: We trade too many markets thinly
- 204 markets in 24h = 8.5 markets/hour. Each only sees 4-5 trades from us.
- Unlawful: 9 markets in 24h = 0.4 markets/hour. Each gets 200+ trades.
- **Fix**: stay on fewer markets longer. Don't restart every 5min via supervisor (that's #45 dynamic discovery).

## Recommended fixes (prioritized)

### Tier 1 (high impact, lower complexity)
1. **Reduce market churn (#45)** — kill the supervisor restart cycle. Each restart = lost in-flight state + re-engaged drift block. Stay on the same market_id for full 5-min lifetime continuously instead of cycling through prev/current/next.
2. **Bigger clips ($5 base instead of $1.10)** — gives us 5× more attempts per market before risk caps. More attempts = more chance to balance fills.
3. **Asymmetric hold signal** — if the FAVORED side is the side we already hold, don't rescue. Only rescue when stranded side is OUT-OF-FAVOR (likely to lose).

### Tier 2 (medium complexity, structural)
4. **Per-market trend gate (#53)** — pause entry when the market's mid moves >5¢ in 30s. Catches the bleed-pattern entry window.
5. **Depth ladder (#51)** — 3-5 levels per leg. Mirrors unlawful's structural advantage (multi-fill per sweep).
6. **Capital concentration mode** — operator config to bias 80% of capital toward 1-2 active markets vs spreading across 10. Pick a mode at startup based on market activity prediction.

### Tier 3 (defer until data justifies)
7. **Multi-market parallel** (re-enable include-prev/next 1) — only after #45 + #53 ensure each market is properly handled.
8. **Adaptive safety_ticks** — only after directional flow signal is reliable.

## Conclusion

Tonight's bleed wasn't from any single bug — it was from **the strategy's structural mismatch with the market**: thin clips × many markets × always-rescue × no-trend-read = adverse-selected fills + capped upside = consistently slightly negative.

The fix is not more defensive gates. **The fix is becoming MORE like unlawful**: concentrate capital, stay on fewer markets longer, get more trades per market, hold positions when trend is in our favor instead of always rescuing.

The conservative v34 currently deployed is necessary safety while we redesign — but it locks in the structural problem (thin trades, many markets, always rescue). The roadmap above is what unlocks the upside.
