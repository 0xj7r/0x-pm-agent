Wallet research artifacts (8b5b82) — generated from
`scripts/backfill_wallet_history.py --wallet 0xb27bc932bf8110d8f78e55da7d5f0497a18b5b82`.

Included:
- summary.json
- phase_summary.json
- activity_windows_summary.json
- closed_positions.json
- accounting_snapshot.zip and accounting_snapshot_meta.json
- accounting_snapshot/equity.csv
- accounting_snapshot/positions.csv

Excluded from this commit by design:
- activity_history.json
- activity_windows/*.json

These large files are reproducible with the same backfill script and are kept in the
local workspace if you need to rerun feature engineering.
