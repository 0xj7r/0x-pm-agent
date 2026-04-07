# History

## 2026-04-07

### Duplicate trade writes in Supabase

- Symptom: the same logical trade appeared twice with timestamp variants like second precision vs microsecond precision.
- Root cause: trade row IDs were derived from `coin + market_id + timestamp`, so formatting differences produced different IDs and defeated `upsert`.
- Fix:
  - new writes now use a stable execution ID generated once at entry and reused at resolution
  - historical rows were deduped and migrated
- Guardrail:
  - do not use timestamps as uniqueness keys for trades
  - all trade persistence changes must preserve stable execution IDs across lifecycle updates

### Stale feature store used by autoresearch

- Symptom: research report showed `btc.markets = 20` and `eth.markets = 140` even though the local DBs had materially more snapshot markets.
- Root cause: `autoresearch/search.py` reused cached feature stores from `backtesting/eval/features/<coin>/` without checking whether the DB had been updated since the feature store was built.
- Fix:
  - `ensure_feature_store()` now refreshes via `append_new_markets()` when the DB mtime is newer than the feature manifest
- Guardrail:
  - after any DB rebuild or append, feature-store freshness must be validated before running search or backtests
  - report dataset counts must be compared against `SELECT COUNT(DISTINCT market_id) FROM snapshots`

### Research pipeline serialization mismatch

- Symptom: autoresearch crashed with `KeyError: 'sharpe'`.
- Root cause: `SearchCandidate.to_dict()` expected `train['sharpe']` and `train['max_drawdown']`, but `simulate_manifest()` did not return them.
- Fix:
  - `simulate_manifest()` now computes and returns `sharpe` and `max_drawdown`
- Guardrail:
  - candidate model fields and evaluator return payloads must stay aligned
  - serialization bugs should be caught by a pipeline smoke test

### Research pipeline CLI typo

- Symptom: the pipeline ran and then crashed at the end with `AttributeError: 'Namespace' object has no attribute 'write'`.
- Root cause: `scripts/run_research_pipeline.py` referenced `args.write-results` instead of `args.write_results`.
- Fix:
  - corrected the CLI attribute access
- Guardrail:
  - every new CLI entrypoint should have at least one end-to-end invocation test

### Long-running historical fetches failed on transient timeouts

- Symptom: BTC historical rebuild died during header fetch on `httpx.ReadTimeout`.
- Root cause: fetcher retried `429` responses but not transient network exceptions.
- Fix:
  - fetcher now retries transient `httpx` request failures with backoff
- Guardrail:
  - networked rebuild jobs must retry both rate limits and transient transport errors

### Backtesting directory was too flat

- Symptom: too many unrelated files accumulated under `backtesting/`, making ownership and navigation unclear.
- Fix:
  - grouped modules into:
    - `backtesting/data`
    - `backtesting/eval`
    - `backtesting/research`
    - `backtesting/analysis`
- Guardrail:
  - keep top-level `backtesting/` for entrypoints, artifacts, and shared modules only
  - place new library code in the appropriate subpackage

### Supabase markets table drifted empty while snapshots accumulated

- Symptom: Supabase `snapshots` had live data, but Supabase `markets` was empty.
- Root cause: the live recorder wrote snapshots to Supabase but did not upsert markets in the same path.
- Fix:
  - `collector/snapshot_recorder.py` now upserts market rows to Supabase before snapshot batch writes
  - added a markets-only backfill path using PolyBackTest headers
- Guardrail:
  - if snapshots are being written to Supabase for a coin, markets must be written in the same live path
  - snapshot backfills should never be considered complete if the corresponding markets metadata is missing

## Operating checks

- Before trusting any research report:
  - confirm DB snapshot-market counts
  - confirm feature-store manifest counts
  - confirm the report dataset counts match the feature store
- Before trusting trade analysis:
  - confirm duplicate same-strategy rows are zero
  - confirm legacy timestamp-derived IDs are zero
- Before promoting a strategy:
  - require canonical validation
  - require non-trivial sample size
  - require stress and robustness outputs
