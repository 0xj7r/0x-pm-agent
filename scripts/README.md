# scripts

Repository-level script organization:

- `scripts/analysis/` — analysis/research utilities.
- `scripts/bots/` — strategy and trading bots.
- `scripts/dataops/` — ingestion/reconciliation and maintenance jobs.
- `scripts/deploy/` — environment and deployment scripts.
- `scripts/whale_pair/` — whale-pair strategy-specific command-line tools and presets.

Canonical location note:

- `scripts/analysis/` and `scripts/dataops/` are now canonical under
  `research/`.
- `scripts/bots/` and `scripts/whale_pair/` are now canonical under
  `execution/`.
- The top-level shim scripts (`scripts/*.py`, `scripts/*.sh`) remain for legacy
  launch compatibility and still execute their canonical counterparts.

Legacy command locations (kept for compatibility):

- `scripts/*.py` and `scripts/*.sh` still resolve to the canonical files in this
  directory tree through `scripts/_legacy_script_dispatch.sh`.
- `scripts/deploy/whale-pair` is a compatibility entry that points to
  `ops/deploy/whale-pair`.

The dispatch shim is now directory-driven, so newly added scripts placed under:
- `scripts/analysis/`
- `scripts/bots/`
- `scripts/dataops/`
- `scripts/whale_pair/cmd/`

automatically resolve via the top-level launcher names without per-script edits.
