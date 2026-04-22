# W1 Strategy Variants

This document turns the current `w1` research into explicit paper-trial
variants. The goal is not to guess one "perfect" strategy. The goal is to
decompose `w1` into testable components and validate them separately.

## Stable facts from current research

- Recent `w1` sample is overwhelmingly `BTC` `5m`.
- Older recovered slices show `BTC` `15m` and `ETH` `15m` as well.
- Both older and newer BTC samples are:
  - two-sided
  - merge-backed
  - heavily fragmented into child fills
- Execution mix is roughly half passive / half taker.
- Visible `ask_sum < 1` rows are rare. The strategy is not just obvious
  negative-risk scanning.

## Variant 1: Neutral Pair Recycler

### Thesis
Build matched `Up`/`Down` inventory and merge quickly. Minimal directional view.

### Entry rules
- BTC `5m` only.
- Quote or lift both sides only when:
  - pair cost is below configured ceiling, or
  - one side is attractive and the complement can plausibly be completed
    within the same window.

### Execution rules
- fixed child clips
- no large directional imbalance
- merge matched inventory continuously

### What it tests
- Is the pair/merge core profitable on its own?
- How much edge survives without intentional skew?

## Variant 2: Skewed Pair Builder

### Thesis
Same pair-and-merge core, but allow deliberate heavier-side accumulation when
the book is favorable.

### Entry rules
- Same as Variant 1, plus:
  - allow one side to run ahead within explicit imbalance bounds
  - prefer cheaper side accumulation when completion economics remain viable

### Execution rules
- fixed child clips
- controlled skew
- merge matched inventory continuously
- leave residual inventory only when the implied asymmetry is favorable

### What it tests
- This is the closest current hypothesis for `w1`.
- It isolates whether intentional skew is a necessary part of the edge.

## Variant 3: Passive Ladder Builder

### Thesis
The edge is materially execution-driven. Work parent intent through passive
child orders across several nearby levels and only cross when needed.

### Entry rules
- Same pair core as above.
- Build price ladders instead of only taking touch liquidity.

### Execution rules
- fixed share clips, not USD clips
- multiple price levels per side
- passive-first
- taker only for completion or time pressure
- parent-order accounting and child-fill aggregation

### What it tests
- Whether a more `w1`-like execution path materially improves economics over
  the simpler taker-oriented baselines.

## Variant 4: Multi-Horizon Expansion

### Thesis
Older recovered `w1` history may show that the engine first generalized across:
- BTC `15m`
- ETH `15m`

before concentrating on BTC `5m`.

### Entry rules
- same as the chosen core variant
- market family configurable: `5m` vs `15m`

### Execution rules
- same execution logic
- separate limits by asset and horizon

### What it tests
- Whether earlier `w1` behavior reflects a broader strategy template that
  later specialized, or a genuinely different phase.

## Measurement plan

Each variant should be measured on:

- markets traded
- total gross cost
- matched pair share count
- merge share count
- residual share count
- maker/passive share
- taker share
- execution slippage
- realized PnL
- unrealized residual PnL
- PnL per gross dollar

## Build order

1. Neutral Pair Recycler
2. Skewed Pair Builder
3. Passive Ladder Builder
4. Multi-Horizon Expansion

This order matters. We need the clean core before we add execution complexity.

## Historical data notes (2026-03-20 → 2026-04-23 UTC)

- Full backfill for `0xb27...` was pulled into `data/wallet_research/history_8b5b82_historical/`.
- The recovered history file is large (`activity_history.json` ~106MB) and is available locally for analysis.
- Companion summaries are in:
  - `summary.json`
  - `phase_summary.json`
  - `activity_windows_summary.json`
  - `closed_positions.json`

Observed shape from this span:

- 119,000 recovered rows across 370 markets.
- Two-sided + merge flow is high:
  - 334 two-sided markets
  - 341 markets with merge rows
- BTC share increases into dominant phase; 5m BTC is the largest family in late-April windows.

## Runbook: compare all explicit variants

```bash
python3 scripts/whale_pair_compare_variants.py \
  --db backtesting/btc.db \
  --limit 40 \
  --whale-activity data/wallet_research/activity_8b5b82_recent_cap.json \
  --latency-snapshots 1 \
  --fill-fraction 0.7 \
  --output /tmp/w1_variant_compare.json
```

Latest smoke signal from this harness:

- `pair_recycler` had the highest total PnL in the tested sample (still negative),
  and also traded in all but one market.
- `skewed_pair_builder` and `passive_ladder` increased trade frequency but degraded realized+residual edge under current cost/slippage assumptions.
