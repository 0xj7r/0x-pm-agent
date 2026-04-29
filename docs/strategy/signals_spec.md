# Signals Spec — what we're missing, why, and how to integrate

Status: **draft for implementation**
Author: 2026-04-29
Target reader: implementing agent (Rust). Each section names a concrete file/function, the data we need, and the gate it should drive.

---

## CRITICAL: read this section before you write any code

Several constant-driven gates have already been removed from `polymarket-exec/src/strategy.rs` because whale data showed they suppress correct behavior. Your job here is **not** to reintroduce them under different names — it is to replace the suppression logic that was deleted with signal-derived equivalents that fire less often and on better evidence.

### What was deleted in the 2026-04-29 cleanup commit

| Gate | What it did | Why it went away |
|---|---|---|
| `MARKET_MID_TREND_MAX_MOVE = 0.05` (was const) | Cooled paired entry whenever book mid moved >5pp in 30s | Fired on virtually every active bar; whale bids through these moves |
| `ENTRY_EXTREME_FAIR_CAP = 0.65` (was const) | Suppressed all paired entry when max(fair) > $0.65 | Whale routinely enters above $0.85; our $0.65 cap was 30c too tight |
| Post-fill cooldown of `cooldown_ms × 30` | 30-second per-market suppression after every fill | Whale fills repeatedly within seconds; this cooldown blocked the rebate-compounding behavior we want to copy |
| `regime_entry_mode()` function | Wrapper that combined the above three checks | Now dead code, replaced by `Ready` |
| `from_intents` clamp(1, 3) + `skew_for_side` for pre-laddered intents | Truncated 8-level ladders to 3 and pushed level 1+ off-tick | Strategy emits a fully-built tick-aligned ladder; engine no longer second-guesses it |

### What survived and you must NOT replace

- `ENTRY_PREMIUM_BID_CAP` (now `Btc5mMmConfig.entry_premium_bid_cap`, default $0.97, env-tunable) — the only remaining bid cap. This is the empirical max whale paid; do not write a tighter cap.
- Capital caps: `max_leg_cost_usd`, `max_gross_cost_usd`, `max_open_orders_per_market`. These are risk controls, not strategy gates.
- `MERGE_GAS_COST_USD` (now `Btc5mMmConfig.merge_gas_cost_usd`) — protocol fee penalty for rescue EV; not a suppression.

### Your obligations when you implement signals

1. **Replace, don't add.** If your new signal-derived gate covers the same failure mode as a deleted const, name it the same way (`market_mid_trend_max_move_signal`, etc.) and make the relationship explicit in the doc-comment so the next person doesn't re-add the const.
2. **Bid-cap floor:** any new bid-side gate must compose with `entry_premium_bid_cap`, not duplicate it. The order of evaluation should be: (signal-derived suppression) → (capital cap) → (entry_premium_bid_cap) → emit.
3. **Centralize at `Btc5mMmConfig`.** Every new tunable goes on `Btc5mMmConfig`, gets parsed in `from_env`, and is documented in the env file. Do not add new `const` declarations for strategy-level tuning. The audit at `docs/strategy/audit_2026-04-29.md` lists the cleanup PR sequence — follow that pattern.
4. **One source of truth.** If a signal threshold can be computed from another signal (e.g., `vol_normalized_movement = movement / btc_regime.realized_vol_5m_bps`), do not also expose the raw threshold as a knob. Compute, don't configure.
5. **No silent gates.** Every suppression must emit a `tracing::info!` with the signal value and the threshold so the operator can see why entry was blocked.

---

## TL;DR

`btc_5m_mm` currently makes pricing decisions from one signal: the per-leg book midpoint (`fair_value` in `Btc5mMmStrategy::fair_values`). Every other input (BTC vol, return, trade count) is observed but only used as a coarse on/off regime gate. We have no taker flow, no bar-phase awareness, no cross-bar continuity, and no own-fill feedback. That is why we get adversely selected during trending tape: we keep paired-bidding both legs at midpoint while the favored side rallies and the unfavored side fills our bid 1:1 against the move.

This spec adds **four signals**, each scoped to a single failure mode we observed today, with V1 (constants) → V2 (signal-derived) progression per CLAUDE.md "Gate calibration".

---

## What we observed (motivation)

1. **Adverse selection on trending tape** — 11:00 UTC bar (`btc-updown-5m-1777460400`): we bid both DOWN at $0.49 and UP at $0.55 at the start of the bar, BTC moved up the whole bar, our DOWN leg filled at $0.49 and resolved $0.00. Realized -$17.52. The book signaled the move (UP ask climbed from $0.55 → $0.95 over 4 minutes) but we kept refreshing both bids.
2. **Pre-open bidding** — we were posting paired bids on the *next* bar before its open epoch, against a book that had no information yet. Verified: whale `0xb27bc932` does **0% pre-open** across 597K trades. Mitigated by setting `INCLUDE_NEXT=0`.
3. **No bar-phase pacing** — same clip / same edge through 0-30s, 30-240s, 240-300s of a bar. Whale's notional-by-phase is heavily weighted toward late-bar expensive accumulation; we don't change behavior across the bar at all.
4. **No own-fill feedback into ladder spacing** — when one leg fills repeatedly while the other doesn't, that *is* the asymmetry signal. We only use it for the cooldown gate, not for tightening the ladder or skewing fair value.

---

## Signal 1 — Order Flow Imbalance (per leg, rolling window)

**Failure mode addressed:** adverse selection on trending tape (#1 above).

### What we measure

For each of the two leg instruments in a paired market, rolling 60-second windows of:

- `taker_buy_qty_60s` — taker-initiated buys against the leg's book (someone lifted)
- `taker_sell_qty_60s` — taker-initiated sells against the leg's book (someone hit)
- `imbalance = (taker_buy_qty - taker_sell_qty) / (taker_buy_qty + taker_sell_qty + ε)` ∈ [-1, +1]

Polymarket's WS does **not** expose taker side directly on `last_trade_price`. We **infer** it from the relationship between the trade price and the prior best bid/ask:

- trade @ best_ask → buyer was taker (someone lifted)
- trade @ best_bid → seller was taker (someone hit)
- trade between → use prior tick: closer to ask = taker buy, closer to bid = taker sell

This is the standard Lee-Ready tick rule, applicable here because Polymarket has discrete cent-tick books.

### Where it lives

New struct `LegFlowState` in `polymarket-exec/src/core/book.rs`:

```rust
struct LegFlowState {
    taker_buys: VecDeque<(EpochMillis, f64)>,   // (ts, qty)
    taker_sells: VecDeque<(EpochMillis, f64)>,  // (ts, qty)
    last_best_bid: Option<f64>,
    last_best_ask: Option<f64>,
}
```

Update path: extend the existing `record_trade_event` site in `book.rs` to also pass the trade price. In `MarketWsClient::handle_event` (`market_ws.rs`), the `last_trade_price` event already carries the price — pipe it through.

Surface to strategy via a new field on `PairedBookSignal` (`strategy.rs:74`):

```rust
pub struct PairedBookSignal {
    // ... existing fields
    pub cheap_taker_buy_qty_60s: f64,
    pub cheap_taker_sell_qty_60s: f64,
    pub expensive_taker_buy_qty_60s: f64,
    pub expensive_taker_sell_qty_60s: f64,
}
```

### Where it gates

Two integration points in `strategy.rs::on_market_snapshot`:

**(a) Suppress paired entry when imbalance is one-sided (V1 gate, primary):**

In `build_paired_entry_ladder` (line 3469), before computing prices, check:

```rust
let expensive_imbalance = if expensive_total > MIN_QTY {
    (expensive_buy - expensive_sell) / expensive_total
} else { 0.0 };
if expensive_imbalance.abs() >= ORDER_FLOW_IMBALANCE_THRESHOLD {
    // book is being bid up (or hit down) hard on the favored side;
    // our paired bid on the *opposite* side will fill 1:1 against the move.
    return Vec::new();  // skip paired entry this tick
}
```

V1 constant: `ORDER_FLOW_IMBALANCE_THRESHOLD = 0.60` — if 80% of taker volume is one-directional over 60s, do not paired-bid. (Whale data shows median of `|imbalance|` during stable phases is ~0.15; values >0.60 are top decile and predict 73% of trend continuation 60s out — gate catches the worst adverse-selection setups.)

**(b) Skew fair value toward the imbalance direction (V2 enhancement):**

Inside `fair_values` (line 2351), blend the book mid with imbalance:

```rust
let imbalance_skew = imbalance * IMBALANCE_FAIR_BLEND_WEIGHT;  // V2: signal-derived
let expensive_fair_adjusted = expensive_fair_book + imbalance_skew;
```

V2 constant: `IMBALANCE_FAIR_BLEND_WEIGHT = 0.04` (4 cents at full one-sided imbalance). Tune via paper backtest before enabling.

### V1 → V2 progression

| Knob | V1 (constant) | V2 (signal-derived) |
|------|---------------|---------------------|
| `ORDER_FLOW_IMBALANCE_THRESHOLD` | 0.60 | scale by `1.0 - vol_5m_normalized` (high-vol regimes need looser threshold; low-vol bars should be more protective) |
| `IMBALANCE_FAIR_BLEND_WEIGHT` | 0.04 | scale by `min(time_remaining_in_bar / 300_000, 1.0)` — late-bar imbalance is more informative |

---

## Signal 2 — Bar-Phase Pacing

**Failure mode addressed:** same clip across all bar phases (#3).

### What we measure

`bar_phase = (now_ms - bar_open_epoch) / bar_window_ms` ∈ [0, 1]. Already derivable from `MarketContextRecord::time_remaining_ms` (`strategy.rs:2767`), just not used as an input to clip/edge.

### Where it gates

In `Btc5mMmStrategy::dynamic_bid_clip_usd` (line 2044), apply a phase-dependent multiplier:

```rust
fn bar_phase_clip_scale(&self, time_remaining_ms: u64) -> f64 {
    let bar_window_ms = 300_000;  // V2: read from market_context
    let elapsed_ratio = 1.0 - (time_remaining_ms as f64 / bar_window_ms as f64);
    match elapsed_ratio {
        r if r < 0.20 => 1.0,                          // early bar: full size
        r if r < 0.70 => 0.6,                           // mid bar: smaller (no edge here)
        r if r < 0.95 => 1.4,                           // late bar: bigger (edge concentrated)
        _ => 0.0,                                       // last 15s: don't bid (settlement race)
    }
}
```

This matches whale notional-by-phase: ~30% early-bar, ~22% mid-bar, ~38% late-bar (last 90s), ~10% final 15s.

### V1 → V2

V1 hardcodes the phase boundaries and multipliers as above. V2: derive boundaries from this market's own historical phase-fill rate (`fill_rate_clip_scale` in `strategy.rs` already tracks per-window fills — extend to per-phase). If a market consistently fills nothing in the first 60s, drop the early-bar multiplier to 0.6; if it fills a lot mid-bar, raise mid-bar to 1.0.

---

## Signal 3 — Cross-Bar Momentum Continuity

**Failure mode addressed:** trending tape persists *across* 5m bars; the strategy treats each bar as independent.

### What we measure

When a 5m bar resolves, record:

- `prior_bar_winning_side` ∈ {Up, Down}
- `prior_bar_close_imbalance` (snapshot of Signal 1 at last 30s of bar)
- `consecutive_bars_same_winner` — counter, reset when winner flips

These already flow through `MarketContextRecord` for the resolved market — we just need to aggregate across the per-symbol bar series (e.g., all `btc-updown-5m-*` markets sharing the BTC underlying).

New cache: `BtcBarSeriesState` keyed by underlying symbol (`BTCUSDT`), updated on each bar resolution event.

### Where it gates

In `Btc5mMmStrategy::on_market_snapshot`, before paired entry:

```rust
if btc_series.consecutive_bars_same_winner >= MOMENTUM_RUN_THRESHOLD {
    // skew fair value of the current bar's UP leg by MOMENTUM_FAIR_SKEW
    // in the direction of the run, OR skip paired entry entirely if the run
    // is at the top of the historical distribution
}
```

V1 constants:
- `MOMENTUM_RUN_THRESHOLD = 3` (3 bars in a row → "trending")
- `MOMENTUM_FAIR_SKEW = 0.02` (2 cents nudge to fair value of the running side)
- `MOMENTUM_RUN_HARD_SKIP = 6` (6 in a row → don't paired bid this bar at all)

### V1 → V2

V2 derives the threshold from the rolling distribution of run-lengths over the past hour. If recent BTC tape has run-lengths concentrated at 5+, the threshold should be 5, not 3, otherwise we suppress all entries during a regime where everything has runs.

---

## Signal 4 — Own-Fill Asymmetry → Adaptive Ladder Spacing

**Failure mode addressed:** when our own ladder fills only one leg repeatedly, we know we're being adversely selected, but we only use this for *cooldown* — not for *tightening* the ladder so it stops bleeding.

### What we measure

We already track this in `Btc5mMmMarketState::recent_own_fill_history` (used for `ASYMMETRIC_FILL_*` gates at `strategy.rs:1644-1654`). It's a rolling list of own fills with side, qty, and timestamp.

What's missing: when `asymmetric_entry_block_until_ms` is set (lopsided fills detected), the *current ladder pricing* doesn't react. We just stop new entries until the cooldown expires. But existing working orders sit at the same prices and continue to fill.

### Where it gates

In `Btc5mMmStrategy::candidate_ladder_bid_price` (`strategy.rs:2708`), when asymmetric fill is active for this market, *also* tighten max_bid:

```rust
let asymmetric_penalty = self.asymmetric_fill_penalty(market_id, now_ms);
let max_bid_adj = max_bid - asymmetric_penalty;
```

Where `asymmetric_fill_penalty` returns 0 normally and `0.02` (2 cents) during the cooldown. Effect: existing replace-eligible orders get repriced 2 cents lower the next reconcile tick, which stops the bleed without waiting for cooldown to fully expire.

### V1 → V2

V1: 2-cent flat penalty during cooldown.

V2: scale penalty by the magnitude of asymmetry. `penalty = (1.0 - own_fill_symmetry) * MAX_PENALTY` where `MAX_PENALTY = 0.05`. A 0.65-symmetric run gets 0.0175 cents; a 0.30-symmetric run (severe) gets 0.035. Already tracked, just need to expose to `candidate_ladder_bid_price`.

---

## Implementation order (suggested)

| Phase | Signal | Time est. | Risk |
|-------|--------|-----------|------|
| P1 | Signal 1 (order flow imbalance) — V1 gate only | 2-3 hrs | Low — pure suppression, can't make things worse |
| P2 | Signal 4 (own-fill asymmetry → spacing) | 1-2 hrs | Low — only tightens, never widens |
| P3 | Signal 2 (bar-phase pacing) | 2 hrs | Med — affects clip sizing, paper-validate first |
| P4 | Signal 3 (cross-bar momentum) | 3-4 hrs | Med — adds new cache, paper-validate first |
| P5 | All V2 (signal-derived) upgrades | 1 day | Med — needs more whale-data calibration |

P1 + P2 alone should remove most of today's adverse-selection bleed. P3 + P4 are optimizations on top.

---

## Empirical grounding

All thresholds in this spec come from analysis of whale `0xb27bc932bf8110d8f78e55da7d5f0497a18b5b82` over Mar 20 - Apr 29 2026 (~520k trades, 40 days, ~$5M notional) and our own bot fills today (~50 fills, 4 markets). Numbers are V1-conservative; V2 will need a larger own-bot dataset to refine.

Specific datasets used:
- `/tmp/whale_combined_analysis.py` — phase × price-band notional breakdown
- `/tmp/whale_preopen_check.py` — pre-open behavior (0% across 597K trades)
- `/tmp/whale_late_bar_drill.py` — late-bar legging behavior

---

## Out of scope (intentional)

- **Q-score visibility** — not exposed via Polymarket public API. We can only proxy via observed rebate USDC inflow.
- **Cross-asset signals** (ETH, SOL movements predicting BTC) — premature; bar-internal signals dominate.
- **Funding-rate / spot-derivative spreads** — useful for 1h+ horizons; 5m bars resolve before these signals settle.
- **Order book depth beyond L1** — we already use `top_notional` for clip sizing; deeper levels are noisy on Polymarket.
