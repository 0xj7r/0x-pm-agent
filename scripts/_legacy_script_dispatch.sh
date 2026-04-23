#!/usr/bin/env bash
set -euo pipefail

script_name="$(basename "$0")"

case "$script_name" in
  analyze_whale.py) target="analysis/analyze_whale.py" ;;
  cluster_whale_wallets.py) target="analysis/cluster_whale_wallets.py" ;;
  infer_whale_execution_features.py) target="analysis/infer_whale_execution_features.py" ;;
  join_wallet_to_market_state.py) target="analysis/join_wallet_to_market_state.py" ;;
  pnl_report.py) target="analysis/pnl_report.py" ;;
  profile_wallet_research.py) target="analysis/profile_wallet_research.py" ;;
  save_whale_analysis.py) target="analysis/save_whale_analysis.py" ;;
  sizing_rubric.py) target="analysis/sizing_rubric.py" ;;
  summarize_wallet_history.py) target="analysis/summarize_wallet_history.py" ;;
  validate_w1_model.py) target="analysis/validate_w1_model.py" ;;
  watch_wallets.py) target="analysis/watch_wallets.py" ;;
  backfill_supabase_markets.py) target="dataops/backfill_supabase_markets.py" ;;
  backfill_supervisor.sh) target="dataops/backfill_supervisor.sh" ;;
  backfill_wallet_history.py) target="dataops/backfill_wallet_history.py" ;;
  dedupe_supabase_trades.py) target="dataops/dedupe_supabase_trades.py" ;;
  migrate_orderbook_columns.py) target="dataops/migrate_orderbook_columns.py" ;;
  rebuild_local_db_from_supabase.py) target="dataops/rebuild_local_db_from_supabase.py" ;;
  reconcile_whale_funding.py) target="dataops/reconcile_whale_funding.py" ;;
  verify_orderbook_coverage.py) target="dataops/verify_orderbook_coverage.py" ;;
  verify_runtime_contract.py) target="dataops/verify_runtime_contract.py" ;;
  endgame_paper_bot.py) target="bots/endgame_paper_bot.py" ;;
  paired_carry_paper_bot.py) target="bots/paired_carry_paper_bot.py" ;;
  penny_paper_bot.py) target="bots/penny_paper_bot.py" ;;
  weather_live_scout.py) target="bots/weather_live_scout.py" ;;
  weather_paper_bot.py) target="bots/weather_paper_bot.py" ;;
  whale_pair_live_bot.py) target="bots/whale_pair_live_bot.py" ;;
  whale_pair_paper_bot.py) target="bots/whale_pair_paper_bot.py" ;;
  kelly_counterfactual.py) target="bots/kelly_counterfactual.py" ;;
  kelly_walkforward.py) target="bots/kelly_walkforward.py" ;;
  *)
    echo "Unknown legacy script wrapper: $script_name" >&2
    exit 2
    ;;
esac

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
case "$target" in
  *.py)
    exec python3 "$SCRIPT_DIR/$target" "$@"
    ;;
  *)
    exec "$SCRIPT_DIR/$target" "$@"
    ;;
esac
