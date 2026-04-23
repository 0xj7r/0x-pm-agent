# scripts/analysis

Wallet and market analysis helpers used for research:

- Trade and account behavior mining (`analyze_whale.py`, `infer_whale_execution_features.py`)
- Wallet clustering and profiles (`cluster_whale_wallets.py`, `watch_wallets.py`, `profile_wallet_research.py`)
- Reporting/summary utilities (`summarize_wallet_history.py`, `save_whale_analysis.py`, `pnl_report.py`, `validate_w1_model.py`)

Legacy entrypoints remain at repository root and route through
`scripts/_legacy_script_dispatch.sh`.

The dispatch layer now resolves canonical scripts automatically by scanning:
- `scripts/analysis`
- `scripts/bots`
- `scripts/dataops`
- `scripts/whale_pair/cmd`
