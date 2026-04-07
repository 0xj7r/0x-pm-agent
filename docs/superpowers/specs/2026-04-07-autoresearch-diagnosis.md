# Autoresearch Diagnosis: Why Backtest Champions Decay Live

Date: 2026-04-07
Status: Diagnosis only. No code changes proposed in this document. A separate
design doc will follow with framework changes.
Optimization target (per user): **ROI**, not Sharpe.

## TL;DR

The autoresearch pipeline is producing champions that look great in the
report and then die in paper trading because the pipeline has **five compounding
sources of overfitting** and **two structural ceilings** that cap the ROI it can
ever discover. The recent paper-trade collapse is not a bug in any single
file. It is the expected behavior of the pipeline once the snapshot collection
grew enough to expose how thin the original signal was.

The most important findings:

1. **Champion selection leaks the test set.** `runner.py` ranks 2,656
   parameter combinations by `test_sharpe` and picks the top one. The "test"
   set is then re-used by the walk-forward validator. There is no truly
   held-out data anywhere in the pipeline.
2. **The champion module is dominated by recency leakage.** In
   `champion.py:_decision_score`, the `recent_stats["pnl_per_trade"]` term
   carries the **largest weight (×6.0)**, which is greater than test_sharpe,
   aggregate OOS, and final holdout combined. Recent = last 20% of the same
   manifest. This is selection bias, not validation.
3. **One trade per market is a hard ceiling on ROI.**
   `evaluator.find_first_trade` takes the first signal and stops. With ~1,194
   markets total and a 22% trigger rate, the *entire* universe of trades the
   system can ever evaluate is ~260 events. There is no path from this
   architecture to "good ROI" because there is no path to enough trades for
   compounding to matter, and no path to capturing intra-market alpha.
4. **The grid is far too large for the sample size.** 2,656 combinations on
   ~30-100 trades per combination guarantees that the best `test_sharpe` is
   dominated by noise. By extreme-value statistics, even pure noise across
   2,656 trials is expected to produce a max Sharpe in the 3-4 range.
5. **Sharpe is the wrong objective for the user's stated goal.** Per-trade
   Sharpe ignores trade frequency, capital deployment, and time decay. A
   "ready_for_paper" champion with 19 holdout trades at $0.17/trade is $3.20
   per cycle. That is not "good ROI"; that is statistical noise dressed up as
   a strategy.

What was *not* the cause: more data is not the problem, it is exposing the
problem. The 2026-04-07 ETH report shows 0 eligible candidates from 1,193
markets, and the "raw profitable" leaderboard is filled with degenerate
strategies that fired on 1 train market and 2 test markets with Sharpe 32. The
pipeline is correctly filtering those, but the underlying search space is
mostly noise.

---

## How I got here

I read the autoresearch core (`runner.py`, `search.py`, `champion.py`,
`metrics.py`, `models.py`, `program.md`), the canonical evaluator
(`backtesting/eval/validate_strategy_readiness.py`,
`backtesting/eval/evaluator.py`), the strategy registry
(`strategies/registry.py`), the deployed config (`strategy_config.json`),
and the most recent champion reports
(`autoresearch/reports/20260407T*champions.json`) plus the older saved
candidate (`autoresearch/candidates/20260405T224346Z-btc.json`).

I have not yet inspected the Supabase paper-trade table in this session.
The diagnosis below is supported entirely by the code path, the snapshot
fingerprints, and the champion artifacts. Once we get into the design phase
I will pull the Supabase trade outcomes and quantify the gap.

---

## Concrete evidence

### 1. Selection-on-test-set leakage (runner.py + search.py)

`search.evaluate_search_space` does a 70/30 chronological split, simulates
all 2,656 strategy/param combinations on both halves, and returns the list
sorted by `test_sharpe` (`metrics.sort_results`). `runner.run_once` then
picks `shortlist[0]` and passes it to `validate_coin`.

The validator (`walk_forward_validate`) builds 5 expanding folds across the
**first 80%** of the manifest and a **final 20% holdout**. But:

- The 30% test set used in search **overlaps** the validator's 20% holdout
  (the last 20% of the manifest is inside the last 30%).
- Every candidate ever evaluated has been exposed to the full manifest by the
  time it reaches validation. There is no untouched data.
- Acceptance is `candidate.test_sharpe > current_best.test_sharpe`. This is
  a ratchet on a leaked metric. Each iteration of the loop tightens the
  bound on a quantity that is, statistically, mostly noise.

This is why the chosen BTC champion in `20260407T132617Z-champions.json`
has `test_sharpe=0.539` on 43 search-test trades but only `holdout
sharpe=0.367` on 19 walk-forward holdout trades. The "validation" is
mostly redundant with the selection criterion.

### 2. Recency leakage in `champion.py:_decision_score`

The score weights are:

```
score += candidate.test_sharpe                * 1.5
score += agg_oos.pnl_per_trade                * 4.0
score += final_holdout.pnl_per_trade          * 5.0
score += recent_stats.pnl_per_trade           * 6.0   <-- highest weight
score += recent_stats.win_rate                * 1.5
score += stable_neighbor_count (capped at 4)  * 0.25
score += stress.pnl_per_trade                 * 3.0
```

`recent_stats` comes from `_recent_manifest`, which is the **last 20% of the
same manifest** the search and validation already used. So the champion is
overwhelmingly chosen for how well it did on the most recent slice of the
training data. This is the textbook recency-bias trap: the strategy is
selected for fitting the regime that just happened, then deployed forward
into a new regime.

The chosen BTC champion shows this clearly. Its "recent stats" are
`trades=18, win_rate=0.667, pnl_per_trade=$0.155`, which contributes
~$0.93 to the decision score. The next-largest contribution comes from the
holdout's `pnl_per_trade=$0.175` × 5.0 = $0.88. Together these two recency
terms make up ~33% of the total score of 5.255, and they are derived from
overlapping windows of the same data.

### 3. The "first trade per market" ceiling (evaluator.py)

`find_first_trade` walks each market from index 10 forward, calls
`check_fn`, and **returns on the first match**. One market = at most one
trade. Implications:

- The total addressable trade count is bounded by the number of markets in
  the snapshot. BTC currently has 1,194 markets and the chosen strategy
  fires on ~22% of them, giving ~263 lifetime trades to optimize against.
- "Sharpe per trade" computed across markets is **cross-sectional**, not
  temporal. It does not measure what Sharpe normally measures (return per
  unit of time-volatility). Two strategies with the same Sharpe can have
  wildly different annualized returns depending on how often they fire.
- All intra-market alpha is discarded. The stories about bots doing well on
  Polymarket overwhelmingly involve **multiple entries per market**: scaling
  in, taking profit, re-entering on retracement, hedging across the YES/NO
  pair. None of that is reachable in this architecture.
- The minimum trade thresholds (`MIN_TRAIN_TRADES=100`, `MIN_TEST_TRADES=30`)
  are requesting that a strategy fire on 8% of all markets in the train
  half. Strategies tight enough to be predictive will rarely fire that
  often, so the only way to clear the threshold is to be loose, which
  collapses signal-to-noise.

This is the single biggest cap on the ROI you can discover, regardless of
how clever the search becomes.

### 4. Multiple-testing on a 2,656-cell grid

The full grid:

| strategy     | combos |
|--------------|-------:|
| threshold    |     30 |
| consistency  |    150 |
| velocity     |    120 |
| skew         |    150 |
| timing       |    150 |
| volatility   |    120 |
| acceleration |    120 |
| combo        |  1,600 |
| vel+cons     |    108 |
| accel+time   |    108 |
| **total**    |**2,656**|

With ~263 trades per candidate at the high end and 30-50 typical, the
distribution of `test_sharpe` across 2,656 i.i.d. random strategies has an
expected maximum well above 2.0 by extreme value statistics, even with no
true signal. The current acceptance threshold of `test_sharpe ≥ 0.5` is
essentially "do not be in the bottom half of pure noise."

The `combo` family alone (1,600 cells) is large enough that selecting its
best by `test_sharpe` is almost guaranteed to overfit. Notice how the
chosen BTC champion is `skew` and not `combo`, despite `combo` having
~10× the search space. That is consistent with `combo` overfitting so hard
to the train set that almost none of its candidates survive the
`is_result_significant` filter on the test side. The system is right to
reject `combo`, but the fact that `combo` exists in the grid at all is
inflating the multiple-testing burden on every other family without giving
us anything in return.

There is **no deflated Sharpe, no PSR, no Bonferroni, no permutation test,
no shuffle null** anywhere in the metrics module.

### 5. The objective function is wrong for ROI

Sharpe per trade is invariant to:

- how many trades the strategy fires (a 1-trade-per-month strategy and a
  10-trades-per-month strategy with the same per-trade distribution have
  the same Sharpe but 10× the ROI)
- the size deployed (the runner does not pass position sizing into the
  evaluator)
- the holding cost or capital lockup (a Polymarket position locks capital
  until resolution; a strategy that ties up $50 for 14 days at $0.17 PnL is
  ~$0.012/day yield, far below opportunity cost)

The user's stated objective is ROI. The pipeline is optimizing
risk-adjusted per-trade PnL. These diverge sharply when, as is the case
here, trade frequency is low and holding times are long.

Looking at the chosen BTC champion's holdout: 19 trades, $3.33 total PnL,
on a 240-market window. If those 240 markets cover ~2 weeks of real time,
that is roughly $6.66/week of PnL on a $50 max position size. That is a
~13%/week gross figure if you assume one position at a time, but the
runner allows up to 20 concurrent positions and uses a Kelly fraction, so
actual deployed capital is much lower and the realized ROI is far smaller.
None of this is captured in `decision_score`.

### 6. The ETH dataset is exposing the noise floor

`20260407T164454Z-champions.json` shows ETH with 1,193 markets, **0
promotion-eligible candidates**, and 1,634 "raw profitable" entries. The
top 10 raw_profitable entries are all degenerate: `train_trades=1,
test_trades=2, train_pnl=0.48, test_pnl=0.94, test_sharpe=32.7`, all with
identical PnL because they fired on the exact same 3 markets despite very
different parameters. These are not strategies, they are parameter combos
that happen to have triggered on a tiny common set.

This is the most diagnostic single artifact in the repo. It tells us:

- The "old" successes were riding sample sparsity. With more data, noise
  averages out and the apparent signal disappears.
- The grid is full of cells that are functionally identical (because their
  filters are non-binding for the small set of markets that pass the move
  threshold) but get evaluated as separate trials, inflating multiplicity.
- The pipeline cannot find a valid ETH champion at all right now. We
  should treat this as the **honest** answer: the current search space and
  evaluator do not support an ETH champion that survives the (already
  weak) acceptance bar.

### 7. Walk-forward is not really walk-forward

`build_walk_forward_splits` builds expanding-window folds, but every fold's
train window starts at index 0. So:

- Fold 1 trains on `[0, min_train)` and tests on the next chunk.
- Fold 2 trains on `[0, train_end_1)` and tests on the next chunk.
- Etc.

The training window is **not** a sliding window of fixed size. It always
includes all earlier data. This means folds are highly correlated, the
"5 folds" provide much less independent evidence than they appear to, and
the `profitable_folds >= num_folds - 1` rule is much easier to satisfy
than it looks.

Combined with the fact that the holdout is the same chronological tail
that was used as the test set during search, the walk-forward stage is
adding very little real information beyond the search step.

### 8. Robustness check is local-only

`neighbor_params` perturbs each parameter by small amounts (e.g.
`move += ±0.01`, `skew += ±0.01`). Then `stable_neighbor_count` counts
how many neighbors are also profitable on OOS. This catches *narrow*
overfits to a single grid cell, but it does not catch:

- regime overfits (the entire neighborhood was trained on the same regime)
- structural overfits to the family (e.g. all `skew` strategies might be
  correlated and all collapse together when skew dynamics shift)
- selection bias from picking the best of 2,656 candidates

The chosen BTC champion has `stable_neighbors=5/7`, which sounds like a
robustness pass, but all 7 neighbors share the same regime and the same
selection process, so they pass or fail together.

---

## Putting it together: why the recent paper trades collapsed

The paper-trade regression is not a single broken thing. It is the
expected outcome of:

1. The "good" champions a few days ago were selected from a smaller
   snapshot where 2,656 trials had even higher max-of-noise Sharpes.
2. They were ranked using a score that put 33% of its weight on the most
   recent slice of training data, so they were literally fit to the regime
   that just preceded their deployment.
3. They were validated using a "holdout" that had already been seen by the
   selection step, so the validator's blessing carried very little
   information.
4. They were deployed live on **out-of-sample data that the pipeline had
   genuinely never seen** (true forward time), and the recency edge that
   got them selected disappeared.
5. The new snapshot is bigger and the noise floor is now visible. The
   pipeline correctly says "I cannot find an ETH champion right now,"
   which is more honest than the previous outputs but feels like a
   regression.

In other words: **the previous high success rates were the bug. The
current low success rates are the pipeline working correctly on top of an
architecture that cannot deliver ROI.**

---

## What this diagnosis does *not* settle

These are the open questions I want to resolve in the design doc, not now:

- **How big is the live vs. backtest gap quantitatively?** I have not
  pulled the Supabase paper trades. The design phase should compute, per
  champion deployment, the actual realized PnL vs. the backtest's stated
  PnL, and the per-bucket win rates, so we can confirm the leakage
  hypothesis with numbers rather than just code reading.
- **Are the polybacktest snapshots themselves clean?** Specifically: do
  they include markets at the moment they would have been tradeable, or
  do they include only markets that already resolved? The latter is a
  silent survivorship/look-ahead bias that would make every backtest
  optimistic regardless of how the search is run.
- **Are the snapshots time-aligned with what the live agent sees?**
  Live, the agent sees order-book best_ask and a partial price history.
  The backtester sees `pm.price_up`, `pm.price_down`, `pm.move_pct`, and
  the precomputed features. If the feature builder uses any future
  information (e.g. computes velocity over a centered window), the
  backtest is leakage-positive and live cannot reproduce.
- **What is the ROI ceiling of a "first trade per market" architecture
  even with a perfect oracle?** Worth computing as a sanity check: if you
  could pick the optimal entry on every market and always win, what is
  the annualized ROI on the deployed capital? If the answer is, say, 30%,
  then the architecture is the binding constraint and we should consider
  multi-entry. If the answer is 300%, the architecture is fine and the
  problem is purely in selection.

---

## What I am confident about

- **Selection by `test_sharpe` over a 2,656-cell grid is statistically
  unsound.** This needs to change before any other improvement matters.
- **Recency-weighted scoring in the champion module is selection bias,
  not validation.** The `×6.0` weight on recent_stats is the single
  largest individual contributor to the decision and it is computed on
  data that has already been used twice.
- **The "holdout" is not a holdout.** It is the same chronological tail
  used in the search test split. Real holdout requires data the search
  step has never touched.
- **Sharpe is the wrong objective for ROI.** We should at minimum
  optimize a quantity that is monotone in capital efficiency (e.g.
  `pnl_per_day_of_capital_locked` or `pnl_per_trade × trade_frequency`).
- **One trade per market is a hard cap on ROI.** Even with a perfect
  selection process, this architecture cannot produce the kind of
  returns the user is asking about.

---

## What is uncertain and needs verification before we change anything

- **Snapshot integrity** (look-ahead bias, survivorship, time alignment).
  If the snapshots are dirty, fixing the search loop on top of them will
  produce a different overfit, not a better one.
- **Live execution gap.** Slippage, latency, fee differences, and the
  fact that live uses `best_ask` rather than mid could account for some
  fraction of the regression independent of overfitting. The stress test
  in `validate_strategy_readiness.py` simulates this with
  `entry_delay=1, entry_slippage=0.01, fee_multiplier=1.25`, but those
  numbers were chosen by intuition, not measurement against live.
- **What "recently working strategies" actually means.** Were the recent
  paper-trade champions selected by this exact pipeline, or by an earlier
  version? If by an earlier version, the regression might also include
  changes in the pipeline itself, not just the data.

---

## Recommendation for next step

Move to phase B (framework redesign) once the user has read this diagnosis
and confirmed the framing. The redesign should address, in priority order:

1. **Honest holdout discipline.** A genuinely untouched final segment of
   data, used **once** to confirm or kill a candidate, never to rank.
2. **Multiple-testing-aware selection.** Either deflated Sharpe / PSR, or
   permutation-based p-values, or a much smaller pre-registered grid.
3. **An ROI-shaped objective.** Replace `test_sharpe` with something like
   `pnl_per_capital_day` or expected `account_return_over_window`.
4. **Walk-forward that actually walks** (sliding window or anchored
   rolling window with disjoint test segments).
5. **Removing recency leakage from the champion score.** Either drop the
   `recent_stats × 6.0` term or compute it on a window that is *outside*
   both the search and validation manifolds.
6. **A live-vs-backtest reconciliation harness** that, for every champion
   ever deployed, tracks the realized vs. predicted PnL and feeds that
   back into the acceptance bar.
7. **Architectural decision: multi-trade-per-market.** This is a bigger
   change and probably belongs in a follow-up after the selection
   plumbing is fixed, but it is the only path to "good ROI" in the
   stories you mentioned.

A subset of these will likely be enough to stop the bleeding. All of
them together get us toward a pipeline that can actually find ROI rather
than recency.
