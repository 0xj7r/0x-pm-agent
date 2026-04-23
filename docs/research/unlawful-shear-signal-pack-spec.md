# Unlawful-Shear Signal Pack Spec

Status: implementation input  
Last updated: 2026-04-23  
Primary wallet: `unlawful-shear` / `0xb27bc932bf8110d8f78e55da7d5f0497a18b5b82`

## Purpose

The microstructure spec tells us how `unlawful-shear` behaves once active.

That is necessary, but not sufficient, to rebuild the strategy.

To turn the research into a usable Rust engine, we need a signal pack that separates:

1. activation policy: when the strategy should be on
2. execution policy: how it behaves once on
3. risk policy: when it should widen, skew, recycle, or stop

This document defines the canonical signal layers, the exact local sources we can derive them from, the observability gaps we still have, and the machine-readable export shape we should use as the handoff between research and implementation.

Primary local source:

- [wallet_research.db](/Users/jackreid/go/polymarket-agent/data/research/wallet_research/unlawful-shear/wallet_research.db)

Machine-readable exporter:

- [export_wallet_signal_pack.py](/Users/jackreid/go/polymarket-agent/scripts/export_wallet_signal_pack.py)

Related reconstruction docs:

- [unlawful-shear-reconstruction-thread.md](/Users/jackreid/go/polymarket-agent/docs/research/unlawful-shear-reconstruction-thread.md)
- [unlawful-shear-microstructure-spec.md](/Users/jackreid/go/polymarket-agent/docs/research/unlawful-shear-microstructure-spec.md)

## What We Are Actually Trying To Learn

For strategy reconstruction, the core question is not just:

- what orders did the wallet place?

It is:

- under what market regimes did it choose to participate?
- what book/spot geometry did it appear to react to?
- how did it size the core leg, hedge leg, and merge cycle as those signals changed?
- what late-window conditions triggered cleanup or salvage behavior?

That means the research pack must not be only a trade log. It must be a per-window state reconstruction.

## Signal Stack

### Layer 1: Regime Gate

This layer answers:

- should the engine run this market at all?
- should it run only during certain hours?
- should it stand down because conditions are thin, stale, or hostile?

Signals we want:

- session hour and day-of-week
- recent BTC realized volatility
- recent BTC drift / momentum
- pre-open paired-book quality
- pre-open top-of-book depth
- early trade-flow intensity
- whether the market is inside the wallet's historically active hours

### Layer 2: Intrawindow Execution

This layer answers:

- once active, how should the engine price and size the two legs?
- when should it push the expensive core?
- when should it add or trim the cheap hedge?
- when should it merge completed pairs?

Signals we want:

- early entry lag from market start
- cheap-leg vs expensive-leg classification
- complement ask/bid geometry
- paired top-of-book spread and gap
- clip cadence and clip-size asymmetry
- hedge/core notional ratio
- pair completion timing
- trade intensity and fill imbalance

### Layer 3: Risk Overlay

This layer answers:

- when should the engine widen or pull?
- when is pair completion too slow?
- when is late-window salvage preferable to further accumulation?

Signals we want:

- time remaining
- merge latency
- stranded-leg imbalance
- spot velocity and short-horizon realized vol
- book thinness / spread blowout
- stale-feed / missing-context flags

## Observable Data Sources

### `wallet_window_reconstructions`

Best source for:

- market start/end
- paired-window identification
- first-trade and last-trade timing
- merge counts
- cheap-leg / expensive-leg labeling
- hedge-cost ratio
- coarse realized PnL fields

This is the base table for the signal pack.

### `wallet_activity_raw`

Best source for:

- clip cadence
- clip notional and price
- trade-side activity
- merge timestamps
- leg-specific trade distribution

This is how we recover the control loop inside a market.

### `wallet_orderbook_snapshots`

Best source for:

- paired top-of-book asks and bids
- complement ask sum and bid sum
- top depth by token
- early-window paired-book quality

This table is sparse but high signal.

### `wallet_market_stream_events`

Best source for:

- stream coverage timing
- book update cadence
- public trade-flow cadence
- spread and top-of-book movement

This gives us richer microstructure than the snapshot table, but with noisier semantics.

### `wallet_btc_price_series`

Best source for:

- pre-open BTC return
- pre-open BTC realized vol
- short-horizon BTC range and trade count

This is useful for offline regime reconstruction.

For live trading, the engine must use a real-time BTC price feed rather than relying on this historical series.

### `wallet_market_catalog`

Best source for:

- market metadata
- token IDs
- asset / market family
- end time
- outcome names where available

This is required to pair token-level book data back into one market window.

## Canonical Per-Window Feature Groups

### 1. Window Metadata

Required fields:

- `window_key`
- `slug`
- `market_id`
- `asset`
- `market_family`
- `start_ts`
- `end_ts`
- `duration_s`
- `paired_outcomes`
- `cheap_leg`
- `expensive_leg`

### 2. Activation Features

Required fields:

- `entry_lag_s`
- `entered_within_30s`
- `entered_within_60s`
- `hour_of_day_utc`
- `day_of_week_utc`
- `time_remaining_after_entry_s`
- `buy_rows`
- `merge_rows`

These are the first fields we use to infer whether the strategy is session-gated.

### 3. Leg Geometry Features

Required fields:

- `cheap_leg_avg_price`
- `expensive_leg_avg_price`
- `price_gap`
- `hedge_cost_ratio`
- `up_avg_buy_price`
- `down_avg_buy_price`

These define the observable target geometry of the strategy.

### 4. Clip / Flow Features

Required fields:

- `trade_count_total`
- `trade_count_cheap`
- `trade_count_expensive`
- `trade_count_first_30s`
- `trade_count_first_60s`
- `notional_total_usd`
- `notional_cheap_usd`
- `notional_expensive_usd`
- `avg_clip_usd_total`
- `avg_clip_usd_cheap`
- `avg_clip_usd_expensive`
- `median_clip_usd_total`
- `cheap_to_expensive_notional_ratio`
- `median_inter_trade_gap_s`

These are the direct execution-control features.

### 5. Merge / Recycle Features

Required fields:

- `first_merge_lag_from_start_s`
- `first_merge_lag_from_entry_s`
- `merge_count_total`
- `merge_count_first_60s_from_entry`
- `merge_count_first_120s_from_entry`
- `merges_present`

This is how we reconstruct pair-completion pressure and capital recycling.

### 6. Book Geometry Features

Required fields where book coverage exists:

- `book_pair_count`
- `book_pair_count_first_60s`
- `book_first_pair_lag_s`
- `book_ask_sum_min_first_60s`
- `book_ask_sum_median_first_60s`
- `book_bid_sum_max_first_60s`
- `book_bid_sum_median_first_60s`
- `book_ask_gap_median_first_60s`
- `book_depth_sum_median_first_60s`
- `book_spread_sum_median_first_60s`

Interpretation:

- `ask_sum` approximates the cost to lift both outcomes immediately
- `bid_sum` approximates the value of immediately selling both outcomes
- `ask_gap` measures cheap-vs-expensive asymmetry at top of book
- `depth_sum` and `spread_sum` help define whether the market is actually tradable

### 7. Stream / Flow Features

Required fields where stream coverage exists:

- `stream_event_count`
- `stream_book_event_count`
- `stream_trade_event_count`
- `stream_first_capture_lag_s`
- `stream_last_capture_lag_s`
- `stream_trade_size_total`
- `stream_trade_size_buy`
- `stream_trade_size_sell`
- `stream_spread_median`
- `stream_spread_min`

These are weaker than private fill/order data, but they are still useful for regime gating and public-flow intensity.

### 8. BTC Spot Features

Required fields where BTC series coverage exists:

- `btc_data_lag_s`
- `btc_prev_1m_return_bps`
- `btc_prev_3m_return_bps`
- `btc_prev_5m_return_bps`
- `btc_prev_15m_return_bps`
- `btc_realized_vol_5m_bps`
- `btc_realized_vol_15m_bps`
- `btc_range_5m_bps`
- `btc_range_15m_bps`
- `btc_trade_count_5m`
- `btc_trade_count_15m`

These are the best offline proxies for the hidden regime gate.

## Coverage Flags

Every exported window must include explicit coverage flags:

- `has_activity_features`
- `has_book_features`
- `has_stream_features`
- `has_btc_features`

The strategy research should never silently treat a missing signal as a negative signal.

## Regime Summaries

The exporter must also produce wallet-level regime summaries, not just per-window rows.

Required summaries:

- hourly participation histogram
- hourly average buy rows
- hourly average merge rows
- hourly average hedge ratio
- coverage by hour for book / stream / BTC signals

Why this matters:

The user-observed regime fact is already meaningful: `unlawful-shear` appears to have very low activity outside core hours. The correct next step is to test that with structured hourly summaries rather than intuition.

## What We Can Infer From This Pack

Once exported, this pack should let us answer:

- does participation cluster in specific UTC hours?
- does the wallet show up only when pre-open BTC vol is above some level?
- does it prefer markets where early paired ask-sum or top-of-book depth crosses a threshold?
- how quickly does it escalate core size after entry?
- how aggressively does it merge once pairable inventory exists?
- how does the hedge ratio drift through the window?

That is enough to build:

- a regime gate
- an execution policy
- a first risk overlay

## What We Still Cannot Observe Directly

This research pack still does not give us:

- exact passive quote placement
- queue position
- exact maker/taker classification for each historical fill
- hidden order cancels and reprices
- per-window rebate attribution

Those are important, but they are not blockers for a paper-grade reconstruction.

The right treatment is:

- reconstruct the observable policy offline
- deploy tiny-live
- calibrate the remaining hidden parameters from our own fills

## Rust Integration Target

The signal pack is not the live control plane.

Its job is to ground the next stage of implementation:

1. derive the candidate gating rules offline
2. turn those into explicit runtime signals
3. feed the live runtime from real-time sources:
   - BTC spot
   - Polymarket order books
   - user fills / inventory / pair ledger

The likely runtime split is:

- offline research pack: JSON generated from historical DB
- live market context: existing `MarketContextStore`
- live signal context: new runtime-facing structure carrying regime and risk signals

## Minimal Output Contract

The machine-readable export should include:

```json
{
  "metadata": {},
  "coverage": {},
  "regime_hourly": [],
  "windows": []
}
```

Where:

- `metadata` identifies the wallet, source DB, and generation time
- `coverage` gives counts of windows with each signal family
- `regime_hourly` contains aggregated hour-of-day summaries
- `windows` contains one object per paired window

## Immediate Next Use

Once the exporter has produced this pack, the next strategy-design task is:

1. rank which activation features most cleanly separate active vs inactive windows
2. define the first explicit regime gate
3. refine the current `unlawful_shear` strategy profile with:
   - activation gating
   - live book-quality gating
   - spot-vol gating
   - late-window cleanup rules

That is the bridge from historical reconstruction to a paper-tradable Rust engine.
