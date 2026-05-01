```markdown
# Comprehensive Strategy Audit & Implementation Document  
**Polymarket BTC 5-Min Up/Down Markets**  
**Prepared for Agent Audit & Implementation**  
**Date: May 01, 2026**

## Implementation Status — 2026-05-01

This spec is now implemented in `polymarket-exec` through the new modular strategy seam:

- Live runtime selector supports `pair_cost_arb`, `paired_mm`, and `hybrid`.
- Live runtime can run composite strategy lists such as `pair_cost_arb,paired_mm` through the existing `Runtime<StrategyMode>` hot path.
- Strategy-specific knobs are loaded from YAML via `PM_BTC_5M_STRATEGY_PROFILE_PATHS` / `PM_BTC_5M_STRATEGY_PROFILE_PATH`.
- Default live profile exists at `polymarket-exec/config/strategies/btc_5m_pair_cost_arb.live.yaml`.
- Paired-MM has its own live profile at `polymarket-exec/config/strategies/btc_5m_paired_mm.live.yaml`.
- Hybrid runs should use `PM_BTC_5M_STRATEGY_PROFILE_PATHS=config/strategies/btc_5m_pair_cost_arb.live.yaml,config/strategies/btc_5m_paired_mm.live.yaml`; profiles are deep-merged in order.
- Runtime config uses `PM_BTC_5M_*` as the only Polymarket BTC 5m env prefix; legacy env aliases were removed.
- Legacy `unlawful_shear`, `goat_pair`, and `bonereaper` strategy implementations are removed from the active runtime selector.
- The legacy `unlawful_gate` signal/runtime path has been removed; active runtime strategy modes are `pair_cost_arb`, `paired_mm`, and their hybrid composition.
- Generic `strategies::StrategyRegistry` now supports both `paired_mm` and `pair_cost_arb`.
- Active source/config no longer exposes a live bonereaper strategy path.
- Shared YES/NO pairing primitives now live under `polymarket-exec/src/market_making/pairing/`.
- `pair_cost_arb` owns the full Gabagool loop: cheap-leg BUYs, buy-light-side recycling, late-window convexity, and merge-trigger decisions.
- `paired_mm` is now the two-sided ladder overlay only: Stoikov reservation pricing, dynamic depth/spacing, and quote emission.
- Runtime remains the hard execution seam for actual `MergeIntent` submission and on-chain merge settlement.

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

**Identifier**: `paired_mm`

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

Paired-MM-specific knobs now live in the paired-MM YAML profile, not in legacy env-only MM blocks.

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

**Canonical prefix**: `PM_BTC_5M_*`.

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


**Fair-Value Model Math – Detailed Explanation**

The **fair-value model** is the “brain” of the Pair-Cost Hedged Arbitrage strategy. It estimates the true probability that the 5-minute window will resolve **Up** (BTC ends ≥ window open price) given the current state of the market. This probability is then used to derive fair prices for YES and NO shares and to decide whether a leg is “cheap” enough to buy.

### The Core Formula

\[
P(\text{Up}) = \Phi\left( \frac{\Delta_{\text{BTC}} + \mu \cdot \tau}{\sigma \sqrt{\tau}} \right)
\]

where \(\Phi\) is the **cumulative distribution function (CDF)** of the standard normal distribution (also called the probit function).

### Variable Breakdown

| Variable          | Meaning                                                                 | Typical Calculation / Source                          |
|-------------------|-------------------------------------------------------------------------|-------------------------------------------------------|
| \(\Delta_{\text{BTC}}\) | Signed percentage displacement of current BTC price from the window’s **open price** | \(\frac{\text{current\_price} - \text{window\_open\_price}}{\text{window\_open\_price}}\) |
| \(\mu\)           | Short-term momentum drift (expected drift per unit time)               | EMA or linear regression of recent 10–30s BTC returns |
| \(\tau\)          | Fraction of the 5-minute window still remaining                        | \(\frac{\text{seconds remaining}}{300}\)              |
| \(\sigma\)        | Short-term realized volatility (annualized or scaled to window)        | Std dev of 30–90s BTC returns (scaled appropriately) |
| \(\Phi(\cdot)\)   | Standard normal CDF                                                     | Full `norm.cdf()` or fast piecewise approximation     |

### Intuitive Interpretation

This is essentially a **Brownian-motion / diffusion model** adapted to the short 5-minute horizon:

- The numerator \(\Delta_{\text{BTC}} + \mu \cdot \tau\) is the **expected total move** by the end of the window (current position + expected drift).
- The denominator \(\sigma \sqrt{\tau}\) is the **expected uncertainty** (standard deviation of possible future moves).
- Dividing them gives a **standardized z-score**.
- Passing that z-score through the normal CDF \(\Phi\) converts it into a clean probability between 0 and 1.

In plain English:  
“If BTC has already moved +0.3% into the window and we still have 2 minutes left, how likely is it that the final move will still be positive after accounting for volatility and recent momentum?”

### How It Is Used in the Strategy

1. **Fair prices**:
   - Fair YES price ≈ \(P(\text{Up})\)
   - Fair NO price ≈ \(1 - P(\text{Up})\)

2. **Cheap-leg detection** (the actual buy trigger):
   - YES is cheap if:
     \[
     \text{YES mid price} < P(\text{Up}) - \text{price_deviation_bps}
     \]
     **and** projected pair_cost < threshold
   - Same logic (mirrored) for NO.

3. **Late-window convexity rule**:
   - If \(P(\text{Up}) > 0.70\) (or \(P(\text{Down}) > 0.70\)) and time left < `late_window_sec`, deliberately allow excess on the high-conviction side instead of forcing a perfect hedge.

### Practical Implementation Notes

- **Speed**: Full normal CDF is fine, but most bots use a fast piecewise linear approximation (your config has `use_piecewise_approx: true`) because it runs in nanoseconds.
- **Lookback periods**: \(\sigma\) and \(\mu\) are usually computed over the last 30–90 seconds of BTC ticks (Binance/Coinbase WS feed).
- **Tuning**: The only free parameter you usually adjust is the weight on momentum (`momentum_weight`) or the cheap-leg deviation in bps (currently 35 bps in the YAML).
- **Edge cases**: Very early in the window (\(\tau \approx 1\)) the model is close to 0.5. Very late in the window (\(\tau \approx 0\)) it becomes extremely sharp — which is exactly when you want the strongest cheap-leg and convexity signals.

This model is deliberately simple yet powerful — it is the same one used in virtually all profitable 5-min/15-min BTC bots (including the original Gabagool clones). It turns raw price data into a calibrated probability that directly feeds the pair-cost guardrail and convexity rules.

If you want the exact Rust pseudocode for this model or a numerical example with real numbers, just say the word!


**Yes — you should use different (smaller) clip sizes for your tiny-live deployment.**

### Why Different Clip Sizes Make Sense

Your current tiny-live setup has `PM_BTC_5M_EXEC_STARTING_CASH_USD=50`.  
The YAML values we gave earlier (`base_clip_usd: 1.5`, `max_clip_usd: 4.0`) are reasonable **full-live** defaults, but they are **too large** for $50 capital:

- A $4 clip = 8% of your entire bankroll → too aggressive for testing.
- You want to be able to run multiple overlapping 5-min windows and survive a few bad merges or gas fees.
- Polymarket has venue minimum order quantities (~5 shares) and small gas costs on Polygon that become relatively expensive on tiny positions.
- The goal of tiny-live is to validate the full logic (pair-cost updates, merge frequency, cheap-leg detection, convexity rule, etc.) with real money, **not** to maximize PnL yet.

**Best practice** (used by almost all bots):
- Have **deployment-specific scaling** of clip sizes.
- Keep fractional Kelly logic the same.
- Scale clips proportionally to available capital.

### Recommended Tiny-Live Clip Sizing

For your current $50 tiny-live wallet, use these values in the YAML:

```yaml
clip_sizing:
  base_clip_usd: 0.75          # ~1.5% of $50
  min_clip_usd: 0.25           # absolute floor
  max_clip_usd: 2.0            # ~4% of $50 max per clip
  fractional_kelly: 0.15       # unchanged
```

**Why these numbers?**
- Base clip of $0.75 is small enough to allow many trades and multiple windows.
- Max clip of $2.0 keeps worst-case exposure very safe.
- Still large enough to test real merge mechanics and order flow.

### How to Implement This Cleanly

**Option 1 (Recommended)**: Add a capital scaler in the YAML

```yaml
clip_sizing:
  base_clip_usd: 1.5
  min_clip_usd: 0.5
  max_clip_usd: 4.0
  fractional_kelly: 0.15
  capital_scale_factor: 0.5     # ← set to 0.5 for tiny-live, 1.0 for full live
```

Then in code the engine does:
```rust
effective_base = config.clip_sizing.base_clip_usd * config.clip_sizing.capital_scale_factor
```

**Option 2**: Separate YAML files
- `btc_5m_pair_cost_arb.tinylive.yaml` → uses the smaller numbers above
- `btc_5m_pair_cost_arb.live.yaml` → uses the larger full-live numbers

### Final Recommendation for You Right Now

Use **Option 1** (add `capital_scale_factor`) — it’s the cleanest long-term solution.

For your immediate tiny-live deployment, set:

```yaml
clip_sizing:
  base_clip_usd: 1.5
  min_clip_usd: 0.5
  max_clip_usd: 4.0
  fractional_kelly: 0.15
  capital_scale_factor: 0.5     # tiny-live scaling
```

This keeps the config simple while giving you safe, testable clip sizes.

## Testing 

For a **high-frequency trading bot** like yours (Polymarket 5-min BTC Pair-Cost strategy + existing paired-MM engine), the testing strategy should be **TDD-first with BDD-style readability on top**. Here’s the precise recommendation:

### Recommended Approach: **TDD-First + BDD-Inspired Naming + Property-Based Testing**

| Layer              | Approach          | Why it fits your bot                                      | % of tests |
|--------------------|-------------------|-----------------------------------------------------------|------------|
| **Unit / Pure functions** | **TDD**           | Pair-cost math, fair-value model, projected_pair_cost, cheap-leg detection, EV calculations must be mathematically perfect | 60–70% |
| **Property / Edge cases** | **Property-based** (proptest) | Floating-point precision, extreme vol, near-zero τ, merge edge cases | 15–20% |
| **Integration / Strategy behavior** | **TDD + BDD-style** | Full decision loop, merge logic, late-window convexity | 15–20% |
| **End-to-end simulation** | **Simulation tests** | Replay historical books + BTC ticks | 5% |

**Why not pure BDD?**  
BDD (cucumber-style) adds too much overhead for low-level quant logic. Instead, we get the **benefits of BDD** (readable, business-oriented tests) simply by writing very descriptive test names and using a clean test structure.

### Concrete Test Configuration for Your Rust Engine

#### 1. Core Testing Stack (add these to `Cargo.toml` if not present)

```toml
[dev-dependencies]
proptest = "1.6"
proptest-derive = "0.5"
mockall = "0.13"
tokio = { version = "1", features = ["test-util"] }
wiremock = "0.6"          # for mocking external APIs if needed
```

#### 2. Recommended Test Organization

```
tests/
├── unit/                  # Pure TDD tests (most important)
│   ├── pair_cost_math.rs
│   ├── fair_value_model.rs
│   ├── cheap_leg_signal.rs
│   └── convexity_rule.rs
├── property/              # proptest
│   └── pair_cost_properties.rs
├── integration/           # Strategy behavior (BDD-style naming)
│   ├── strategy_decision_loop.rs
│   ├── merge_behavior.rs
│   └── late_window_convexity.rs
├── simulation/            # Full historical replay
│   └── backtest_replay.rs
└── fixtures/              # test data (JSON books, BTC ticks, etc.)
```

#### 3. Example Test Styles (Copy-Paste Ready)

**Unit / TDD example** (`tests/unit/pair_cost_math.rs`):

```rust
#[test]
fn test_projected_pair_cost_buy_yes() {
    let state = PairCostState { qty_yes: 1000.0, cost_yes: 520.0, qty_no: 800.0, cost_no: 360.0, ..Default::default() };
    let result = state.projected_pair_cost(Leg::Yes, 200.0, 0.48);
    assert!((result - 0.9833).abs() < 0.0001);
}
```

**Property-based (critical for floats)**:

```rust
proptest! {
    #[test]
    fn pair_cost_always_between_0_and_2(
        qty_yes in 0.0..10000.0,
        cost_yes in 0.0..10000.0,
        qty_no in 0.0..10000.0,
        cost_no in 0.0..10000.0,
    ) {
        let state = PairCostState { qty_yes, cost_yes, qty_no, cost_no, ..Default::default() };
        let pc = state.pair_cost();
        prop_assert!(pc >= 0.0 && pc <= 2.0);
    }
}
```

**Integration / BDD-style** (readable like user stories):

```rust
#[tokio::test]
async fn when_leg_is_cheap_and_pair_cost_below_threshold_then_buy_and_update_state() {
    // Given a cheap YES leg with pair_cost = 0.982
    // When the strategy runs its decision loop
    // Then it should place a buy order on YES and update averages
}
```

### Final Recommendations for Your Team

1. **Start with TDD on the new Pair-Cost logic** (pair-cost math + cheap-leg signal + merge) — this is the highest ROI.
2. **Use descriptive test names** so the tests read like BDD scenarios (no need for full Cucumber).
3. **Require 100% test coverage** on all pure math functions (`PairCostState`, fair-value model, etc.).
4. **Add a `tests::simulation` module** that replays real historical 5-min windows + orderbooks (this will catch most integration bugs).
5. Run tests in CI with `cargo test --all-features` + proptest.

This hybrid approach gives you:
- Mathematical correctness (TDD)
- Excellent readability and documentation (BDD-style names)
- Robustness against edge cases (property tests)

Would you like me to:
- Write the full set of starter test files for the Pair-Cost strategy?
- Or update the comprehensive audit document with a full “Testing Strategy” section?

Just say the word and I’ll deliver it.

**End of Document**
```

This is the complete, clean Markdown document you can copy and send directly to the other agent. Let me know if you need any final tweaks before handing it over!
