# scripts/dataops

Data ingestion and maintenance scripts:

- Activity and marketplace backfills (`backfill_wallet_history.py`, `backfill_supabase_markets.py`, `backfill_supervisor.sh`)
- Data reconciliation/normalization (`reconcile_whale_funding.py`, `join_wallet...`, `verify_*` utilities)
- One-off DB/ledger helpers (`rebuild_local_db_from_supabase.py`, `migrate_orderbook_columns.py`, `dedupe_supabase_trades.py`)

Legacy entrypoints remain at repository root and route through
`scripts/_legacy_script_dispatch.sh`.

The dispatch layer now resolves canonical scripts automatically by scanning
`scripts/dataops` (and the other grouped directories), which keeps future
command additions lightweight.
