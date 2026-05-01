```markdown
# Comprehensive Strategy Audit & Implementation Document  
**Polymarket BTC 5-Min Up/Down Markets**  
**Prepared for Agent Audit & Implementation**  
**Date: May 01, 2026**

This document consolidates **the entire conversation history** — original paired-MM plan, live-testing pain points, all mathematical details, signals, risk engine, configuration, data requirements, infrastructure, and the full migration plan. It covers **both strategies** in depth so the receiving agent can perform a complete audit, analysis, and modular implementation.

---

### 1. Project Background & Live Testing Pain Points

You are building a **production-grade Rust trading engine** (`polymarket-exec`) deployed on EU AWS (low-latency to Polymarket’s eu-west-2 CLOB). The engine is already highly modular with folders such as `strategies/`, `signals/`, `market_making/`, `core/`, `runtime/`, `infra/`, etc.

**Market characteristics** (5-min BTC Up/Down binary markets):
- New independent market every 5 minutes.
- Yes/No shares (priced 0–1 USDC) resolve to $1 or $0 based on whether BTC ends the window ≥ or < the window open price (Chainlink oracle).
- Extremely high volume, continuous overlapping windows, and emotional volatility → frequent YES + NO sum < 1.00 mispricings.
- Polymarket incentives: Liquidity Rewards (favor tight two-sided depth) + maker rebates.

**Original live-testing issues** (critical context):
- Lots of **accumulated one-sided positions** that cannot get filled → stranded → full resolution loss.
- **Loss-making in whipsaw / volatile regimes** → repeated adverse fills without round-trip profits.

These are classic short-horizon binary MM problems. The original paired-MM approach tried to solve them with aggressive inventory skew and rescue logic, but the Pair-Cost style solves them more elegantly while adding convex upside.

---

### 2. Strategy 1: Paired Market Making (Original – Still Fully Supported)

**Identifier**: `paired_mm` (or `btc_5m_mm`)

**Core philosophy**: Act as a true liquidity provider by posting **two-sided ladders** on both Yes and No, capturing spread + liquidity rewards + rebates while staying roughly delta-neutral.

**Key mechanics**:
- Split USDC → equal Yes + No shares.
- Dynamic multi-level ladder with base spread 1–8¢, adjusted by fair-value and inventory.
- **Inventory skew** (Avellaneda-Stoikov reservation price adjustment):
  \[
  \text{quote_shift} \approx \gamma \times Q \times \sigma^2 \times \tau
  \]
- Fractional Kelly clip sizing based on edge (spread + rewards).
- **Merge**: Whenever balanced Yes + No → immediate USDC recycle.
- **Rescue**: When heavily one-sided → EV-based decision (SELL heavy side or buy light side + merge).
- **EV hold vs rescue**:
  \[
  \Delta_\text{EV} = Q \times (P_\text{model} - \text{best_bid})
  \]
- Vol-regime defense: widen spreads / reduce size / pause in high ATR/whipsaw.
- Fair-value model (same as below).

**Strengths**: Maximizes liquidity rewards (two-sided balance scores very high).  
**Weaknesses**: Higher operational complexity; still vulnerable to stranded positions and whipsaw (your observed issues).

Your existing `WHALE_PAIR_BTC_5M_MM_*` knobs (clip sizes, skew, ladder levels, etc.) remain fully functional for this strategy.

---

### 3. Strategy 2: Pair-Cost Hedged Arbitrage (Gabagool-style / Recommended Primary)

**Identifier**: `pair_cost_arb`

**Core philosophy**: Pure opportunistic **cheap-leg arbitrage**. Never sell. Only buy the temporarily cheap leg, maintain pair-cost < threshold, merge balanced pairs instantly, and allow natural asymmetric excess for convex payoffs.

This is the style used by the top wallets (gabagool22 archetype and its open-source clones).

**Core loop** (executed on every WebSocket/book/BTC-tick event):
1. Detect when one leg is temporarily cheap.
2. Buy **only the cheap leg** (limit order preferred).
3. Update running averages → compute `pair_cost`.
4. Immediately merge any balanced pairs.
5. Allow **natural asymmetric excess** on one side (source of convex upside).
6. In late window + strong conviction → deliberately amplify excess.

**Core mathematics**:
- `avg_yes = cost_yes / qty_yes`, `avg_no = cost_no / qty_no`
- `pair_cost = avg_yes + avg_no`
- **Projected pair cost** after buying Δq at price p_L:
  \[
  \text{projected_pair_cost} = 
  \begin{cases}
  \frac{c_y + \Delta q \cdot p_L}{q_y + \Delta q} + \text{avg}_n & \text{(YES)} \\
  \text{avg}_y + \frac{c_n + \Delta q \cdot p_L}{q_n + \Delta q} & \text{(NO)}
  \end{cases}
  \]
- **Buy rule**: projected_pair_cost < `threshold` (default 0.99).
- **Merge**: Any `min(qty_yes, qty_no)` → immediate merge.
- **Convex payoff**: Balanced portion = guaranteed small profit; excess on winner = `excess_qty × (1 - avg_excess)`.

**Fair-value / cheap-leg signal**:
\[
P(\text{Up}) = \Phi\left( \frac{\Delta_\text{BTC} + \mu \cdot \tau}{\sigma \sqrt{\tau}} \right)
\]
Cheap leg = mid_price meaningfully below fair_price **AND** projected_pair_cost < threshold.

**Convexity amplification** (late-window rule):
- If time left < `late_window_sec` **and** P > `convex_p_threshold` (0.70) → stop buying opposite leg.

**Risk engine** (reuses your existing):
- Per-market exposure caps, global drawdown stops, vol-regime switch (tighten threshold or pause in high-vol).
- EV hold-vs-rescue for any rare unmergeable excess near expiry.

---

### 4. Side-by-Side Comparison

| Aspect                        | Paired-MM (Original)                     | Pair-Cost Hedged Arb (Recommended)      |
|-------------------------------|------------------------------------------|-----------------------------------------|
| Primary edge                  | Spread + liquidity rewards               | Mechanical mispricing + convex excess   |
| Directional risk              | Medium (needs skew/rescue)               | Very low (pair-cost guardrail)          |
| Stranded positions            | Frequent                                 | Almost eliminated                       |
| Convex / asymmetric upside    | Possible via late-window lean            | Built-in naturally                      |
| Capital turnover              | Good                                     | Extremely high                          |
| Operational complexity        | Higher                                   | Lower                                   |
| Liquidity rewards             | Excellent                                | Good (via optional hybrid overlay)      |

**Hybrid mode**: Run 80–90% Pair-Cost + 10–20% paired-MM ladders (controlled by `hybrid_mm.capital_pct`).

---

### 5. Data Streams & Infrastructure Required

**Already implemented**:
- Polymarket CLOB API + Market WS + User WS.
- Binance + Coinbase BTC spot WS + REST bootstrap.
- SQLite order store, journal, audit logs.

**Critical addition**:
- Polygon RPC (Dwellir full node — free tier for testing, **$49/mo paid tier strongly recommended for live**).

**On-chain actions** (via RPC):
- `split()` and `merge()` (frequent in Pair-Cost style).

---

### 6. Configuration Refactor

**Prefix change**: All old `WHALE_PAIR_*` → `PM_BTC_5M_*`.

**Strategy selector** (in `.env`):
```env
PM_BTC_5M_STRATEGY=pair_cost_arb   # or paired_mm for original
```

**Strategy-specific knobs** → moved to **YAML** files (one per strategy) for type safety and clarity.

**Example YAML for Paired-MM** (`btc_5m_paired_mm.live.yaml`):
```yaml
strategy: paired_mm
# (existing MM knobs can be migrated here or kept in .env)
```

**Full YAML for Pair-Cost Hedged Arbitrage** (`config/strategies/btc_5m_pair_cost_arb.live.yaml`):
```yaml
strategy: pair_cost_arb

pair_cost:
  threshold: 0.99
  high_vol_threshold: 0.97
  min_merge_usd: 50.0
  min_edge_bps: 100

clip_sizing:
  base_clip_usd: 1.5
  min_clip_usd: 0.5
  max_clip_usd: 4.0
  fractional_kelly: 0.15

convexity:
  late_window_sec: 120
  convex_p_threshold: 0.70
  max_excess_usd: 8.0

signals:
  fair_value:
    momentum_weight: 0.65
    vol_lookback_sec: 60
    use_piecewise_approx: true
  cheap_leg:
    price_deviation_bps: 35
    order_flow_imbalance_threshold: 0.0
  vol_regime:
    high_vol_atr_threshold: 0.00085
    extreme_vol_pause: true

hybrid_mm:
  enabled: true
  capital_pct: 0.15
  base_spread_bps: 80

rescue:
  hedge_rescue_edge_bps: 25
  max_gross_cost_usd: 12
  max_gross_cost_bps: 300

operational:
  cooldown_ms: 100
  asymmetric_fill_max_penalty: 0.02
  max_exposure_usd: 12
  max_session_loss_bps: 2500

version: "1.0"
last_updated: "2026-05-01"
```

---

### 7. Implementation Roadmap

1. Add `PairCostState` struct in `core/`.
2. Implement `PairCostArbStrategy: StrategyTrait` in `strategies/`.
3. Load YAML config via `serde`.
4. Wire signals + cheap-leg detection.
5. Add merge logic + convexity late-window rule.
6. Optional hybrid MM overlay.
7. Paper mode → tiny-live → scale.

**Future differentiators** (to outperform others):
- Cross-timeframe (5m ↔ 15m) arb.
- Markov persistence in signals.
- Multi-asset support.
- CLOB v2 + pUSD optimizations.

---

This document is the **single source of truth**. The agent can now audit both strategies, implement the modular solution, and ensure the migration solves the original pain points while preserving full flexibility.

**End of Document**
```

This is the complete, clean Markdown document you can copy and send directly to the other agent. Let me know if you need any final tweaks before handing it over!