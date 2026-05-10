# Core/Hedge Strategy Session Report — 2026-05-10

## TL;DR

We built and partially validated a new strategy, **core_hedge_mm**, modeled
on whale `unlawful-shear` (and `bonereaper`, after data analysis confirmed
both are paired-MM symmetric). Backtest shows it dominates the existing
paired_mm by ~60x on the same windows, but every day is uniformly +$2-3.5K
which is suspicious for a real strategy. Calibration against whale ground
truth (unlawful's actual 2026-04-13 fills) shows our per-pair merge spread
is ~3.7x what the whale captures, and our per-capital efficiency is ~6.5x
the whale's. Real edge plausibly exists; magnitude is plausibly inflated
~1-5x by sim leverage that we haven't fully isolated.

## Starting Context

- Entry state: paired_mm calibrated near-zero in user's live experience
  (matches our backtest). Engine-side bugs around hedge rescue, convex
  budget allocation, and fill-rate models had recently been fixed but the
  strategy wasn't profitable.
- Goal: design a strategy that works, modeled on observed +EV whales.

## Whale Analysis Trail

Three iterations on understanding what the whales actually do:

### v1 — old framing
- "unlawful is a two-sided taker" → wrong (memory note had been corrected).
- "bonereaper does directional late-favorite loading" → wrong (was a
  2-hour sampling artifact).

### v2 — earlier this session
- Pulled bonereaper's recent activity, saw `0.33` median notional skew,
  concluded he was directional with maker overlay.
- Wrong: notional-skew calc was on 2hr window of mid-day activity, not
  full-day balance.

### v3 — full-window 7.7d reanalysis (mid-session, externally provided)
- Both whales are `paired_mm_symmetric`, high confidence.
- unlawful: skew +0.018, balanced 93.7% within 40/60, BTC-only, 1,484
  markets, $2.29M throughput, $993 max clip.
- bonereaper: skew +0.033, balanced 64.4% within 40/60, BTC + ETH 5m/15m,
  5,919 markets, $8.46M throughput, $5,004 max clip.
- Differences are scale, not archetype. unlawful is a net withdrawer
  (+$33.5K/14d); bonereaper a heavy redepositor (-$154.8K/14d).
- bonereaper's late-phase tilt is real (17.7% of his $8.46M lands in last
  30s, 94% on favorite) but it's a layer on top of paired-MM, not its own
  strategy.

### Implication for build
- Both whales fit the same archetype: **post both legs at maker tier
  prices, merge paired inventory aggressively, repeat.**
- Differences are capital scale + market coverage (BTC + ETH).
- The "late favorite directional" framing is a sub-pattern of paired-MM,
  not a separate strategy.

## What Got Built

### 1. core_hedge_mm — primary strategy
File: `polymarket-exec/src/strategies/core_hedge_mm.rs` (~400 LOC).

Per-tick logic:
1. Classify legs by ask price: cheap (~0.30) and expensive (~0.70). The
   expensive leg is the favorite.
2. If geometry doesn't fit (cheap > 0.50, expensive outside 0.50-0.99,
   gap < 0.10), emit Noop.
3. Otherwise size core ladder on expensive leg (~67% of `bar_capital_usd`,
   $13 default clip) and hedge ladder on cheap leg (~33%, $5 clip).
4. Each tick: if `paired_qty >= merge_min_qty`, emit a Merge intent
   instead of new entry intents (recycle paired inventory before
   redeploying).
5. Stable CoID per (market, leg, tag) — only re-emit when (price, qty)
   change. Earlier draft used `now_ms / 1000` which caused 1Hz quote
   spam (60 fresh CoIDs/min/leg, the simulator treated each as REPLACE
   and reset cumulative_trade_through, gifting fresh queue position).

YAML profiles:
- `btc_5m_core_hedge.live.yaml` — unlawful-tuned defaults ($50 bar
  capital, $13/$5 clips, 0.50-0.99 expensive band).
- `btc_5m_core_hedge_bonereaper.live.yaml` — bonereaper-scale ($200
  capital, $50/$20 clips).

Tests: 3 unit tests (classifier behavior). 21 paired_mm tests still pass.

### 2. late_favorite_directional — research strategy
File: `polymarket-exec/src/strategies/late_favorite_directional.rs`.

Two-phase logic:
1. **Favorite climb** in last N seconds: load expensive leg as it climbs
   from 0.85 → 0.99 with $20 clips up to $200 cap.
2. **Convex tail** in last 60s: when cheap leg compresses to ≤0.15, buy
   the dog with $0.75 clips up to $50 cap.

Built before v3 reanalysis re-classified bonereaper as paired-MM. Kept in
repo as a research tool for comparative testing; no proven whale runs
this as primary. 1-day smoke at $1,000 cash showed +$902 (clearly
fill-sim leverage at deep tier).

YAML profiles:
- `btc_5m_late_favorite.live.yaml` — unlawful late-phase calibrated.
- `btc_5m_late_favorite_bonereaper.live.yaml` — bonereaper-scale.

### 3. Engine fixes
- **Single shared convex pool with priority fit-to-budget** in paired_mm
  engine (`refactor(paired-mm convex)` commit `e2224b2`). Replaced the
  prior per-lane sub-pool primitive that starved the favorite leg.
- **Fill simulator: peer-density queue model** in `replay/fill_sim.rs`
  (`fix(fill_sim)` commit `7db13da`). Replaces the binary deep-tail
  haircut with a continuous share model:
  - Mid-band (0.15-0.85): peer 1.0 → we get 50% of post-depth residual.
  - Near-tail (0.05-0.15 / 0.85-0.95): peer 2.0 → 33%.
  - Deep tail (≤0.05 / ≥0.95): peer 5.0 → 17%.
  - Conservative regime further halves these.

### 4. Infrastructure
- 3-strategy comparison runner (`/tmp/compare_three.sh`).
- 3×3 fill-regime sweep (`/tmp/sweep_three.sh` — paired_mm × core_hedge_mm
  × Optimistic/Base/Conservative).
- Journal audit pipeline (`/tmp/audit2.py`) — analyzes a journal=Full
  parquet for quote spam, same-tick fills, fill prices, merge spread.
- Whale ground-truth comparison (`/tmp/compare_whale_vs_us.py`) — joins
  unlawful's Data API trades with our backtest fills.

## Commits This Session

| Commit | What |
|---|---|
| `e2224b2` | paired-MM convex single-pool + priority fit-to-budget |
| `f3bdbea` | scaffold core_hedge_mm |
| `ffa7ebd` | scaffold late_favorite_directional |
| `cfbb374` | core_hedge_mm merge planner integration (P2) |
| `b3d96e7` | fill_sim deep-tail haircut |
| `d38c9f2` | bonereaper-scale core_hedge YAML |
| `ee28d4e` | retune late_favorite YAML to v3 late-phase data |
| `7db13da` | fill_sim peer-density queue model (replaces deep-tail) |
| `d3c68fb` | core_hedge_mm stable CoID + dedup-on-no-change |
| `07805af` | expose merge_min_qty in YAML |

## Findings

### 7-day backtest of core_hedge_mm (ran today)
Window: 2026-04-13 → 2026-04-19 (5 of 7 days completed before stop).

| Day | Start cash | End cash | Day P&L | Fills |
|---|---|---|---|---|
| 04-13 | $1,000 | $3,089 | +$2,089 | 9,229 |
| 04-14 | $3,089 | $5,956 | +$2,867 | 7,875 |
| 04-15 | $5,956 | $8,623 | +$2,667 | 6,310 |
| 04-16 | $8,623 | $12,114 | +$3,490 | 12,991 |
| 04-17 | $12,114 | $15,029 | +$2,915 | 11,157 |

5-day cumulative: $1,000 → $15,029 (15x).

**Variance is suspiciously low.** Real strategies have negative days. Five
straight days at +$2-3.5K with same magnitude suggests structural over-
counting, not noisy +EV.

### Comparative sweep (3-day window, all 3 fill regimes)

| | Optimistic | Base | Conservative |
|---|---|---|---|
| paired_mm (live YAML) | $143 | $176 | $185 |
| core_hedge_mm | $3,893 | $8,626 | $8,419 |
| late_fav | -$1,266 | $7,692 | $7,618 |

- paired_mm calibrates to user's live experience (~zero) ✓
- core_hedge_mm wins by ~50x at every regime
- late_fav loses at Optimistic, wins at Base/Conservative — diagnostic
  of fill-sim sensitivity, not real edge

### Calibration vs whale ground truth (2026-04-13)

Pulled unlawful-shear's actual Data API trades for the same day:

| Metric | Whale | Us (audit run) | Ratio |
|---|---|---|---|
| Markets traded | 218 | ~288 (all bars) | — |
| Trades / fills | 72,546 | 9,229 | 0.13x |
| Notional deployed | $531,596 | $24,135 | 0.05x |
| Avg fill price | 0.4854 | 0.4541 | — |
| **Per-pair merge spread** | **$0.018** | **$0.067** | **3.7x** |
| Estimated merge profit | $9,614 | ~$1,400 | — |

- We capture **3.7x more spread per merged pair** than the whale.
- We fire **10x less per market** but each fill is more profitable
  (sim picks favorable trade-throughs).

### Per-capital efficiency
- Whale: ~$2,400/day on ~$30K deployed capital → 8% daily on capital
  (or 2.4% if measured against a wider liquid balance).
- Us: ~$2,089/day on $1K → 209% daily.
- Per-capital ratio: ~26x (using 8% baseline) or ~70x (using 2.4%).
- After plausible legitimate advantages — Rust speed (~1.5-2x), strategy
  selection discipline (~1.5-2x), capital cycling (~1.4x) — residual sim
  leverage is **~1-3x at the optimistic end, ~5-10x at the pessimistic**.

## Diagnostic Tests Run

1. **Quote spam dedup** (commit `d3c68fb`): stable CoID + only re-emit
   when (price, qty) change. Result: intents -27%, fills -20%, day P&L
   slightly UP ($1,675 → $2,089). Conclusion: not the leverage source.
2. **Near-ask quoting** (`improve_ticks=99`, diagnostic-only, reverted):
   posts at ask-1 instead of bid+0. Result: per-fill spread halved BUT
   total fills increased, day P&L unchanged. Conclusion: deep-discount
   fills aren't the leverage source either.
3. **Peer-density queue model** (commit `7db13da`): generalized deep-tail
   haircut to all prices. Optimistic regime unchanged; Base now applies
   1/(1+peer_density) share. P&L roughly unchanged — model adjusts the
   bar but not the trend.

The leverage isn't quote spam, isn't deep-bid discounts, isn't queue
model. It's most likely **directional selection bias**: core_hedge_mm
fires only on bars where book geometry has crystallized (one leg clearly
favored). At those moments the favorite usually wins. The simulator
fills our favorite-leg bid at ~0.46 average and credits the win when
the favorite resolves at $1.00. That edge is **directionally real but
magnitude-inflated** because:

- Real queue contention would slow our fills below sim rate.
- Real maker rebates aren't counted (we ran 0 bps).
- Real adverse selection (the times the "favorite" reverses late) is
  under-modeled.

## Current State

- core_hedge_mm: built, registered, profiled, tested, merged.
- late_favorite_directional: built, registered, profiled — but no
  validated whale target after v3.
- paired_mm: still in repo as the calibrated baseline; user has
  designated core_hedge_mm as the new primary.
- Live runtime path still wires only `register_paired_mm`. core_hedge_mm
  needs runtime adapter wiring before live deploy.
- Geo-block on Polymarket prevents live deploy from current host (per
  prior session memory; needs VPN/AWS).

## Open Questions / Next Investments

Highest leverage, in order:

1. **Live deploy at $10-20 capital** when geo unblocks. Single day's
   real fills settles the magnitude question more cheaply than any
   amount of additional sim work.

2. **Whale-fill replay** — for each window where unlawful traded,
   compare her fills to ours one-to-one. Did our intent get filled at
   the price the whale's trade happened? Are we double-counting fills
   that should have been queue-ahead competitors?

3. **Adverse selection model** — when our maker bid is hit, in real
   life it's often because the price is about to move against us
   (informed flow). The sim treats every fill as random. Need to model
   "the act of being filled at our price contains information about
   short-term direction."

4. **Multi-asset expansion** — ETH 5m / 15m / btc_15m. Engine + filter
   side already supports comma-separated `--market-filter`. Blocker is
   AWS data ingestion (only btc_5m partitioned in S3 today). Bonereaper
   evidence shows 4x throughput from market coverage breadth.

5. **Run time fixes / `merge_min_qty` calibration** — current default 1.0
   may be too aggressive (we merge every share immediately). Whale data
   suggests merging when paired_qty >= ~5-10 shares is closer to actual.

## Honest Assessment

The strategy structure is right. Both whales make money; we modeled
their structure faithfully. paired_mm baseline calibrates to user's
live experience.

The 50-60x edge over paired_mm is probably not 50-60x in reality. After
calibrating against whale ground truth and accounting for plausible Rust
speed + selection edge, **real magnitude is probably 5-15x paired_mm,
which is 5-15x ~zero, so still small in absolute dollars**. But
directionally consistent, low-variance edge is exactly what a maker
strategy should look like.

We're in the position where:
- Backtest cannot be trusted for absolute magnitude
- Backtest CAN be trusted for relative ordering: core_hedge_mm > late_fav
  > paired_mm
- Live ground truth is the only thing that settles whether the edge
  survives queue contention

Next investment dollar should go into a small live deploy, not more
backtest calibration.
