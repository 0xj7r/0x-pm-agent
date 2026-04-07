# Autoresearch Framework Redesign — Design Spec

Date: 2026-04-07
Status: Draft for review
Sibling docs: `2026-04-07-autoresearch-diagnosis.md` (the failure analysis
this design responds to)

## 0. Read this first

This spec replaces the existing autoresearch pipeline (selection on leaked
test Sharpe over a 2,656-cell grid, expanding-window walk-forward whose
holdout overlaps the search test set, recency-weighted champion score,
one-trade-per-market evaluator, midpoint entry prices) with a probability-
forecaster pipeline that gates on tradable edge after costs, sizes with
fractional Kelly on a conservative q estimate, validates with combinatorial
purged cross-validation, and is operated by three structurally-separated
agents that cannot leak the holdout to one another.

The spec is opinionated. Each section says what we are doing and why we
are not doing the obvious alternative. It is the result of a diagnosis,
a reframe, a Codex review that pushed back on parts of the reframe, and
the discovery that the local snapshot data was missing PolyBackTest's
orderbook depth (fixed by passing `include_orderbook=true`, commits
`b96c044` and `dc864c7`).

The optimization target is **realised return on deployed capital after
fees and slippage**. Not Sharpe. Not classification accuracy. Not raw
calibration. Tradable edge.

## 1. Mission and non-goals

### 1.1 Mission

Build a research framework that, given a stream of resolved Polymarket
5-minute up/down markets on BTC and ETH, can identify probability
forecasters whose edge after costs is statistically distinguishable from
zero on truly out-of-sample data, size them with fractional Kelly on
shrunk q estimates, paper-trade them, and reconcile their live behaviour
against their backtest predictions in a way that surfaces calibration
drift before it becomes a PnL drift big enough to ruin us.

### 1.2 What this framework explicitly does not do

- It does not optimize Sharpe, win rate, or any classification metric in
  isolation. Sharpe is invariant to trade frequency and capital lockup
  and assumes Gaussian returns; binary contracts violate both.
- It does not let the agent tune the Kelly fraction against
  out-of-sample log growth. The Kelly fraction is policy.
- It does not promote candidates based on aggregate calibration alone. A
  forecaster perfectly calibrated to P(Up wins) can have zero monetisable
  edge because the market price embeds the same information.
- It does not auto-promote candidates to live trading. Promotion requires
  a human in the loop.
- It does not extend to multi-trade-per-market in v1. The architecture is
  designed to allow it later, but v1 preserves the existing single-entry
  evaluator so the new framework's results are directly comparable to
  the old pipeline's on the same dataset.
- It does not unify BTC and ETH into a single model. Each coin gets its
  own forecaster and its own holdout. Cross-coin transfer is a separate
  validation step, not a training assumption.

## 2. Foundational facts about the data and market

These are the facts the design has to fit. Everything is constrained by
them.

### 2.1 The instrument

Polymarket 5-minute up/down markets are binary contracts that pay $1 if
a coin is up at the end of a 5-minute window and $0 otherwise. Contracts
trade as a YES/NO pair where YES + NO ≈ 1 by construction (with a small
spread). New markets open every 5 minutes around the clock; ~290 BTC
markets and ~290 ETH markets per day.

### 2.2 The data we have after the orderbook fix

Source: PolyBackTest's `/v2/markets/{id}/snapshots` endpoint with
`include_orderbook=true`. The previous fetcher omitted this query
parameter and silently received midpoint-only responses; this was the
single largest source of overoptimism in the historical pipeline.

Per snapshot we now persist (in `backtesting/{coin}.db`):

- `time` — sub-second precision timestamp
- `{coin}_price` — underlying coin price at the snapshot moment
- `price_up`, `price_down` — token mid prices (legacy, kept for back-
  compatibility but not used by the honest simulator)
- `best_bid_up`, `best_ask_up`, `bid_size_up`, `ask_size_up` — top of
  book on the YES token
- `best_bid_down`, `best_ask_down`, `bid_size_down`, `ask_size_down` —
  top of book on the NO token
- `orderbook_up_json`, `orderbook_down_json` — full 15-level depth
  serialised as JSON, used only by the slippage model when sized
  simulation goes beyond top of book

A live recorder (`collector/snapshot_recorder.py`) writes to the same
schema with bid/ask from the Polymarket WebSocket. Recorder rows have
NULL in the `orderbook_*_json` columns because the WS feed gives only
top of book, not depth. The framework must treat these rows as
top-of-book-only when the slippage model is engaged.

### 2.3 Coverage and biases the framework must respect

Per the in-flight backfill audit (`scripts/verify_orderbook_coverage.py`):

- ETH at ~60 markets pulled: **92.7% of snapshot rows have populated
  orderbook**. The remaining 7.3% are PolyBackTest collection gaps —
  rows where the API returned no `orderbook_up`/`orderbook_down`. The
  simulator MUST skip these rows, not fall back to midpoint.
- Bid-ask spread on the up token (n=122,161): median 1¢, mean 1.3¢,
  p99 6¢, max 39¢. Median 1¢ on a token at ~50¢ is a **2% round-trip
  cost** before fees. Any strategy that does not produce >2% edge per
  trade after costs is dead by construction.
- The market metadata table covers 32 days (2026-03-07 to 2026-04-07),
  but only ~5 days of snapshots existed locally before the backfill.
  After the in-flight backfill, we expect ~31 days of snapshots per coin
  with ~9,000 markets per coin.
- The fetcher's `move_threshold` was previously 0.05% (only fetch
  snapshots for markets where the underlying already moved ≥0.05%) —
  fixed in commit `d738382` to 0. Older runs of the fetcher may have
  produced data with this bias; rows produced before that commit must
  be assumed contaminated.
- Rows produced before the orderbook fix (commits `b96c044` and
  `dc864c7`) have NULL in the new columns. The clean-slate clear we
  did during the fix removed them from the local dbs.

### 2.4 The economic constraints these facts imply

A profitable strategy on this instrument has to clear:

1. The bid-ask spread (median 2% round-trip).
2. Polymarket fees (~2% taker fee depending on entry price).
3. Slippage at the size you actually want to trade (size > top of book
   pushes you further down the book).
4. Any model uncertainty haircut.

The combined hurdle for a single round-trip on a $0.50 token is
**roughly 4-5% per trade** before any edge appears. A forecaster needs
to be confident in q within ~5% of the truth before its sizing makes any
money at all. This is the binding constraint and the design has to
acknowledge it loudly.

## 3. Conceptual reframe: forecaster, not signal

### 3.1 The old model

A `check_fn(pm, i)` returns one of `"Up"`, `"Down"`, `"SKIP"`, `None`.
If it returns a direction, the system enters at the midpoint price for
that side and waits for resolution. The "strategy" is a binary filter
plus an entry-side oracle. There is no probability anywhere in the
system; no concept of edge; no concept of sizing beyond a fixed
`max_position_usd`. The `kelly_multiplier=0.25` constant in
`strategy_config.json` is vestigial because there is no `q` to plug in.

### 3.2 The new model

A `forecaster_fn(market_state, i) -> q ∈ [0,1]` returns the model's
estimated probability that the YES token resolves to $1 at index `i`.
Everything downstream is a function of `q`, the live ask `p_ask`, the
fees, and a sizing policy:

```
edge_yes = q - p_ask_yes
edge_no  = (1 - q) - p_ask_no
```

A forecaster is the *only* unit of strategy in the new system. Filter
functions like the existing `threshold`, `consistency`, `velocity`,
`skew`, etc., are not strategies in the new system: they become
*features* fed to a forecaster.

### 3.3 Why the reframe

Three reasons, in priority order:

1. **It maps to the actual decision.** The decision is a price
   comparison: my probability vs the market's price. A signal
   generator hides this comparison behind a Boolean and loses the
   information about how much edge any given trade has.
2. **It enables sizing.** Without `q`, Kelly is undefined. With `q`,
   Kelly tells you how much to bet. The size of the bet is the largest
   driver of realised return after edge itself.
3. **It enables calibration as a quality check.** A forecaster can be
   evaluated for whether its 70%-confident predictions actually resolve
   YES 70% of the time. A signal generator cannot. Calibration is the
   meta-check that catches overconfidence before sizing destroys you.

### 3.4 Where the reframe was wrong on first pass

Codex (and other quant feedback) caught several places where the
reframe sounded right but would have introduced new failures:

- **"Calibration is the foundation gate"** was wrong as stated. A model
  perfectly calibrated to P(Up wins) can have zero tradable edge,
  because the market price already embeds that probability with less
  noise. The right gate is **edge after costs vs market-implied
  baseline**, with calibration as a quality check on the q used in the
  edge computation. Murphy decomposition is a diagnostic, not a
  promotion gate.
- **"Quarter-Kelly on raw q"** was too aggressive. Small errors in q
  near the decision boundary blow up sizing. The fix is to **shrink
  q toward 0.5 before sizing**, then apply the policy fraction. The
  shrinkage is a function of estimation error, not OOS log growth, so
  it is policy not tuning.
- **"Calibration drift kill at 30-50 trades"** was statistically
  unsound. With 5 buckets and 50 trades, SE on bucket win rate is
  ~16 percentage points. Any kill criterion at this sample size has a
  high false positive rate. The kill becomes a **review trigger**
  requiring ≥100 live trades total, ≥25 in the active edge region,
  with sequential significance testing.
- **"Seven agents from day one"** was control theater. The economics
  have to be honest before the governance matters. v1 is three
  agents.

## 4. Objective function

### 4.1 The thing we maximise

The objective is **expected log growth of bankroll** under fixed
quarter-Kelly sizing on a shrunk q estimate, computed by forward
simulation across the OOS partition with `best_ask` as the entry price
and a documented fee model. Concretely:

```
log_growth_per_trade(forecaster, partition):
    bankroll = 1.0
    log_returns = []
    for market in partition:
        for i in trading_window(market):
            q_hat = forecaster(market, i)
            q = shrink(q_hat, alpha=SHRINK_ALPHA)

            ask_yes = market.best_ask_up[i]
            ask_no  = market.best_ask_down[i]
            if isnan(ask_yes) or isnan(ask_no):
                continue   # PolyBackTest had no book at this index

            # Try YES
            p_break_even_yes = ask_yes + fee(ask_yes)
            edge_yes = q - p_break_even_yes
            if edge_yes > MIN_EDGE:
                f_kelly = (q - ask_yes) / (1 - ask_yes)
                f = max(0.0, KELLY_FRACTION * f_kelly)
                f = min(f, MAX_BET_FRACTION)
                stake = f * bankroll
                fill = simulate_fill(stake, market.orderbook_up[i])
                won = (market.winner == "Up")
                pnl = (1 - fill.avg_price) * fill.shares if won \
                      else -fill.avg_price * fill.shares
                pnl -= fill.fees
                ret = pnl / bankroll
                bankroll *= (1 + ret)
                log_returns.append(log(1 + ret))
                break  # one trade per market in v1

            # Try NO (symmetric)
            ...

    return {
        "log_growth_per_trade": mean(log_returns),
        "log_growth_total": log(bankroll_final / bankroll_initial),
        "n_trades": len(log_returns),
        ...
    }
```

The reported scalar that gets ranked and the reported scalar that gets
gated are both `log_growth_per_trade` with a stationary block bootstrap
95% CI. **Lower CI must exceed zero to pass any stage.**

### 4.2 The shrinkage function

```
shrink(q_hat, alpha):
    return 0.5 + alpha * (q_hat - 0.5)
```

`alpha` is a policy parameter, not a hyperparameter. It is set based on
the estimation error of `q_hat` from the validation step (out-of-sample
calibration error on CV). Default policy: if the model's CV calibration
shows |q_hat - q_true| ~ 0.05 in the bins where it places bets, set
`alpha = 0.5`. Tighter calibration → larger `alpha`. The relationship
is documented in the preregistration; the agent does not get to pick
`alpha` after seeing its OOS log growth.

A more sophisticated alternative is to compute a posterior on q given
the calibration uncertainty and use a lower confidence bound (e.g. the
5th percentile of the posterior). v1 uses the simple shrinkage. v2 may
upgrade to the posterior version.

### 4.3 The fee model

`shared/fees.py:taker_fee` already exists in the repo. The simulator
must use the live fee schedule, not a constant percentage. If the fee
schedule changes during the backtest period, the simulator uses the
fee at the historical timestamp, not today's fee.

### 4.4 The MIN_EDGE constraint

Even if `q > p_break_even`, we do not enter unless the edge buffer is
larger than a minimum threshold tied to model uncertainty:

```
MIN_EDGE = max(SPREAD_AT_ENTRY * 0.5, MODEL_UNCERTAINTY * 2)
```

Rationale: the spread itself eats half its width on average; model
uncertainty (standard error of the q estimate) needs at least 2 sigma
of edge to dominate. The actual constants are declared in the
preregistration.

### 4.5 The KELLY_FRACTION policy

Fixed at 0.25 (quarter-Kelly). Diagnostic-only simulations may report
log growth at 0.10 and 0.50 alongside, but the headline metric and the
gating decision use 0.25. A forecaster with edge so thin that quarter-
Kelly is not enough to make money is a forecaster we should not be
deploying.

### 4.6 The MAX_BET_FRACTION cap

Hard cap at 0.05 of bankroll on any single trade regardless of what
Kelly says. Prevents a single overconfident `q_hat` from blowing up the
account before the calibration check catches it. 5% is a conservative
upper bound; tighter is fine.

### 4.7 The market-implied baseline (mandatory)

Every research run must include a baseline forecaster `q_market(p_ask)`
that just outputs the inverse of the live ask after fees. This is the
"the market is right" hypothesis. **Any candidate forecaster that does
not beat this baseline on log growth (with bootstrap lower CI > 0
above the baseline) is rejected.** This single check kills the most
common failure mode: producing a forecaster that captures variation
the market also captures, then pretending the variation is alpha.

## 5. Validation methodology

### 5.1 Partitions

The manifest of resolved markets (ordered by `start_time` ascending) is
split into three contiguous partitions:

```
TRAIN  : first 60%
CV     : next 20%
HOLDOUT: last 20%
```

The split function lives in `autoresearch/partitions.py` and its commit
hash is recorded in every preregistration. A change to the split
function invalidates every prior preregistration that referenced it.

### 5.2 Combinatorial Purged Cross-Validation on TRAIN+CV

López de Prado's CPCV (rather than expanding-window walk-forward)
because:

- It produces multiple OOS test sets that share no overlap, so the
  OOS distribution of log growth is constructed from genuinely
  independent samples.
- It supports purging and embargoes around fold boundaries to prevent
  label leakage from markets whose price history overlaps a fold edge.
  For 5-min markets, the embargo is 1 hour: any market starting within
  1 hour after a fold's training end is excluded from the test fold.
- It allows direct computation of the Probability of Backtest
  Overfitting (PBO) statistic across the candidate grid.

CPCV configuration: `N_PARTITIONS = 10`, `K_TEST = 2`, embargo `= 12
markets ≈ 1 hour`. This produces `C(10, 2) = 45` distinct train/test
splits per candidate.

### 5.3 Block bootstrap with ACF-derived block length

Per-trade log returns are not iid: consecutive trades on the same
underlying have correlated outcomes (BTC was either rising or falling
during the window in which both happened). Naive iid bootstrap CIs are
too tight.

We use the **stationary block bootstrap** (Politis-Romano) with mean
block length determined by the autocorrelation function of the
per-trade log return series:

```
block_length(returns):
    acf = autocorr(returns, lags=range(1, 50))
    k = first_lag_below(acf, 1.0 / sqrt(len(returns)))
    return max(2, 2 * k)
```

The block length is computed and committed to the preregistration
**before** the validation run, derived from the TRAIN partition's
returns under the baseline forecaster (so the block length itself is
not chosen post-hoc).

### 5.4 The calibration audit (diagnostic, not gate)

For every candidate that passes the edge gate:

1. Compute `brier_score(q_hat, outcomes)` on CV.
2. Compute the Murphy decomposition: `brier = uncertainty - resolution
   + calibration`.
3. Build a reliability diagram with 5-10 quantile-based buckets
   (equal-population, not equal-width, so each bucket has enough
   sample size to be readable).
4. Apply isotonic recalibration on TRAIN, then re-evaluate on CV.
5. Report whether the recalibrated forecaster has materially better
   `calibration` component than the raw forecaster.

The calibration audit produces a diagnostic report and a single binary:
`calibrated_or_not`. If `not calibrated_or_not`, the forecaster is
marked "uncalibrated but kept for review" and the Red Team's job is to
investigate whether the miscalibration is concentrated in the bins
where the model places bets. **A forecaster with an uncalibrated tail
can still be tradable if its bets only land in the calibrated middle.**

### 5.5 Slice-conditional calibration

Aggregate calibration can hide regime-conditional miscalibration. The
calibration audit must compute reliability diagrams for each of these
slices independently:

- Time of day: UTC hour buckets (4 buckets: 0-6, 6-12, 12-18, 18-24)
- Day of week
- Underlying volatility regime: 24h-rolling realised vol, 3 buckets
  (low, mid, high) defined on the TRAIN distribution
- Market thinness: `final_volume` percentile, 3 buckets
- Move magnitude bucket: `abs_move_pct` at entry, 3 buckets

A forecaster that is calibrated in aggregate but materially
miscalibrated in any single slice is downgraded for Red Team review.
This is the slot where regime risk gets caught.

### 5.6 Probability of Backtest Overfitting (PBO)

For every search run, compute PBO across the candidate grid using
combinatorially symmetric cross-validation. Report the PBO statistic
in the search artifact. **A grid with PBO > 0.5 is rejected entirely**
— the search is more likely than not to pick a winner that fails OOS,
which makes any single winner from that search untrustworthy. The
preregistration must declare a maximum acceptable PBO; if the search
exceeds it, the run is logged as "PBO failed" and no candidates are
promoted, no holdout is touched.

### 5.7 The holdout discipline

The holdout is the last 20% of the manifest, contiguous in time. It
exists to answer one question per candidate: "does this candidate's
edge survive on data the search and validation steps have never
seen?"

Rules:

- The holdout is **read-once per candidate**. The candidate's hash
  (which includes its parameters AND the dataset fingerprint AND the
  preregistration commit hash) must not appear in the audit log as
  having been holdout-tested before.
- The holdout is read-only via the Holdout Burner agent (Section 7).
- The Holdout Burner refuses to run on candidates whose hash appears
  in the audit log.
- The framework imposes a **holdout budget**: at most 5 holdout
  evaluations per coin per calendar week, regardless of how the
  candidates differ. The reason is that even one-shot per candidate is
  multiple-testing if you test 100 candidates against the holdout —
  the family-wise error rate explodes. A 5-eval budget is a Bonferroni-
  ish hard cap.
- If the holdout budget is exhausted, no further holdout work happens
  this week, full stop.
- Failing the holdout is a successful research outcome. The candidate
  is killed, the audit log records the failure, and the agent moves on
  to the next preregistered question. Loosening the holdout criterion
  to "rescue" a failed candidate is the textbook p-hacking we are
  avoiding.

## 6. Data prerequisites and the data audit gate

Before any search, validation, or holdout run can happen, the Data
Steward agent must run the data audit and the audit must return
PASS. The audit checks:

1. **Coverage**: no gap >1 hour in the snapshot timeline within the
   active backfill window. Gaps >1 hour become date ranges that the
   simulator must explicitly skip.
2. **Book coverage**: ≥90% of snapshot rows in the active window have
   `best_ask_up IS NOT NULL`. Gaps >10% require investigation before
   research.
3. **Spread sanity**: median spread on the up token over the active
   window is within 3σ of the long-run distribution. A sudden spread
   regime change blocks research until reviewed.
4. **Look-ahead causality**: for a sample of feature computations,
   recompute the feature on a truncated copy of the underlying prices
   (truncated at index `i + 1`) and verify the value at index `i` is
   unchanged. This catches future-leaking features. Required for
   every new feature added to the feature store.
5. **Live-vs-backtest entry alignment**: confirm `best_ask_up` is
   populated for both PolyBackTest-fetched rows and live recorder
   rows. If the recorder is writing to a column the fetcher does not
   write to, the mismatch is flagged.
6. **Stationarity probe**: split the active window into thirds, compute
   feature means and variances per third, flag any feature whose mean
   shifts by more than 2σ between adjacent thirds. Stationarity drift
   is not necessarily a blocker but it changes the validation
   methodology (we may need to weight recent data more heavily, or
   restrict the window).
7. **Label sanity**: confirm `winner` is populated for all markets in
   the active window, and that the YES/NO ratio is within 0.45-0.55
   (no degenerate markets where one outcome is overwhelmingly
   common).
8. **Backfill freshness**: the most recent snapshot is within 1 hour
   of `now()` on the running machine. If we are working from a stale
   db, the audit warns prominently.

The audit's output is appended to `autoresearch/audit.jsonl`. A failure
blocks the research loop until a Data Steward run reports PASS.

## 7. Agent architecture (v1: three roles)

Per Codex's "control theater" critique, v1 ships with three agents
instead of seven. The three are the ones whose separation is
non-negotiable for honesty: data writer, research runner, and holdout
gatekeeper. Everything else (preregistration, red team, reconciliation)
is an orchestrator-level discipline (the conversational session running
this work) until paper trades exist for the Reconciler to consume.

### 7.1 Data Steward

**Role.** Owns the data layer end to end. Backfill, audit, feature
store rebuild, live recorder coordination.

**Tool whitelist.** Read/write to `backtesting/*.db`,
`backtesting/eval/features/`, `autoresearch/audit.jsonl`. Bash for
running the fetcher, the feature_store builder, and the coverage
audit. Web fetch for PolyBackTest docs. No access to
`autoresearch/candidates/`, `autoresearch/preregistrations/`, or any
holdout-tagged file.

**Responsibilities.**
- Run the PolyBackTest backfill on demand or on a schedule.
- Run the data audit and post the result to the audit log. Refuse to
  return success if any check fails.
- Rebuild the feature store after every backfill.
- Maintain the integrity of the live recorder's bid/ask columns.
- Flag PolyBackTest collection gaps and decide whether to refetch.

**Refusal cases.** Refuses to run any forecaster training or
evaluation. Refuses to read candidate artifacts. Refuses to write to
the holdout partition.

### 7.2 Researcher

**Role.** Trains forecasters on TRAIN, validates on CV, produces
candidate artifacts. Combines what would otherwise be separate Searcher
and Validator agents because their separation does not catch a real
failure mode that the audit log does not already catch.

**Tool whitelist.** Read access to `backtesting/*.db` (snapshots in
TRAIN+CV partitions only — enforced by a `PreToolUse` hook that filters
SQL queries by `start_time`). Read access to
`autoresearch/preregistrations/`, `autoresearch/audit.jsonl`. Write
access to `autoresearch/candidates/<hash>/` for the candidates it
produces. No read access to the HOLDOUT partition. No read access to
paper trade results.

**Responsibilities.**
- Read the active preregistration. Refuse to run if no preregistration
  is committed for this question.
- Train the forecaster family declared in the preregistration on TRAIN.
- Apply isotonic recalibration on TRAIN (if declared).
- Compute log growth on CV with block bootstrap CIs, market-implied
  baseline comparison, slice-conditional calibration, Murphy
  decomposition, PBO across the grid.
- Write the candidate artifact directory:
  `autoresearch/candidates/<hash>/{search_result.json,
  validation_report.json, calibration_report.json,
  preregistration_ref.txt}`.
- Append a hash-chained entry to `autoresearch/audit.jsonl`.

**Refusal cases.** Refuses to run if the preregistration commit hash
does not match `git rev-parse HEAD` of the preregistration file.
Refuses to run if PBO across the grid exceeds the preregistered cap.
Refuses to read holdout files. Refuses to write outside its candidate
directory.

### 7.3 Holdout Burner

**Role.** The only agent with read access to the HOLDOUT partition.
Runs a single, irreversible holdout evaluation per candidate hash.

**Tool whitelist.** Read access to `backtesting/*.db` for HOLDOUT
partition rows only. Read access to a single candidate directory.
Write access to `autoresearch/candidates/<hash>/holdout_result.json`
and the audit log. Nothing else.

**Responsibilities.**
- Take a candidate hash as input.
- Verify the audit log does not already contain a `holdout_burned`
  entry for this hash. If it does, refuse and return the prior result.
- Verify the holdout budget for this coin/week has remaining capacity.
  If exhausted, refuse and return the budget status.
- Load the forecaster from the candidate directory.
- Run the same simulator used in CV, on the HOLDOUT partition.
- Write `holdout_result.json` (log_growth lower CI, calibration on
  holdout, slice breakdowns).
- Append a `holdout_burned` audit log entry with the hash and result.
- Mark the holdout as consumed for this candidate. Subsequent
  invocations on the same hash refuse.

**Refusal cases.** Same hash twice. Holdout budget exhausted.
Candidate directory missing the validation report. Preregistration
commit hash does not match the candidate's `preregistration_ref.txt`.

### 7.4 The orchestrator (this conversational session)

Until paper trading produces enough artifacts for a Reconciler agent to
have anything to reconcile, the orchestrator (the conversational
session running this work) carries these responsibilities:

- **Preregistration writing.** Drafts the markdown file, commits it,
  hands the commit hash to the Researcher.
- **Red Team review.** Before invoking the Holdout Burner on any
  candidate, the orchestrator must produce a "best argument against
  this candidate" markdown file that lists the most plausible failure
  mode, the regime under which it would fail, and a kill criterion for
  paper trading. The Holdout Burner refuses to run without this file
  in the candidate directory.
- **Promotion decision.** The orchestrator decides, based on the
  holdout result + the red team review + a human (you) approving, to
  write the candidate to `strategy_config.json` for paper trading.

The orchestrator is hard-bounded for the holdout partition: a
`PreToolUse` hook in `.claude/settings.json` blocks any `Read` or
`Bash` call that touches snapshots in the HOLDOUT date range, unless
the call is sourced from an active Holdout Burner subagent invocation.
This is the one structural enforcement that exists in v1 outside the
agent boundaries themselves.

Every other discipline (preregistration, red team, promotion) is
honor-system, backed by the audit log so violations are forensically
visible afterward.

## 8. Audit log specification

`autoresearch/audit.jsonl` is the source of truth for "what actually
happened." Append-only. Hash-chained. Every entry includes the SHA256
of the previous entry.

### 8.1 Entry schema

```jsonc
{
  "ts": "2026-04-07T22:30:00Z",
  "agent": "data_steward",          // or "researcher", "holdout_burner", "orchestrator"
  "action": "data_audit_passed",    // see action vocabulary below
  "subject": {                      // what the action acted on
    "type": "dataset",
    "coin": "btc",
    "fingerprint": "sha256:..."
  },
  "details": { ... },               // action-specific
  "git_head": "f25e633",            // commit at the moment of action
  "prev_hash": "sha256:...",        // hash of the previous entry's full row
  "self_hash": "sha256:..."         // hash of THIS entry computed without self_hash field
}
```

### 8.2 Action vocabulary

- `data_backfilled`: a backfill run completed
- `data_audit_run`: a coverage/integrity audit was run; `details.passed`
- `feature_store_built`: feature store rebuild completed
- `preregistration_committed`: a new preregistration was sealed
- `search_run`: a Researcher search across a grid completed
- `candidate_written`: a candidate artifact directory was written
- `candidate_red_teamed`: orchestrator wrote the falsification doc
- `holdout_burned`: a holdout evaluation was run on a candidate
- `holdout_budget_exhausted`: a holdout request was refused for budget
- `candidate_promoted`: a candidate was written to strategy_config
- `candidate_killed`: a candidate was removed from production

### 8.3 Tamper detection

The audit log writer (`autoresearch/audit.py`) provides:

- `append(entry)`: computes `prev_hash` from the last line, computes
  `self_hash`, atomically appends.
- `verify()`: walks the file, recomputes each entry's `self_hash` and
  confirms `prev_hash` matches the prior entry's `self_hash`. Any
  mismatch fails loudly with the offending line number.
- A pre-commit hook runs `verify()` on the audit log before any commit
  that touches it.

Tampering is not the threat model (there is one user). The hash chain
exists so that an LLM agent cannot quietly rewrite history without
the violation being obvious afterward.

## 9. Preregistration format

A preregistration is a markdown file at
`autoresearch/preregistrations/<utc>-<coin>-<purpose>.md` that:

1. States the question being asked in one sentence.
2. Lists the forecaster family (logistic, isotonic, GBT, etc.) and the
   parameter grid (concrete, finite, ≤100 cells per family). The grid
   is specified as YAML inside the markdown for parseability.
3. States the objective function (formula or reference to a function).
4. States the dataset fingerprint (coin + commit of the partition
   function + the date range the partition covers).
5. States the calibration method (none, isotonic, Platt).
6. States `KELLY_FRACTION`, `SHRINK_ALPHA`, `MIN_EDGE`,
   `MAX_BET_FRACTION` policies.
7. States the block length used for bootstrap and how it was derived.
8. States the maximum acceptable PBO across the grid (default 0.4).
9. States the decision rule for promotion to holdout testing.
10. States the holdout pass criteria (concrete numbers).

The preregistration is committed to git in its own commit. The commit
hash is the registration receipt. Subsequent edits to the file are
forbidden; if the question changes, write a new preregistration in a
new file and acknowledge in it that the new question came from
inspecting prior results.

A template lives at
`autoresearch/preregistrations/_template.md`. The Researcher agent
parses preregistrations against this template and refuses to run if
required fields are missing or the parameter grid exceeds the cap.

## 10. Candidate directory layout

Each candidate that survives the Researcher stage gets a directory
`autoresearch/candidates/<hash>/` where `hash` is:

```
sha256(
    forecaster_family ||
    json.dumps(params, sort_keys=True) ||
    dataset_fingerprint ||
    preregistration_commit_hash
)
```

This means: same forecaster, same params, but different data → different
hash. Same params, same data, but different preregistration → different
hash. The hash is the unit the holdout budget is spent against.

Files inside:

```
preregistration_ref.txt            # one line, the preregistration commit hash
search_result.json                 # in-sample stats from TRAIN
validation_report.json             # CV log growth, bootstrap CI, baseline gap
calibration_report.json            # Brier, Murphy, reliability diagram, slices
red_team_review.md                 # written by orchestrator before holdout
holdout_result.json                # written ONCE by Holdout Burner
promotion.json                     # written when human approves promotion
```

## 11. Wave plan: features then forecasters

### 11.1 Wave 1 — stacked logistic over current signals + microstructure

The current `threshold`, `consistency`, `velocity`, `skew`, `combo`,
`vel+cons`, `accel+time`, `volatility`, `acceleration`, `timing`
strategies are reframed as **feature columns**. They become 10-ish
booleans (filter fired or not) or numeric scores that feed into a
single L2-regularised logistic regression.

To this we add **microstructure features** that the existing strategies
do not see:

- Top-of-book spread on YES and NO
- Top-of-book size imbalance: `(bid_size_up - ask_size_up) / (bid_size_up + ask_size_up)`
- Book skew: difference between weighted mid (size-weighted) and raw mid
- Quote update rate: number of distinct best_ask values per N seconds
- BTC ↔ ETH lag indicator: how fast the BTC price moved in the last N
  seconds (when training the ETH forecaster) and vice versa
- Time-of-day one-hot

The wave 1 forecaster is `LogisticRegression(C=1.0, penalty='l2')`
fit on these features, with isotonic recalibration applied on top.

**Wave 1 expected outcome.** Codex flagged that wave 1 is unlikely to
produce a usable strategy, because the existing signals are all
transforms of the same few price-path ingredients. We agree. **The
purpose of wave 1 is to validate the framework, not the strategy.** If
the framework correctly says "wave 1 fails the edge gate against the
market-implied baseline", that is the framework working. If wave 1
passes the holdout and produces a tradable strategy, we should be
*deeply suspicious* and the Red Team's job is to attack it.

The wave 1 deliverable is a complete end-to-end run from data audit to
holdout result, with the framework reporting an honest "no candidate
passes" outcome. This is the smoke test for the entire pipeline.

### 11.2 Wave 2 — gradient-boosted trees with explicit recalibration

Once wave 1 has established the framework correctly rejects naive
features, wave 2 introduces a higher-capacity model on the same
microstructure-augmented feature set:

- LightGBM with shallow trees (max_depth ≤ 4), early stopping on CV.
- Platt scaling on top (GBT outputs are notoriously miscalibrated).
- Same calibration audit and slice-conditional checks.

Wave 2 will overfit unless the hyperparameter grid is small. The
preregistration caps the grid at 30 cells.

### 11.3 Wave 3 — multi-trade-per-market

Currently the simulator takes one trade per market and stops, matching
the legacy `find_first_trade` behaviour. Wave 3 introduces multiple
entries per market (re-entry on retracement, exit before resolution
when q drifts away from edge) and the state tracking that requires.
This is the change that opens the path to the kind of returns the
"bots doing well" stories actually involve. Wave 3 is gated on wave 2
producing a candidate that survives holdout — there is no point making
the simulator more capable if the basic forecaster cannot beat the
spread.

### 11.4 What is explicitly out of scope for wave 1-3

- Market making (providing liquidity rather than taking it)
- Cross-market relative value (BTC vs ETH, 5m vs 1h windows)
- Forecaster ensembles
- Reinforcement learning
- Meta-strategies that combine multiple forecasters

These can be added later under the same framework discipline. Adding
them before wave 3 succeeds is overengineering against an unproven
substrate.

## 12. Reconciler (deferred to v2)

The Reconciler does not exist in v1 because there are no paper trades
to reconcile against. When the first promoted candidate has accumulated
≥100 paper trades, the Reconciler agent gets built and:

- Reads paper trades from Supabase, filtered by `candidate_hash`.
- Recomputes per-bucket realised win rate against forecasted q.
- Runs a sequential significance test (CUSUM or SPRT) on the
  calibration residuals.
- **Review trigger**: |z| > 2.5 on aggregate calibration residuals.
- **Kill trigger**: |z| > 3.0 on two consecutive review windows OR
  PnL drift exceeding the holdout's predicted log growth lower CI by
  more than 1.5 standard errors.

Both triggers require a minimum of 100 trades total and 25 trades in
the active edge region, per Codex's stress test of the original 30-50
trade kill criterion.

**Calibration drift, not PnL drift, is the primary kill criterion** —
because PnL is too noisy in <100 trades to detect failure quickly, but
calibration residuals are detectable in 30-50. PnL drift is the
secondary criterion only.

A new column `candidate_hash` is required on the Supabase `trades`
table for the Reconciler to attribute paper trades to artifacts. This
is a small migration that ships with the Reconciler.

## 13. Build order

In strict dependency order. Each phase commits when complete and is
verified before the next phase starts.

### Phase 1 — data layer (largely DONE as of 2026-04-07)

Status: in progress, blocking everything else.

- [x] Fix the orderbook fetcher (commit `dc864c7`)
- [x] Schema migration for orderbook columns (commit `b96c044`)
- [x] Feature store reads orderbook columns (commit `f25e633`)
- [x] Coverage audit script (commit `f25e633`)
- [ ] Backfill BTC and ETH from PolyBackTest with `include_orderbook=true`
      (in progress, ~30-60 min wall time)
- [ ] Verify book coverage ≥90% per coin via the audit script
- [ ] Rebuild the feature store from the corrected snapshots
- [ ] Final coverage audit to confirm everything is honest

### Phase 2 — methods library

Pure functions with unit tests, no agent integration yet:

- [ ] `autoresearch/methods/bootstrap.py`: stationary block bootstrap,
      ACF-based block length selection
- [ ] `autoresearch/methods/cpcv.py`: combinatorial purged
      cross-validation with embargo
- [ ] `autoresearch/methods/calibration.py`: Brier, Murphy decomposition,
      reliability diagrams, isotonic and Platt recalibration
- [ ] `autoresearch/methods/pbo.py`: probability of backtest overfitting
- [ ] `autoresearch/methods/edge.py`: edge after costs, market-implied
      baseline, MIN_EDGE policy
- [ ] `autoresearch/methods/sizing.py`: shrinkage, fractional Kelly,
      MAX_BET_FRACTION cap
- [ ] `autoresearch/methods/simulator.py`: forward simulator with
      best_ask entry, fee model, slippage from orderbook depth

Each module ships with its own test file. The simulator gets a
property-based test that verifies it produces zero PnL on a random
forecaster against the market-implied baseline (sanity check on the
edge accounting).

### Phase 3 — audit log + preregistration plumbing

- [ ] `autoresearch/audit.py`: append-only hash-chained writer plus
      `verify()` walker
- [ ] `autoresearch/preregistrations/_template.md`
- [ ] `autoresearch/partitions.py`: partition function with stable
      commit hash referencing
- [ ] Pre-commit hook that runs `audit.verify()`

### Phase 4 — Data Steward agent

- [ ] `.claude/agents/data-steward.md` system prompt + tool whitelist
- [ ] Data audit skill that runs the 8 checks in section 6
- [ ] Backfill skill that wraps the fetcher
- [ ] Feature store rebuild skill
- [ ] All three skills emit audit log entries

### Phase 5 — Researcher agent

- [ ] `.claude/agents/researcher.md` system prompt + tool whitelist
- [ ] Researcher reads preregistration, runs CPCV, computes calibration,
      writes candidate artifact, emits audit log entry
- [ ] Wave 1 forecaster: stacked logistic over existing signals +
      microstructure features
- [ ] Cross-coin transfer report: train on BTC, evaluate on ETH and vice
      versa, log the result alongside the in-coin candidate

### Phase 6 — Holdout Burner agent

- [ ] `.claude/agents/holdout-burner.md` system prompt + tool whitelist
- [ ] Holdout budget tracker
- [ ] PreToolUse hook that blocks holdout reads from any non-burner
      session
- [ ] One-shot enforcement via audit log lookup

### Phase 7 — first end-to-end run on real data

- [ ] Preregister the wave 1 logistic candidate
- [ ] Researcher run produces a candidate (or honestly says "PBO too
      high, no candidate")
- [ ] Orchestrator writes red team review
- [ ] Holdout Burner runs (or refuses) on the candidate
- [ ] We learn whether wave 1 produces anything tradable on real data

### Phase 8 — paper trading + Reconciler (deferred)

- [ ] If a wave 1 candidate passed holdout, promote to paper
- [ ] Add `candidate_hash` to Supabase `trades` table
- [ ] Build Reconciler agent
- [ ] Run weekly drift checks
- [ ] Use the live results to inform wave 2

### Phase 9 — wave 2

- [ ] Add LightGBM forecaster + Platt recalibration
- [ ] Preregister, run, holdout-test
- [ ] Compare against wave 1 baseline

### Phase 10 — wave 3 (multi-trade per market)

- [ ] Refactor simulator to support state across multiple entries
- [ ] Refactor `find_first_trade` to a per-step decision function
- [ ] Re-run wave 2 forecaster on multi-trade simulator
- [ ] Validate that the multi-trade behaviour is justified by the
      data, not by overfitting more degrees of freedom

## 14. Open questions and red-team challenges

These are the things this design does not yet answer and that the
implementer must keep in mind:

1. **PolyBackTest snapshot cadence consistency.** The audit reports
   ~2,500-3,000 snapshots per 5-min market, which is a snapshot
   roughly every 0.1 seconds. Is the cadence uniform or event-driven?
   If event-driven (snapshot on every book update), then "snapshot
   index" is not a uniform time axis and CPCV embargoes need to be
   computed in market-time, not snapshot-index time.
2. **Live recorder ↔ backfill interference.** If the local recorder
   runs concurrently with a backfill against the same db, the
   recorder's INSERT and the fetcher's INSERT can interleave. SQLite
   WAL mode handles this safely but the resulting rows may have
   different feature populations (recorder rows have no orderbook
   JSON). The Data Steward audit must distinguish them.
3. **Cross-coin contamination.** If the BTC forecaster uses ETH price
   features (per the wave 1 microstructure list), the BTC and ETH
   experiments are no longer independent. The Researcher must use the
   same time partition for both coins so a holdout failure on BTC
   does not silently bleed into the ETH search.
4. **Market-implied baseline asymmetry.** The "market is right"
   baseline should compute `q_market(p_yes_ask, p_no_ask)` as a
   function of both sides, not just one. If `p_yes_ask + p_no_ask`
   diverges meaningfully from 1.0 (because of fees built into the
   spread), the baseline must account for the asymmetry.
5. **The 5-day -> 31-day data shift.** When the backfill completes,
   the dataset jumps from 5 days to 31 days. Any existing strategy
   that was tuned on 5 days is now sitting on a different
   distribution. We should not retain any prior champion artifacts
   (`autoresearch/candidates/*.json`) across this transition.
6. **What if PolyBackTest's 92.7% book coverage gets worse for older
   markets?** Older markets may have lower book coverage if
   PolyBackTest's collection started later. The Data Steward must
   report book coverage per day, not just aggregate, and the
   Researcher must skip days below a coverage threshold.
7. **Spread regime shifts during active hours.** Median spread is 1¢
   but max is 39¢. We have not yet characterised whether the wide-
   spread tail is concentrated at certain hours (e.g. low-liquidity
   overnight). If so, the slice-conditional calibration check on hour
   buckets becomes crucial.
8. **Slippage model fidelity.** The orderbook JSON gives 15 levels
   of depth. The slippage model can simulate fills against that
   depth, but it cannot simulate competition (other takers hitting
   the same depth before us). For sized simulations beyond top of
   book, we should reduce the available size at each level by an
   "other takers" haircut declared in the preregistration.

## 15. Glossary

- **q**: model's estimate of P(YES wins) for a binary contract
- **p_ask**: best ask on the contract being traded (live taker price)
- **edge**: `q - p_break_even` where break-even includes fees
- **fractional Kelly**: stake = fraction × ((q - p) / (1 - p)) × bankroll
- **Brier score**: mean squared error of probabilistic forecasts
- **Murphy decomposition**: `Brier = uncertainty − resolution + calibration`
- **CPCV**: Combinatorial Purged Cross-Validation (López de Prado)
- **PBO**: Probability of Backtest Overfitting
- **Block bootstrap**: bootstrap that resamples contiguous blocks to
  preserve autocorrelation in time-series data
- **Holdout budget**: max number of holdout evaluations per coin per
  week, regardless of how distinct the candidates are; a Bonferroni-ish
  cap on family-wise error from peeking
- **Reliability diagram**: plot of forecasted probability vs realized
  frequency, bucketed by quantile
- **Slice-conditional calibration**: reliability diagrams computed
  separately for time-of-day, vol regime, market thinness, etc.

---

## Appendix A — relationship to the existing codebase

What stays, what gets rewritten, what gets deleted.

### Stays as-is

- `collector/snapshot_recorder.py` (already writes bid/ask to Supabase;
  needs minor patch to also write to local sqlite columns when those
  exist)
- `shared/fees.py` (taker fee model)
- `shared/db.py` (now with extended snapshots schema)
- `backtesting/data/fetcher.py` (now with `include_orderbook=true`)
- `strategies/registry.py` and the existing filter functions — but
  reframed as **features**, not strategies, and called from a
  forecaster module

### Rewritten

- `autoresearch/runner.py` → orchestrator that delegates to subagents
- `autoresearch/search.py` → Researcher agent's executable
- `autoresearch/metrics.py` → log_growth, Brier, Murphy, edge, baseline
- `autoresearch/models.py` → ForecasterSpec, Calibration, EdgeReport
- `autoresearch/store.py` → audit log writer
- `backtesting/eval/feature_store.py` → already updated to read book
  columns
- `backtesting/eval/validate_strategy_readiness.py` → CPCV instead of
  expanding-window walk-forward, calibration audit, edge gate

### Deleted

- `autoresearch/champion.py` (recency-leakage decision score)
- `autoresearch/program.md` (replaced by this spec + the discipline doc)
- All files under `autoresearch/candidates/` and
  `autoresearch/reports/` (legacy artifacts produced under the old
  pipeline; cannot be trusted)

### New

- `autoresearch/audit.py` and `autoresearch/audit.jsonl`
- `autoresearch/partitions.py`
- `autoresearch/preregistrations/_template.md`
- `autoresearch/methods/{bootstrap,cpcv,calibration,pbo,edge,sizing,simulator}.py`
- `autoresearch/forecasters/{logistic,gbt,baseline}.py`
- `.claude/agents/{data-steward,researcher,holdout-burner}.md`
- `.claude/settings.json` PreToolUse hook for holdout protection
- `docs/research-discipline.md` (the human-readable discipline doc)
- `CLAUDE.md` at the project root requiring agents to read the
  discipline doc on every session

---

## Appendix B — open changes the spec depends on

These are not yet built but the spec assumes them:

1. The PolyBackTest backfill with `include_orderbook=true` completes
   and the coverage audit reports ≥90% book coverage per coin.
2. The Supabase `trades` table gets a `candidate_hash` column (deferred
   until the Reconciler is built).
3. The live recorder writes bid/ask to the local sqlite columns
   (currently writes only to Supabase). Defer-able as long as live
   simulation is run against PolyBackTest's data, not the recorder's.
