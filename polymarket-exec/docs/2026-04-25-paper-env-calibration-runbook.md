# Paper Environment Calibration Runbook

_Created: 2026-04-25_
_Source design: `docs/architecture/2026-04-25-paper-env-design.md`_

The paper environment ships with conservative defaults from MM literature
(submit latency 150ms, queue depth 0.75, post-only reject 0.85, cancel
race 500ms). They are **not calibrated against actual Polymarket fill
data**. This runbook is the loop for closing that gap.

## When to run this loop

- Before scaling capital past tiny-live.
- After any change to a paper-env knob.
- When `vs_whale.notional_capture_ratio` drifts more than 2x in either
  direction across consecutive shadow_live sessions.
- When realized edge tracks expected edge poorly (< 0.5 capture ratio).

## The loop

```
shadow_live session → paper_report.json → suggest_paper_calibration.py
                                                ↓
                            replay with new knobs → variant_report.json
                                                ↓
                            compare_replay.py baseline.json variant.json
                                                ↓
                            commit knob change OR iterate
```

## Step 1: Capture a baseline

Start with the shadow-live env preset and let the engine run against
real BTC 5m markets for a session window long enough to generate
meaningful fill counts. **Minimum 3-4 hours** for the unlawful_shear
strategy on BTC 5m markets at typical volume.

```bash
set -a
source polymarket-exec/env/btc_5m_mm_shadowlive.env
export WHALE_PAIR_BOOK_SNAPSHOT_LOG_PATH=data/calibration/session1.snap.jsonl
export WHALE_PAIR_PAPER_REPORT_PATH=data/calibration/session1.report.json
set +a

cargo run -p polymarket-exec
```

When you stop the run (Ctrl+C), the engine flushes both the snapshot
log and the paper report. The report's `vs_whale` section is auto-
populated from `dashboard_whale_events_path` if that's set in your env.

## Step 2: Read what happened

```bash
python3 polymarket-exec/scripts/suggest_paper_calibration.py \
    data/calibration/session1.report.json
```

The output gives `observed` metrics + `suggested` knob changes with
rationale. Read the rationale before applying — heuristics are
directional, not optimal.

Key signals to look for:

| Signal | Likely cause | Knob to adjust |
|---|---|---|
| `total_fills == 0` | Strategy never submitted, OR queue depth too pessimistic | `paper_queue_depth_fraction` ↓ |
| `maker_fraction < 0.4` | Post-only rejects too aggressive; strategy fell back to crossing | `paper_post_only_reject_probability` ↓ |
| `edge_capture_ratio < 0.5` | Book moved between submit and fill — latency too high | `paper_submit_latency_ms` ↓ |
| `avg_slippage_bps > 25` | Paper letting orders cross too easily | `paper_post_only_reject_probability` ↑ |
| `vs_whale.notional_capture_ratio < 0.05` | Underfilling vs whale on same window | `paper_queue_depth_fraction` ↓ |
| `vs_whale.notional_capture_ratio > 3.0` | Overfilling vs whale (paper too generous) | `paper_queue_depth_fraction` ↑ |

## Step 3: A/B against the same recorded book

Once you've picked a knob change, you do NOT need to wait for another
live session. Replay the recorded snapshot log with the new knob and
compare.

```bash
# Variant: lower queue depth assumption
WHALE_PAIR_EXEC_MODE=replay \
    WHALE_PAIR_REPLAY_INPUT_PATH=data/calibration/session1.snap.jsonl \
    WHALE_PAIR_PAPER_QUEUE_DEPTH_FRACTION=0.50 \
    WHALE_PAIR_PAPER_REPORT_PATH=data/calibration/session1.qd50.json \
    cargo run -p polymarket-exec

# Diff
polymarket-exec/scripts/compare_replay.py \
    data/calibration/session1.report.json \
    data/calibration/session1.qd50.json
```

The diff table shows side-by-side metrics with percent deltas. A "good"
knob change should:

- Move `vs_whale.notional_capture_ratio` toward 1.0
- Improve `edge_capture_ratio` (toward 1.0)
- Not collapse `maker_fraction` (a maker-first strategy should stay > 0.7)
- Not blow up `avg_slippage_bps`

## Step 4: Iterate

Replay is fast (no waiting on live data). Try 2-3 variants per knob.
When a knob value moves the metrics in the right direction without
breaking other metrics, that's your candidate.

## Step 5: Validate the candidate against a fresh session

Knob calibrated against ONE recorded session can overfit to that
session's regime. Run a fresh shadow_live session with the new knob
value and check that the metrics still look right.

```bash
WHALE_PAIR_PAPER_QUEUE_DEPTH_FRACTION=0.50 cargo run -p polymarket-exec
```

If vs-whale and edge metrics hold, commit the new value to your env
preset. If not, the knob change overfitted; iterate further on the
recorded log first.

## Step 6: Commit

Update the env preset (`polymarket-exec/env/btc_5m_mm_shadowlive.env`)
with the new knob value. Commit the change with a one-line note about
the validation evidence (which session, what metric improved).

## What to do when calibration disagrees with intuition

A knob change that improves capture ratio but tanks edge capture is
NOT a good change — the strategy is filling more but at worse prices.
Conversely, if `vs_whale.notional_capture_ratio` is 0.5 and edge
capture is 1.0, the strategy is being correctly conservative versus
the whale. Don't chase the whale's volume if your fills are tracking
their edge.

## Limits of this loop

The four conservative knobs model fill probability + reject rate +
latency + cancel race. They do NOT model:

- Fee model accuracy beyond the maker rebate / taker fee split (Phase 9)
- Strategy decision quality (that's strategy.rs's job)
- Counterparty composition (who else is on the book and their behavior)
- Network regime shifts (what worked on calm days breaks on volatile days)

When a session report shows persistently bad metrics that no knob
change improves, the gap is in one of those areas, not in the paper
fill model.

## Operational baseline

Aim for:

- `vs_whale.notional_capture_ratio`: 0.1 - 1.5 (we're not the whale; we're a fraction of their flow)
- `edge_capture_ratio`: 0.7 - 1.0 (paper realized edge tracks expected edge)
- `maker_fraction`: 0.7+ for unlawful_shear (it's a maker-first strategy)
- `avg_slippage_bps`: < 10 for maker-first strategies

If you can hit these consistently across 3+ shadow_live sessions, the
paper env is calibrated well enough to gate live capital scaling.
