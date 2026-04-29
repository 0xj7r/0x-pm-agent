# Asymmetric Core+Hedge Strategy — Implementation Spec

Status: **draft**, ready for implementation
Author: 2026-04-29
Target reader: implementing agent (Rust). Should be buildable in ~1-2 hours given this spec.

---

## TL;DR

Add a new entry path to `btc_5m_mm` strategy that fires **late in a 5-minute bar** when one leg has clearly become the favored winner, the implied price is in the $0.85-$0.97 band, and BTC volatility is high enough that the directional move is non-trivial. Buy the **expensive (winning-side) leg** with passive maker-priced limit orders, time-in-force GTD with short TTL.

This complements the existing paired bidding (which dominates in mid-prices, $0.20-$0.80) and the heavily-gated convex_accum (which captures the 3.5% cheap-leg sliver). The new path captures the **13-25% of whale notional spent on late-bar expensive accumulation** that our engine currently has no path for.

---

## Background and empirical justification

### What whale data shows

Across **40 days of unlawful-shear (`0xb27bc932bf8110d8f78e55da7d5f0497a18b5b82`) trading on `btc-updown-5m-*` markets** (Mar 20 - Apr 29, 2026; ~520k trades, ~$5M notional), broken down by (time-in-bar, price-band):

```
$ notional by (time, price) bucket
                 cheap (<$0.20) | mid ($0.20-0.80) | expensive (>$0.80)
early bar       |  $    7,553    |  $    504,636    |  $     48,878
mid bar         |  $   22,941    |  $    369,268    |  $    161,975
late bar        |  $   27,455    |  $    272,720    |  $    250,693  ← thesis
─────────────────────────────────────────────────────────────────────
TOTAL           |  $   57,949    |  $  1,146,624    |  $    461,547
                |   (3.5%)        |   (68.8%)         |   (27.7%)
```

- **Late-bar expensive accumulation (>$0.80, last 100s of 5min bar) = 15% of total notional**, present in 67% of markets across all 40 days
- Pearson correlation: dispersion ↔ late-bar-expensive-% = **r = +0.688** (strong; high-vol days have 20% late-exp, low-vol days have 12%)
- Cheap-leg accumulation is 3.5% of notional and stable across vol regimes — clearly NOT the primary side bet

### What our engine currently does

| Path | Captures | Contribution to whale's mix |
|---|---|---|
| Paired bidding (`build_paired_entry_ladder`) | Mid-prices, throughout bar | ~58% of whale notional |
| `convex_accum` (heavily gated) | Cheap leg (<$0.45), trend-persistence-gated | ~3.5% of whale notional |
| **Missing** | **Late-bar expensive (>$0.85, last 90s)** | **~13-15% of whale notional** |

This spec adds the missing path.

---

## Strategy intent

When a 5-minute BTC market is approaching resolution and one side has become clearly favored (BTC moved meaningfully in one direction vs `price_to_beat`), the venue prices the favored leg at $0.85-$0.97. At those prices:

- Buy at $0.93, win $1.00 = **$0.07 locked-in profit per share** at ~93%+ probability
- Plus maker rebate on the fill
- Fail mode: tail risk of last-second BTC reversal — bounded because we only buy when vol-confirmed directional move

This is **high-probability, low-margin accumulation**, complementary to:
- Paired bidding (medium probability, medium margin)
- Convex_accum (low probability, high margin — held tightly because it's a lottery)

The whale's edge: better calibration of resolution probability than the venue at the late-bar mark. With 60s remaining and BTC clearly down vs price-to-beat, true probability of DOWN winning may be 99%+ even when venue prices DOWN at $0.93. That 6c gap (99% × $1.00 - $0.93 = ~$0.06) is the edge.

---

## Trigger gates (ALL must be true to fire)

```
1. time_remaining_in_bar_ms ∈ [30_000, 120_000]
   - Lower bound: <30s left, market may resolve before our maker bid fills (waste)
   - Upper bound: >120s left, prices haven't converged yet (use paired bidding)

2. expensive_leg_book_ask ∈ [0.85, 0.97]
   - <0.85: not "clearly favored" yet — let paired bidding handle
   - >0.97: locked-in profit too small (3¢) vs gas + slippage risk
   - This is per-leg ask, NOT fair value — we're targeting the actual book

3. realized_vol_5m_bps >= LATE_BAR_CORE_MIN_VOL_BPS (default 50)
   - In low-vol bars, late-stage prices can flip back without warning
   - Whale data shows late-exp share doubles when vol > median
   - Gate ensures we're in a vol regime where direction is durable

4. macro_signal_confirms_direction:
   - If expensive leg is UP (price > 0.50): require BTC spot >= price_to_beat × (1 + LATE_BAR_CORE_MOMENTUM_FLOOR_BPS / 10_000)
   - If expensive leg is DOWN: require BTC spot <= price_to_beat × (1 - LATE_BAR_CORE_MOMENTUM_FLOOR_BPS / 10_000)
   - LATE_BAR_CORE_MOMENTUM_FLOOR_BPS default: 5 (i.e., spot must be at least 5 bps in the favored direction)
   - Without spot reading, skip (no trade)

5. per_bar_count_under_cap:
   - state.late_bar_core_bids_this_bar < LATE_BAR_CORE_MAX_BIDS_PER_BAR (default 4)
   - Reset when convex_bar_end_ms changes (reuse existing tracking field; rename if needed)

6. budget_under_cap:
   - per-leg cumulative spend on this market this bar < LATE_BAR_CORE_BUDGET_USD (default 5.0)
   - Per-bar cap, separate from convex budget to avoid path conflict

7. existing_position_check:
   - If we ALREADY hold this leg with avg_cost > 0.80, skip (don't average up at near-resolution)
   - Acceptable to hold pre-existing inventory and add MORE only if avg_cost < 0.80

8. crossover_with_convex:
   - If convex_accum is firing this tick on the SAME market, the late-bar-core path takes precedence and convex_accum is suppressed for the bar
   - Both can fire on the same market across different ticks but not the same one
```

If any gate fails, return `Some(reason)` from `late_bar_core_skip_reason()` and do not emit an intent.

---

## Order construction

When all gates pass:

```rust
OrderIntent {
    market_id: snapshot.market_id.clone(),
    instrument_id: expensive_leg_id.clone(),
    side: TradeSide::Buy,
    limit_price: passive_maker_price,  // see below
    quantity: late_bar_core_qty,        // see below
    reduce_only: false,
    quote_level_tag: Some("mm-late-bar-core:l1".to_string()),
    pair_id: None,                       // single leg, not paired
    kind: IntentKind::Entry,             // accumulation, subject to entry caps
    reason: format!("late-bar core accumulation: ask={ask:.4} fair={fair:.4} remaining_ms={remaining}"),
    created_at_ms: now_ms,
    client_order_id: ClientOrderId::from(format!(
        "btc-5m-mm:{market}:{instrument}:b:n:mm-late-bar-core:l1:attempt-{now}:..."
    )),
}
```

### Passive maker price

```
passive_maker_price = book_best_bid (resting at top of bid queue)
```

Rationale: rest at the top of the bid queue. Aggressive enough to fill quickly if a seller hits us at the bid, but doesn't cross the book (preserving maker rebate). If the book moves up before fill, the order expires (TTL handles this).

Floor: must be ≥ $0.85 (the trigger gate's lower bound). If best_bid < $0.85, do not emit.

### Quantity sizing

```
qty = min(
    LATE_BAR_CORE_BUDGET_USD / passive_maker_price,
    venue_max_quantity_for_leg(this_market, expensive_leg_id),
    remaining_late_bar_core_budget_for_market_this_bar / passive_maker_price,
)
qty = max(qty, venue_min_order_quantity)  // upsize to venue minimum if budget allows
qty = floor_to_tick(qty)  // 2 decimal places, per V2 SDK requirement
```

If qty < venue_min_order_quantity, skip (under-sized order would reject).

### Time-in-force

In `submit_request_from_intent` (runtime/runner.rs:3681), add a branch for `mm-late-bar-core` tag:

```rust
let is_late_bar_core = intent.quote_level_tag.as_deref()
    .map(|tag| tag.starts_with("mm-late-bar-core"))
    .unwrap_or(false);
let live_expires_at_ms = (!execution_policy.paper_mode
    && execution_policy.live_order_ttl_ms > 0
    && !is_hedge_rescue
    && !is_late_bar_core)
    .then_some(...)
    .or_else(|| if is_late_bar_core {
        // 60-second TTL for late-bar core: fill or expire
        Some(observed_at_ms.saturating_add(60_000))
    } else { None });
let (time_in_force, post_only) = if is_hedge_rescue {
    (TimeInForce::Ioc, false)
} else if is_late_bar_core {
    (TimeInForce::Gtd, !execution_policy.paper_mode)
} else if live_expires_at_ms.is_some() {
    (TimeInForce::Gtd, !execution_policy.paper_mode && execution_policy.live_post_only)
} else {
    (TimeInForce::Gtc, !execution_policy.paper_mode && execution_policy.live_post_only)
};
```

Late-bar-core orders are always GTD with 60s TTL and always post_only.

---

## File changes

### `polymarket-exec/src/strategy.rs`

**New constants** (place near other CONVEX_* constants around line 1670-1690):

```rust
/// Late-bar expensive-leg accumulation path. Fires when one side has
/// clearly become the favored winner late in the 5min bar. See
/// docs/strategy/asymmetric_core_hedge_spec.md for empirical justification.
const LATE_BAR_CORE_PRICE_FLOOR: f64 = 0.85;
const LATE_BAR_CORE_PRICE_CEILING: f64 = 0.97;
const LATE_BAR_CORE_TIME_REMAINING_MS_MIN: u64 = 30_000;
const LATE_BAR_CORE_TIME_REMAINING_MS_MAX: u64 = 120_000;
const LATE_BAR_CORE_MIN_VOL_BPS: f64 = 50.0;
const LATE_BAR_CORE_MOMENTUM_FLOOR_BPS: f64 = 5.0;
const LATE_BAR_CORE_BUDGET_USD: f64 = 5.0;
const LATE_BAR_CORE_MAX_BIDS_PER_BAR: u32 = 4;
const LATE_BAR_CORE_TTL_MS: u64 = 60_000;
```

**New per-market state field** in `Btc5mMmMarketState` (around line 620-650):

```rust
late_bar_core_bids_this_bar: u32,
late_bar_core_bar_end_ms: Option<EpochMillis>,
late_bar_core_spend_this_bar_usd: f64,
```

**New skip-reason function** (mirrors `convex_skip_reason`, place near it around line 2786):

```rust
/// Returns Some(reason) if late-bar core accumulation should be skipped.
/// None means "go ahead and call build_late_bar_core_intent".
fn late_bar_core_skip_reason(
    &self,
    market_id: &MarketId,
    expensive_leg_id: &InstrumentId,
    expensive_leg_quote: &QuoteSnapshot,
    expensive_leg_fair: f64,
    btc_regime: &crate::signals::BtcRegimeSnapshot,
    market_context: Option<&MarketContextRecord>,
    now_ms: EpochMillis,
    inventory: &InventorySnapshot,
) -> Option<String> {
    // Gate 1: time remaining
    let ctx = market_context?;
    let remaining_ms = Self::time_remaining_ms(Some(ctx), now_ms)?;
    if remaining_ms < Self::LATE_BAR_CORE_TIME_REMAINING_MS_MIN {
        return Some(format!("late-bar-core skip: too late, remaining={}ms", remaining_ms));
    }
    if remaining_ms > Self::LATE_BAR_CORE_TIME_REMAINING_MS_MAX {
        return Some(format!("late-bar-core skip: too early, remaining={}ms", remaining_ms));
    }
    // Gate 2: expensive leg ask in band
    let ask = Self::best_ask(expensive_leg_quote)?;
    if ask < Self::LATE_BAR_CORE_PRICE_FLOOR {
        return Some(format!("late-bar-core skip: ask {ask:.4} below floor"));
    }
    if ask > Self::LATE_BAR_CORE_PRICE_CEILING {
        return Some(format!("late-bar-core skip: ask {ask:.4} above ceiling"));
    }
    // Gate 3: vol regime
    let vol = btc_regime.realized_vol_5m_bps.unwrap_or(0.0);
    if vol < Self::LATE_BAR_CORE_MIN_VOL_BPS {
        return Some(format!("late-bar-core skip: vol {vol:.1}bps below min"));
    }
    // Gate 4: macro signal confirms direction
    let leg_is_up = Self::leg_is_up(expensive_leg_id, market_context)?;
    let spot = btc_regime.last_price?;
    let price_to_beat = ctx.price_to_beat?;
    if spot <= 0.0 || price_to_beat <= 0.0 {
        return Some("late-bar-core skip: no spot/price_to_beat".to_string());
    }
    let direction_bps = ((spot / price_to_beat) - 1.0) * 10_000.0;
    let confirmed = if leg_is_up {
        direction_bps >= Self::LATE_BAR_CORE_MOMENTUM_FLOOR_BPS
    } else {
        direction_bps <= -Self::LATE_BAR_CORE_MOMENTUM_FLOOR_BPS
    };
    if !confirmed {
        return Some(format!(
            "late-bar-core skip: direction not confirmed (leg_is_up={leg_is_up} direction_bps={direction_bps:.1})"
        ));
    }
    // Gate 5: per-bar count
    let curr_bar_end = ctx.event_end_time_ms;
    if let Some(state) = self.market_states.get(market_id) {
        let same_bar = matches!(
            (state.late_bar_core_bar_end_ms, curr_bar_end),
            (Some(a), Some(b)) if a == b
        );
        if same_bar {
            if state.late_bar_core_bids_this_bar >= Self::LATE_BAR_CORE_MAX_BIDS_PER_BAR {
                return Some(format!(
                    "late-bar-core skip: bar count cap reached ({} bids)",
                    state.late_bar_core_bids_this_bar
                ));
            }
            if state.late_bar_core_spend_this_bar_usd >= Self::LATE_BAR_CORE_BUDGET_USD {
                return Some(format!(
                    "late-bar-core skip: bar budget reached (${:.2})",
                    state.late_bar_core_spend_this_bar_usd
                ));
            }
        }
    }
    // Gate 6: existing position avg cost
    for position in &inventory.positions {
        if position.market_id == *market_id
            && position.instrument_id == *expensive_leg_id
            && position.quantity > 1e-9
            && position.avg_price > 0.80
        {
            return Some(format!(
                "late-bar-core skip: avg_cost {:.4} too high to add",
                position.avg_price
            ));
        }
    }
    None
}
```

**New intent builder** (place near `build_convex_accumulation_intent`):

```rust
fn build_late_bar_core_intent(
    &self,
    market_id: &MarketId,
    expensive_leg_id: &InstrumentId,
    expensive_leg_quote: &QuoteSnapshot,
    expensive_leg_fair: f64,
    venue_rules: Option<&VenueMarketRules>,
    now_ms: EpochMillis,
) -> Option<OrderIntent> {
    let best_bid = Self::best_bid(expensive_leg_quote)?;
    if best_bid < Self::LATE_BAR_CORE_PRICE_FLOOR {
        return None;
    }
    let tick = self.tick_size(venue_rules);
    let venue_min_qty = venue_rules
        .map(|r| r.minimum_order_size)
        .filter(|m| m.is_finite() && *m > 0.0)
        .unwrap_or(self.config.venue_min_order_quantity);
    let target_qty = Self::LATE_BAR_CORE_BUDGET_USD / best_bid;
    let qty = target_qty.max(venue_min_qty);
    let qty = (qty * 100.0).floor() / 100.0;  // 2 decimal places per V2 SDK
    if qty < venue_min_qty {
        return None;
    }
    let price = Self::floor_to_tick(best_bid, tick);
    Some(Self::build_order(
        market_id.clone(),
        expensive_leg_id.clone(),
        TradeSide::Buy,
        price,
        qty,
        false,
        "mm-late-bar-core:l1".to_string(),
        format!(
            "btc-5m-mm late-bar core: ask={:.4} bid={:.4} fair={expensive_leg_fair:.4}",
            Self::best_ask(expensive_leg_quote).unwrap_or(0.0),
            best_bid
        ),
        IntentKind::Entry,
        now_ms,
    ))
}
```

**Wire into `on_market_snapshot`** (in the (true,true), (false,true), (false,false) branches that call paired/convex, around line 3450-3550):

After the convex_accum block, add:

```rust
// Late-bar expensive-leg accumulation: complements paired (mid prices) and
// convex (cheap legs) by capturing the late-bar high-probability winner.
// Independent of paired/convex; both gates and per-bar counters are separate.
let expensive_leg_id = if left_fair > right_fair { &left_id } else { &right_id };
let expensive_leg_quote = if left_fair > right_fair { &left_quote } else { &right_quote };
let expensive_leg_fair = left_fair.max(right_fair);
let late_skip = self.late_bar_core_skip_reason(
    &snapshot.market_id,
    expensive_leg_id,
    expensive_leg_quote,
    expensive_leg_fair,
    &context.btc_regime,
    context.market_context.as_ref(),
    context.now_ms,
    &context.inventory,
);
if let Some(reason) = late_skip {
    tracing::debug!(target: "strategy.late_bar_core_gate", market = %snapshot.market_id, reason, "late-bar core skipped");
} else if let Some(intent) = self.build_late_bar_core_intent(
    &snapshot.market_id,
    expensive_leg_id,
    expensive_leg_quote,
    expensive_leg_fair,
    context.venue_rules.as_ref(),
    context.now_ms,
) {
    // Track per-bar count and spend
    let curr_bar_end = context.market_context.and_then(|c| c.event_end_time_ms);
    if let Some(state) = self.market_states.get_mut(&snapshot.market_id) {
        let same_bar = matches!(
            (state.late_bar_core_bar_end_ms, curr_bar_end),
            (Some(a), Some(b)) if a == b
        );
        if !same_bar {
            state.late_bar_core_bids_this_bar = 0;
            state.late_bar_core_spend_this_bar_usd = 0.0;
        }
        state.late_bar_core_bar_end_ms = curr_bar_end;
        state.late_bar_core_bids_this_bar = state.late_bar_core_bids_this_bar.saturating_add(1);
        state.late_bar_core_spend_this_bar_usd += intent.limit_price * intent.quantity;
    }
    intents.push(intent);
}
```

### `polymarket-exec/src/runtime/runner.rs`

Add late-bar-core branch in `submit_request_from_intent` (line 3681) — see "Time-in-force" section above for diff.

### `polymarket-exec/src/strategy/tests.rs`

**TDD tests (write failing first, then implement above)**:

```rust
#[test]
fn late_bar_core_fires_when_all_gates_pass() {
    // Setup: 60s remaining, vol=70bps, BTC clearly down, DOWN ask=0.92, no existing inventory.
    // Expected: 1 intent with tag="mm-late-bar-core:l1", side=Buy, instrument=DOWN.
    let mut config = btc_5m_mm_test_config();
    config.inventory_skew_bps = 0.0;
    let mut strategy = Btc5mMmStrategy::new(config);
    let market_context = MarketContextRecord {
        market_id: "market-mm".to_string(),
        instrument_ids: vec!["up".to_string(), "down".to_string()],
        price_to_beat: Some(100.0),
        final_price: None,
        event_start_time_ms: Some(0),
        event_end_time_ms: Some(60_000),  // 60s remaining at now=0; we'll set now_ms=0
    };
    let btc_regime = crate::signals::BtcRegimeSnapshot {
        last_price: Some(99.0),  // 100 bps below price_to_beat → DOWN favored
        realized_vol_5m_bps: Some(70.0),
        observed_at_ms: 0,
        ..crate::signals::BtcRegimeSnapshot::default()
    };
    let ctx = context_at_with_market(Vec::new(), 0, market_context, btc_regime);
    strategy.on_market_snapshot(&ctx, &snapshot("up", "market-mm", 0.05, 0.07, 0));
    let decision = strategy.on_market_snapshot(&ctx, &snapshot("down", "market-mm", 0.92, 0.93, 0));

    let core_intents: Vec<_> = decision.intents.iter()
        .filter(|i| i.quote_level_tag.as_deref() == Some("mm-late-bar-core:l1"))
        .collect();
    assert_eq!(core_intents.len(), 1, "should fire one late-bar-core intent");
    let intent = core_intents[0];
    assert_eq!(intent.instrument_id, InstrumentId::from("down"));
    assert_eq!(intent.side, TradeSide::Buy);
    assert!((intent.limit_price - 0.92).abs() < 1e-9, "should bid at best_bid 0.92");
    assert!(intent.quantity * intent.limit_price <= 5.0 + 1e-9, "should cap at $5 budget");
}

#[test]
fn late_bar_core_skips_when_vol_too_low() {
    // Same setup but realized_vol_5m_bps=20 < 50 floor.
    // Expected: 0 late-bar-core intents.
    // ... (similar test, assert .filter().count() == 0 and reason note contains "vol")
}

#[test]
fn late_bar_core_skips_when_direction_not_confirmed() {
    // Setup: 60s remaining, vol=70bps, BTC nearly flat (last_price = price_to_beat).
    // Expected: skip with reason "direction not confirmed".
}

#[test]
fn late_bar_core_skips_when_already_holding_at_high_avg_cost() {
    // Setup: same gates pass, but inventory has expensive leg at avg=0.90 already.
    // Expected: skip with reason "avg_cost too high to add".
}

#[test]
fn late_bar_core_respects_per_bar_count_cap() {
    // Setup: fire 4 times, then on 5th tick verify it skips with "bar count cap".
}

#[test]
fn late_bar_core_uses_gtd_with_60s_ttl_at_submit() {
    // (Test the runner submit_request_from_intent change)
    // Build an OrderIntent with tag="mm-late-bar-core:l1".
    // Call submit_request_from_intent.
    // Assert time_in_force=Gtd, expires_at_ms = now+60_000, post_only=true (in non-paper).
}
```

---

## Edge cases / non-goals

- **Don't pursue early-bar core accumulation** — that's paired bidding's territory. Whale data shows expensive-leg buys early/mid-bar are ~$54K vs $250K late-bar (5× ratio).
- **Don't try to predict resolution probability beyond the band gate** — leave that to future signal work. The $0.85-$0.97 band is the proxy for "venue thinks this is the favored leg."
- **Don't average up an expensive position** — gate 7 prevents this. If avg_cost > 0.80 already, hold what we have.
- **Don't fire if BTC spot or price_to_beat are missing** — fail-safe to no-trade.
- **Don't pursue this on illiquid books** — implicit via the venue_min_order_quantity check + book-best-bid being the price source. If best_bid is far from a real fillable level, qty will be small or zero.

---

## Acceptance criteria

1. All new constants compile with explicit `// JUSTIFY:` comments referencing this spec.
2. `late_bar_core_skip_reason` returns Some/None per the gate logic described.
3. `build_late_bar_core_intent` returns an `OrderIntent` with correct tag, side, price (= best_bid), and qty (≥ venue_min, ≤ budget cap).
4. `submit_request_from_intent` applies GTD + post_only=true + 60s TTL for late-bar-core tags.
5. All new tests (above) pass; all existing tests still pass.
6. Live deploy: with `LATE_BAR_CORE_MIN_VOL_BPS=50`, expect to see ~5-15 late-bar-core fills per day at our $500 capital scale (one per market that hits all gates).
7. Per-cycle expected gain: ~$0.05-$0.10 per share on a 5-share clip = $0.25-$0.50 per fill. At 5-15 fills/day = ~$1-$8/day rebate-aside revenue.

---

## Future / out-of-scope refinements

- **Vol-derived dynamic price floor**: currently `0.85` is constant. Could derive from realized vol (high vol → wider band, narrower at low vol).
- **Per-market activity scoring**: prefer firing on markets with higher bid-side depth (the late-bar-core gets filled faster).
- **Cross-market budget**: currently per-bar budget is per-market. Could add a global per-bar core budget cap to prevent over-deploying capital in lockstep.
- **Hedge-aware**: if we already hold the OPPOSITE (cheap) leg via paired bidding, the late-bar-core fill is a paired completion — could integrate with merge planning.

These are explicitly NOT part of this spec — keep the first version simple.

---

## V2: Signal-derived constants

Per CLAUDE.md "Gate calibration" principle, V1 ships with constants for safety; V2 (planned within 2-4 weeks of V1 deploy, if V1 validates) makes them signal-derived. Specific mapping:

| V1 constant | V1 default | V2 signal source |
|---|---|---|
| `LATE_BAR_CORE_PRICE_FLOOR` | 0.85 | `0.80 + 0.10 × clamp(realized_vol_5m_bps / 100, 0, 1)` |
| `LATE_BAR_CORE_PRICE_CEILING` | 0.97 | Stays constant (margin-floor = gas + fees) |
| `LATE_BAR_CORE_TIME_REMAINING_MS_MIN` | 30_000 | `bar_window_ms / 10` |
| `LATE_BAR_CORE_TIME_REMAINING_MS_MAX` | 120_000 | `bar_window_ms × 0.4` |
| `LATE_BAR_CORE_MIN_VOL_BPS` | 50 | Trailing 30-day percentile (e.g., 60th pctile of vol distribution) |
| `LATE_BAR_CORE_MOMENTUM_FLOOR_BPS` | 5 | `realized_vol_5m_bps × 0.1` |
| `LATE_BAR_CORE_BUDGET_USD` | 5.0 | `bankroll_usd × 0.01` + self-feedback adjuster |
| `LATE_BAR_CORE_MAX_BIDS_PER_BAR` | 4 | `LATE_BAR_CORE_BUDGET_USD / typical_clip_usd` |

V2 must preserve V1 as a fallback (`Option<f64>` env override per knob). Never remove the constant; signal-derivation reads ARE constants when signal data is unavailable (`unwrap_or(default)`).

## V3: Self-feedback loop (future)

Once V2 ships and we have ~30 days of late-bar-core attribution data, add per-path P&L feedback:

- Per-day rolling P&L per `quote_level_tag` (paired vs convex vs late-bar-core vs hedge-rescue)
- Auto-scale `LATE_BAR_CORE_BUDGET_USD` ±25% if 7-day P&L is consistently above/below target
- Auto-tighten gates if hit-rate (filled / fired) drops below 40%
- Kill-switch path: pause path entirely if 30-day P&L < -$X (configurable circuit breaker)

V3 is explicitly out of scope for the V1 spec. Reference here so future work has a clear hook.

## References

- Whale data: 40-day analysis at `/tmp/whale_combined_analysis.py` + cached JSON at `/tmp/unlawful_2026-04-{24..29}_rows.json`
- Vol correlation: r=0.688 between price dispersion and late-bar-expensive %
- Existing convex code: `polymarket-exec/src/strategy.rs:2796` (`convex_skip_reason`) and `:3528` (`build_convex_accumulation_intent`)
- Submit pipeline: `polymarket-exec/src/runtime/runner.rs:3681` (`submit_request_from_intent`)
- TIF reference: `polymarket-exec/src/wire/execution_adapter.rs:63` (`TimeInForce` enum)
- Roadmap entry: `LIVE_EXECUTION_PRIORITY_CHECKLIST.md` item 54
