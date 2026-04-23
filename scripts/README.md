# scripts

Repository-level script organization:

- `scripts/analysis/` — analysis/research utilities.
- `scripts/bots/` — strategy and trading bots.
- `scripts/dataops/` — ingestion/reconciliation and maintenance jobs.
- `scripts/deploy/` — environment and deployment scripts.
- `scripts/whale_pair/` — whale-pair strategy-specific command-line tools and presets.

Legacy command locations (kept for compatibility):

- `scripts/*.py` and `scripts/*.sh` still resolve to the canonical files in this
  directory tree through `scripts/_legacy_script_dispatch.sh`.
