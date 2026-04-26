# Paper Eval Sweeper

This is the ClawSweeper pattern applied to paper execution testing:
capture one shadow-live book stream, replay it across a calibration
matrix in parallel shards, and merge the evidence into one summary.

The sweeper is evidence-only. It does not promote strategy config, edit
env defaults, close issues, or touch live execution state.

## Capture Once

```bash
python3 polymarket-exec/scripts/paper_eval_sweeper.py capture-command \
  --out-dir data/evals/2026-04-25-shadow
```

Run the printed command block for a shadow-live session. It sources
`polymarket-exec/env/btc_5m_mm_shadowlive.env`, writes a book snapshot
JSONL, and writes a baseline paper report. The printed command sets
`WHALE_PAIR_EXEC_STARTING_CASH_USD=1000` so paper runs can actually
submit simulated orders.

Use a multi-hour capture for calibration. Short captures are still
useful for harness smoke tests, but they should not drive knob changes.

## Plan A Sweep

```bash
python3 polymarket-exec/scripts/paper_eval_sweeper.py plan \
  --snapshot data/evals/2026-04-25-shadow/capture/session.snap.jsonl \
  --baseline-report data/evals/2026-04-25-shadow/capture/baseline.report.json \
  --out-dir data/evals/2026-04-25-shadow/sweep-001 \
  --shard-count 10 \
  --include-tests
```

The planner reads `polymarket-exec/evals/paper_eval_matrix.json` and
writes:

- `plan.json`
- `shards/shard-<n>.json`
- one future artifact directory per job under `jobs/`
- `agent_prompt.md` in each job directory when that job runs

## Run Shards

Run locally:

```bash
python3 polymarket-exec/scripts/paper_eval_sweeper.py run-shard \
  --plan data/evals/2026-04-25-shadow/sweep-001/plan.json \
  --shard-index 0
```

Run all shards locally:

```bash
for i in $(seq 0 9); do
  python3 polymarket-exec/scripts/paper_eval_sweeper.py run-shard \
    --plan data/evals/2026-04-25-shadow/sweep-001/plan.json \
    --shard-index "$i" &
done
wait
```

Each replay job runs:

```bash
WHALE_PAIR_EXEC_MODE=replay \
WHALE_PAIR_REPLAY_INPUT_PATH=<snapshot> \
WHALE_PAIR_PAPER_REPORT_PATH=<job>/paper_report.json \
<variant env> \
cargo run -p polymarket-exec
```

If `WHALE_PAIR_ASSET_IDS` and `WHALE_PAIR_INSTRUMENT_MARKETS` are not
already set, the sweeper infers them from the snapshot log so offline
replay jobs can run without a sourced live env. If
`WHALE_PAIR_EXEC_STARTING_CASH_USD` is not already set, replay jobs
default it to `1000` for the same reason.

Then it writes `result.json`, `diff.md`, `suggestions.json`,
`agent_prompt.md`, `stdout.log`, and `stderr.log`.

`agent_prompt.md` includes the job command plus the prompt anchoring
rules from `polymarket-exec/evals/prompt_anchoring_patterns.md`. Those
rules reference the local `claude-code-ts` prompting patterns around
intent extraction, quality controls, self-verification, short concrete
tool summaries, and the Find/Verify/Dedupe task pipeline.

## Merge

```bash
python3 polymarket-exec/scripts/paper_eval_sweeper.py merge \
  --out-dir data/evals/2026-04-25-shadow/sweep-001
```

The merge step writes:

- `summary.md` for operator review
- `summary.json` for downstream automation

## Gates

The initial gates live in `polymarket-exec/evals/paper_eval_matrix.json`:

- minimum quality fills: 3
- maker fraction: at least 0.70
- edge capture ratio: at least 0.70
- average slippage: at most 10 bps
- variant maker fraction may not drop more than 0.15 vs baseline
- variant edge capture may not drop more than 0.25 vs baseline
- variant slippage may not rise more than 10 bps vs baseline

Low-fill runs are marked `warn`, not `fail`, because they are usually
capture-quality problems rather than proof that the execution model is
bad.

## Fix Anchoring

If a shard proposes a fix, it must include:

- the reproducible command or report metric that failed
- the artifact path containing the evidence
- the narrow code path it believes is responsible
- the command that should prove the fix

This is deliberate. Paper evals should prevent speculative execution
engine edits; they should not create them.

## Scaling

Use high shard counts for replay and deterministic tests. Keep
`shadow_live` capture concurrency low. The expensive live/demo part is
observing a real market window; the scalable part is replaying that same
book stream across many knobs and strategy variants.
