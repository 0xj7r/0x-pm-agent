# Unlawful v2 Regime Gate Spec

Status: implementation handoff  
Date: 2026-04-23  
Target: `whale-pair-exec`  
Strategy sleeve: `unlawful_shear`

## 1. Purpose

This document specifies the exact signal layer and regime gate that should be added to the existing `unlawful_shear` Rust strategy.

This is not a fresh strategy rewrite.

The execution geometry already exists and is directionally correct:

- expensive core
- cheap hedge
- repeated clip-based rebalancing
- fast merge recycling
- late-window cleanup

What is missing is the activation and aggression layer.

The goal of this spec is to define:

- when the sleeve should turn on
- when a specific market should be suppressed
- when it should stop opening new risk
- which BTC, order-book, and volume signals it should rely on
- how those signals should be wired into the runtime

This spec is intended to be implementable without further reverse engineering.

## 2. Current Strategy Boundary

Current `unlawful_shear` behavior is concentrated in:

- [strategy.rs](/Users/jackreid/go/polymarket-agent/whale-pair-exec/src/strategy.rs)

Current runtime/config surfaces are:

- [config/mod.rs](/Users/jackreid/go/polymarket-agent/whale-pair-exec/src/config/mod.rs)
- [runtime/mod.rs](/Users/jackreid/go/polymarket-agent/whale-pair-exec/src/runtime/mod.rs)
- [runtime/runner.rs](/Users/jackreid/go/polymarket-agent/whale-pair-exec/src/runtime/runner.rs)
- [book.rs](/Users/jackreid/go/polymarket-agent/whale-pair-exec/src/book.rs)
- [types.rs](/Users/jackreid/go/polymarket-agent/whale-pair-exec/src/types.rs)
This spec should be implemented by extending those surfaces, not by replacing them.

Notes:

- `WHALE_PAIR_STRATEGY_PROFILE_PATH` is still supported by the runtime, but it is optional
- the current branch should be treated as `strategy mode + built-in defaults + env overrides`
- if named sleeves are introduced, they should be added deliberately as new profile artifacts rather than assumed from the deleted historical path

## 3. Grounding From The Exported Signal Pack

Canonical source:

- [unlawful_signal_pack.json](/Users/jackreid/go/polymarket-agent/data/research/wallet_research/unlawful-shear/unlawful_signal_pack.json)

Key empirical facts we are grounding on:

### Session distribution

Paired-window activity clusters heavily in:

- `11:00 UTC`: `234` windows
- `23:00 UTC`: `211` windows

Secondary buckets:

- `22:00 UTC`: `23` windows
- `10:00 UTC`: `10` windows

Thin buckets:

- `09:00 UTC`: `5`
- `20:00 UTC`: `5`
- `21:00 UTC`: `5`

Interpretation:

- the historical pack still shows clear concentration around `11` and `23`
- but live API validation on April 23, 2026 also showed a separate active block around `19:00 UTC`
- hours should therefore be treated as a prior, not as a hard on/off gate
- v1 paper mode should prefer historically strong hours and require stronger BTC/book confirmation elsewhere

### Entry timing

From the paired-window distribution:

- `entry_lag_s` median: `9`
- 75th percentile: `12`
- 90th percentile: `82`

Interpretation:

- normal behavior is very early
- late windows exist, but they are exceptions
- new-market entry should be concentrated in the first `30s`

### Merge timing

- `first_merge_lag_from_entry_s` median: `26`
- 75th percentile: `36`
- 90th percentile: `58`

Interpretation:

- if no first merge has happened within about `60s` of first fill, the market is stalling relative to the observed norm

### Price geometry

- `cheap_leg_avg_price`
  - median: `0.296`
  - 75th percentile: `0.402`
  - 90th percentile: `0.469`
- `expensive_leg_avg_price`
  - 10th percentile: `0.559`
  - median: `0.696`
  - 90th percentile: `0.837`
- `price_gap`
  - 25th percentile: `0.217`
  - median: `0.389`
- `hedge_cost_ratio`
  - 25th percentile: `0.231`
  - median: `0.426`
  - 75th percentile: `0.653`

Interpretation:

- the strategy is not a 50/50 complement buyer
- it is an expensive-core / cheap-hedge strategy
- the live gate should key off this geometry directly

### BTC regime context

- `btc_realized_vol_5m_bps`
  - 25th percentile: `4.76`
  - median: `7.35`
  - 75th percentile: `10.87`
- `btc_realized_vol_15m_bps`
  - 25th percentile: `11.28`
  - median: `15.27`
  - 75th percentile: `21.14`
- `btc_trade_count_5m`
  - 25th percentile: `5,631`
  - median: `8,365`
  - 75th percentile: `11,783`

Interpretation:

- BTC directional sign is not the main trigger
- BTC activity and realized volatility are the useful regime signals

### Broad participation read

Live April 23, 2026 activity materially updates the earlier interpretation:

- once active, `unlawful` appears to participate in nearly every consecutive BTC 5m market in the session
- the strategy does not look like a sparse-event picker
- the signal layer should therefore be designed primarily to:
  - enable or disable the sleeve
  - scale aggression
  - shape core vs hedge asymmetry
  - suppress obviously bad windows

Interpretation:

- default per-market posture should be permissive once the sleeve is active
- the strongest role of the signal layer is not rare-entry selection
- the strongest role of the signal layer is sizing, skew, and suppression

## 4. What We Should Rely On

### Use as primary sleeve-enable signals

1. session hour
2. BTC realized vol
3. BTC trade count / activity
4. runtime health:
   - market websocket fresh
   - BTC spot feed fresh
   - no major cleanup backlog

### Use as primary per-market control signals

1. paired Polymarket book geometry:
   - cheap leg ask
   - expensive leg ask
   - price gap
   - both books present and fresh
2. early market flow quality
3. current pair / cleanup state
4. short-horizon BTC shock and local volatility burst

### Use as primary aggression signals

1. cheap leg price
2. expensive leg price
3. gap between legs
4. cheap/expensive notional ratio so far
5. fill balance across both legs
6. BTC realized vol / trade count

### Use as secondary / risk signals

1. short-horizon BTC shock
2. absence of merge progress
3. stale or missing book updates
4. late-window time remaining

### Do not use as primary entry signals

1. BTC directional sign by itself
2. raw complement ask sum by itself
3. fixed order-book depth floors from the sparse historical capture
4. time-of-day as a hard close switch

Rationale:

- direction does not separate active vs inactive windows well enough
- top-of-book complement sums in the local book capture can reflect placeholder/opening-state quotes
- depth support is too sparse historically to justify a hard floor yet

## 5. Exact Decision Model

Introduce a new runtime-visible state for `unlawful_shear`:

```rust
pub enum UnlawfulExecutionMode {
    Standby,
    Entry,
    Manage,
    Cleanup,
    Flatten,
}
```

This must drive strategy permissions:

- `Standby`
  - no new buys
  - no new quote ladder
  - allow passive monitoring only
- `Entry`
  - allow `core-entry`
  - allow `hedge-probe`
  - allow `early-probe`
- `Manage`
  - allow `add-hedge`
  - allow `rebalance-core`
  - allow `rebalance-flip`
  - do not open a fresh market from zero inventory if the entry window has passed
- `Cleanup`
  - no new buy intents
  - allow merge/redeem
  - allow salvage / reduce-only cleanup
- `Flatten`
  - cancel all non-reduce-only working quotes
  - allow only flatten / merge / redeem actions

Introduce a separate runtime-visible aggression model:

```rust
pub enum UnlawfulAggressionTier {
    Suppressed,
    Light,
    Normal,
    Press,
}
```

This must control:

- whether the market is tradeable at all
- clip sizes
- cheap/core target ratio
- whether fresh core adds are allowed
- whether only cleanup is allowed

Design rule:

- `ExecutionMode` answers "what phase are we in?"
- `AggressionTier` answers "how hard should we trade this market right now?"

## 6. Exact Trigger Logic

### 6.1 Session classification

Classify each market window by its UTC start hour:

- `Preferred`: `10`, `11`, `19`, `22`, `23`
- `Neutral`: `09`, `12`, `20`, `21`, `00`
- `Opportunistic`: all other hours

Notes:

- `11` and `23` remain the strongest historically grounded hours
- `19` is promoted into `Preferred` because live April 23 activity clearly ran there
- the runtime should score session quality, not fully close the sleeve outside a small set of hours
- session class should only shift required confirmation strength and default aggression

### 6.2 BTC regime thresholds

Use rolling live BTC metrics.

Required live fields:

- `realized_vol_5m_bps`
- `realized_vol_15m_bps`
- `trade_count_5m`
- `trade_count_15m`
- `return_30s_bps`
- `return_60s_bps`
- `observed_at_ms`

#### Preferred-hour open thresholds

Open the gate only if all are true:

- `realized_vol_5m_bps >= 5.0`
- `realized_vol_15m_bps >= 11.0`
- `trade_count_5m >= 5000`

#### Neutral-hour open thresholds

Open only if all are true:

- `realized_vol_5m_bps >= 8.0`
- `realized_vol_15m_bps >= 15.0`
- `trade_count_5m >= 8000`

#### Opportunistic-hour open thresholds

Allow an opportunistic-hour window only if all are true:

- `realized_vol_5m_bps >= 12.0`
- `realized_vol_15m_bps >= 20.0`
- `trade_count_5m >= 10000`
- `price_gap >= 0.30`
- both books fresh and present

Implementation note:

- this is intentionally stronger, but not disabled by default
- the runtime should record all blocked opportunities by hour so the collector can learn whether the thresholds are too tight or too loose

### 6.3 Per-market admission philosophy

Once the sleeve is enabled:

- default decision should be `participate`
- the runtime should only suppress a market if clear negative conditions are present

This means the market-level model is:

- `default yes`
- `block only on bad structure or operational risk`

Required hard suppressions:

- missing or stale book on either side
- cleanup backlog above configured cap
- inventory imbalance above hard limit
- BTC shock state above configured shock threshold
- time remaining too short for fresh pair formation

Required soft suppressions:

- geometry materially worse than rolling session norm
- one-sided fills with no pair progress
- repeated clip rejection or unusable depth

### 6.4 Book geometry thresholds

Both instruments in the market must have:

- fresh books
- best bid present
- best ask present
- positive size on best bid and best ask

#### Hard entry band

Entry is allowed only if all are true:

- `cheap_ask <= 0.47`
- `expensive_ask >= 0.56`
- `expensive_ask <= 0.84`
- `price_gap >= 0.22`

This band is based on the observed 10th/90th and 25th percentile historical window geometry.

#### Preferred band

If all of these are true, allow full clip sizing:

- `cheap_ask <= 0.40`
- `expensive_ask >= 0.62`
- `expensive_ask <= 0.78`
- `price_gap >= 0.35`

If only the hard band is true, scale clips down by `0.6`.

### 6.5 Freshness thresholds

- `book_age_ms <= 1200`
- `btc_signal_age_ms <= 2000`

If either is stale:

- no new entry
- no new rebalance buys
- move to `Cleanup` if inventory exists
- otherwise `Standby`

### 6.6 Time-based execution states

Use market elapsed time from `event_start_time_ms`.

- `0s-30s`: `Entry` if session + BTC + book gate all pass
- `30s-210s`: `Manage`
- `210s-270s`: `Cleanup`
- `>=270s` or past market end: `Flatten`

### 6.7 Merge-stall logic

Track `first_fill_ms` and `first_merge_ms` per market.

If:

- inventory exists
- `first_merge_ms` is still `None`
- `now_ms - first_fill_ms > 60000`

Then force mode to `Cleanup`.

Rationale:

- historical 90th percentile of first merge lag is `58s`

### 6.8 BTC shock logic

Use BTC returns only as a risk overlay.

#### Soft shock

If either is true:

- `abs(return_30s_bps) >= 15.0`
- `abs(return_60s_bps) >= 20.0`

Then:

- suppress new `core-entry`
- allow only hedge repair, merge, and cleanup

#### Hard shock

If either is true:

- `abs(return_30s_bps) >= 25.0`
- `abs(return_60s_bps) >= 30.0`

Then force mode to `Cleanup`.

The hard shock threshold is aligned with the current `risk.disable_making_on_spot_shock_bps` default.

## 7. Exact Runtime Evaluation Order

The runtime must evaluate the gate in this order:

1. market context present?
2. market start/end present?
3. BTC signal fresh?
4. both books fresh and two-sided?
5. classify session bucket
6. evaluate BTC regime thresholds
7. derive cheap / expensive legs from live asks
8. evaluate hard vs preferred geometry band
9. evaluate time-based mode
10. apply merge-stall override
11. apply BTC shock override

This order is mandatory because later logic depends on earlier context.

## 8. Runtime Data Structures

Add the following new structs.

### 8.1 BTC regime snapshot

New file:

- `whale-pair-exec/src/signals/btc_regime.rs`

```rust
pub struct BtcRegimeSnapshot {
    pub last_price: Option<f64>,
    pub realized_vol_5m_bps: Option<f64>,
    pub realized_vol_15m_bps: Option<f64>,
    pub trade_count_5m: u64,
    pub trade_count_15m: u64,
    pub return_30s_bps: Option<f64>,
    pub return_60s_bps: Option<f64>,
    pub observed_at_ms: u64,
}
```

### 8.2 Paired-book signal

New file:

- `whale-pair-exec/src/signals/unlawful_gate.rs`

```rust
pub struct PairedBookSignal {
    pub cheap_instrument_id: InstrumentId,
    pub expensive_instrument_id: InstrumentId,
    pub cheap_bid: Option<BookLevel>,
    pub cheap_ask: Option<BookLevel>,
    pub expensive_bid: Option<BookLevel>,
    pub expensive_ask: Option<BookLevel>,
    pub price_gap: Option<f64>,
    pub observed_at_ms: u64,
    pub books_fresh: bool,
    pub both_sides_present: bool,
}
```

### 8.3 Market activity signal

New file:

- `whale-pair-exec/src/signals/market_activity.rs`

```rust
pub struct MarketActivitySignal {
    pub last_trade_event_count_10s: u32,
    pub last_trade_event_count_30s: u32,
    pub last_trade_event_count_60s: u32,
    pub last_trade_event_age_ms: Option<u64>,
}
```

This is informational in v1. Do not hard-block on these counts yet.

### 8.4 Combined unlawful signal snapshot

New file:

- `whale-pair-exec/src/signals/unlawful_gate.rs`

```rust
pub struct UnlawfulSignalSnapshot {
    pub session_bucket: SessionBucket,
    pub mode: UnlawfulExecutionMode,
    pub gate_reasons: Vec<String>,
    pub btc: BtcRegimeSnapshot,
    pub book: PairedBookSignal,
    pub activity: MarketActivitySignal,
    pub first_fill_ms: Option<u64>,
    pub first_merge_ms: Option<u64>,
    pub elapsed_s: Option<u64>,
    pub time_remaining_s: Option<u64>,
}
```

## 9. File Ownership

### New files

- `whale-pair-exec/src/signals/mod.rs`
- `whale-pair-exec/src/signals/btc_regime.rs`
- `whale-pair-exec/src/signals/market_activity.rs`
- `whale-pair-exec/src/signals/unlawful_gate.rs`
- `whale-pair-exec/src/wire/spot_ws.rs`

### Existing files to modify

- `whale-pair-exec/src/lib.rs`
  - export the new `signals` and `wire::spot_ws` modules

- `whale-pair-exec/src/book.rs`
  - add helper methods for depth and freshness if needed
  - do not place regime logic here

- `whale-pair-exec/src/config/mod.rs`
  - add spot feed config:
    - `WHALE_PAIR_EXEC_SPOT_WS_URL`
    - `WHALE_PAIR_EXEC_SPOT_SYMBOL`
  - load them into `AppConfig`

- `whale-pair-exec/src/strategy.rs`
  - extend `UnlawfulShearProfile` with new regime-gate fields
  - extend `StrategyContext` with `unlawful_signal: Option<UnlawfulSignalSnapshot>`
  - modify `UnlawfulShearStrategy::on_market_snapshot` to honor mode
  - current execution policy remains; gate decides whether execution branches are allowed

- `whale-pair-exec/src/runtime/mod.rs`
  - build `UnlawfulSignalSnapshot` before calling strategy
  - cancel working quotes when mode drops to `Cleanup` or `Flatten`
  - track `first_fill_ms` and `first_merge_ms` per market

- `whale-pair-exec/src/runtime/runner.rs`
  - spawn the BTC spot websocket client
  - plumb the signal stores into the runtime loop

- `whale-pair-exec/src/wire/market_ws.rs`
  - increment market activity counters on `last_trade_price`
  - optionally count `price_change` as a softer flow signal

- optional `WHALE_PAIR_STRATEGY_PROFILE_PATH` if we later reintroduce explicit named JSON profiles
  - add the new `unlawful_shear` regime fields below

## 10. Exact Config Additions

Add these fields to `strategies.unlawful_shear` in `StrategyProfile`.

```json
{
  "regime_primary_hours_utc": [11, 23],
  "regime_secondary_hours_utc": [10, 22],
  "allow_extreme_offhour_override": false,
  "entry_window_seconds": 30,
  "cleanup_start_seconds": 210,
  "close_start_seconds": 270,
  "merge_stall_seconds": 60,
  "entry_book_max_age_ms": 1200,
  "entry_btc_signal_max_age_ms": 2000,
  "primary_min_btc_realized_vol_5m_bps": 5.0,
  "primary_min_btc_realized_vol_15m_bps": 11.0,
  "primary_min_btc_trade_count_5m": 5000,
  "secondary_min_btc_realized_vol_5m_bps": 8.0,
  "secondary_min_btc_realized_vol_15m_bps": 15.0,
  "secondary_min_btc_trade_count_5m": 8000,
  "override_min_btc_realized_vol_5m_bps": 12.0,
  "override_min_btc_realized_vol_15m_bps": 20.0,
  "override_min_btc_trade_count_5m": 10000,
  "entry_cheap_ask_max": 0.47,
  "entry_expensive_ask_min": 0.56,
  "entry_expensive_ask_max": 0.84,
  "entry_price_gap_min": 0.22,
  "preferred_cheap_ask_max": 0.40,
  "preferred_expensive_ask_min": 0.62,
  "preferred_expensive_ask_max": 0.78,
  "preferred_price_gap_min": 0.35,
  "hard_shock_return_30s_bps": 25.0,
  "hard_shock_return_60s_bps": 30.0,
  "soft_shock_return_30s_bps": 15.0,
  "soft_shock_return_60s_bps": 20.0
}
```

Implementation note:

- array fields may remain profile-only for v1
- do not add env overrides for list fields unless there is a real operational need

## 11. Exact Strategy Behavior Changes

### Allowed in `Entry`

- `core-entry`
- `hedge-probe`
- `early-probe`

### Allowed in `Manage`

- `add-hedge`
- `rebalance-core`
- `rebalance-flip`
- `salvage-*` only if the existing salvage condition is already true

### Allowed in `Cleanup`

- `salvage-expensive`
- `salvage-cheap`
- reduce-only sells
- merge / redeem requests

### Allowed in `Flatten`

- all non-reduce-only buy logic disabled
- forced cleanup / close logic only

### Mandatory suppression rules

If mode is `Standby`, `Cleanup`, or `Flatten`, suppress:

- `core-entry`
- `hedge-probe`
- `early-probe`
- `add-hedge`
- `rebalance-core`
- `rebalance-flip`

unless the specific action is explicitly tagged as reduce-only cleanup.

## 12. Live Data Sources

### BTC spot

Implement a new Binance spot websocket client:

- symbol: `BTCUSDT`
- feed: aggregated trade stream

Reason:

- the local historical BTC series already came from Binance
- we need rolling returns, rolling realized vol, and trade counts
- the Polymarket book alone is not enough

### Polymarket market data

Continue using the existing Polymarket market websocket.

Use it for:

- top-of-book bids/asks
- book freshness
- last-trade event counts

## 13. Exact Runtime Pseudocode

```rust
fn evaluate_unlawful_mode(inputs: UnlawfulGateInputs) -> UnlawfulSignalSnapshot {
    if inputs.market_context_missing {
        return standby("missing market context");
    }
    if inputs.market_ended || inputs.elapsed_s >= close_start_seconds {
        return flatten("market near/past end");
    }
    if inputs.book_stale || inputs.btc_stale || !inputs.both_books_present {
        return cleanup_or_standby(inputs.has_inventory, "stale or incomplete signals");
    }

    let session_ok = match inputs.session_bucket {
        Primary => btc_primary_thresholds_pass,
        Secondary => btc_secondary_thresholds_pass,
        Off => allow_override && btc_override_thresholds_pass,
    };

    if !session_ok {
        return cleanup_or_standby(inputs.has_inventory, "session/btc regime closed");
    }

    if hard_btc_shock {
        return cleanup("hard btc shock");
    }

    if inputs.elapsed_s >= cleanup_start_seconds {
        return cleanup("late window");
    }

    if inputs.has_inventory && inputs.merge_stalled {
        return cleanup("merge stalled");
    }

    if !hard_geometry_pass {
        return cleanup_or_standby(inputs.has_inventory, "book geometry invalid");
    }

    if !inputs.has_inventory {
        if inputs.elapsed_s <= entry_window_seconds {
            return entry(if preferred_geometry_pass { 1.0 } else { 0.6 });
        }
        return standby("entry window missed");
    }

    if soft_btc_shock {
        return manage_restricted("soft btc shock");
    }

    manage(if preferred_geometry_pass { 1.0 } else { 0.6 })
}
```

`clip_scale` must be included in the signal snapshot so the strategy can reduce clip size without inventing new thresholds internally.

## 14. Tests

### Unit tests

Add tests in:

- `whale-pair-exec/src/signals/unlawful_gate.rs`
- `whale-pair-exec/src/strategy.rs`

Required cases:

1. primary-hour + BTC gate + hard geometry + early window => `Entry`
2. secondary-hour + weak BTC => `Standby`
3. off-hour + override disabled => `Standby`
4. off-hour + override enabled + extreme BTC => `Entry`
5. early window missed with zero inventory => `Standby`
6. existing inventory + merge stall > 60s => `Cleanup`
7. existing inventory + soft BTC shock => restricted `Manage`
8. existing inventory + hard BTC shock => `Cleanup`
9. elapsed >= 270s => `Flatten`

### Scenario tests

Add or extend:

- [btc_5m_mm_scenarios.rs](/Users/jackreid/go/polymarket-agent/whale-pair-exec/tests/btc_5m_mm_scenarios.rs)

Required scenario fixtures:

1. valid early entry window produces `core-entry` + `hedge-probe`
2. same market with regime closed produces no buy intents
3. merge stall drives cleanup-only behavior
4. late window suppresses new buys and emits only reduce-only actions

## 15. Acceptance Criteria

This implementation is complete only if all of the following are true:

1. `unlawful_shear` no longer fires solely from paired-book geometry.
2. Entry can occur only when session + BTC + book gate are open.
3. The runtime can distinguish `Entry`, `Manage`, `Cleanup`, and `Flatten`.
4. New buys are impossible in `Cleanup` and `Flatten`.
5. Missing/stale BTC or book data fail closed.
6. The strategy emits gate reasons into the event log for every suppressed market.
7. The strategy profile can configure every threshold listed above.
8. Unit and scenario tests cover every mode transition.

## 16. What Not To Do

Do not:

- use BTC drift sign as the main trigger
- hard-block on depth floors not supported by the data
- let the strategy open fresh markets after `30s`
- let the strategy keep adding core risk after a `60s` merge stall
- let `Cleanup` mode submit new buy orders

## 17. Immediate Follow-On

Once this lands for `unlawful_shear`, the same signal framework should be reused for:

- `Bonereaper`
- the maker-first BTC/ETH sleeves more broadly

But this `unlawful_v2` gate must be implemented first before generalizing.
