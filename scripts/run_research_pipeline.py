"""Run the historical research pipeline and produce champion decisions."""
from __future__ import annotations

import argparse
import json
import sys
from datetime import datetime, timezone
from pathlib import Path
from typing import Any

sys.path.insert(0, str(Path(__file__).resolve().parent.parent))

from autoresearch.champion import rank_candidates
from autoresearch.search import shortlist_candidates
from autoresearch.store import RESULTS_PATH
from backtesting.run_backtest_all import extract_trades, run_monte_carlo
from shared.constants import COINS

BASE_DIR = Path(__file__).resolve().parent.parent
REPORTS_DIR = BASE_DIR / "autoresearch" / "reports"


def _backtest_summary(coin: str, chosen: dict[str, Any], months: int, sims: int, start_balance: float, bet_pct: float) -> dict[str, Any]:
    strategy_name = chosen["name"]
    params = chosen["params"]
    trades = extract_trades(coin, params, strategy_name)
    wins = sum(1 for trade in trades if trade["won"])
    trades_per_day = max(len(trades) / max(months * 30, 1), 1.0)
    mc = run_monte_carlo(
        trades,
        starting_balance=start_balance,
        bet_pct=bet_pct,
        months=months,
        trades_per_day=trades_per_day,
        num_sims=sims,
    ) if trades else {}
    return {
        "observed_trades": len(trades),
        "observed_wins": wins,
        "observed_win_rate": wins / len(trades) if trades else 0.0,
        "trades_per_day_assumption": trades_per_day,
        "monte_carlo": mc,
    }


def _results_entry(chosen: dict[str, Any]) -> dict[str, Any]:
    candidate = chosen["search_candidate"]
    validation = chosen["validation"]
    recent = chosen["recent"]["stats"]
    return {
        "best_strategy": {
            "name": chosen["name"],
            "params": chosen["params"],
            "train_win_rate": candidate["train_wr"],
            "test_win_rate": candidate["test_wr"],
            "train_trades": candidate["train_trades"],
            "test_trades": candidate["test_trades"],
            "train_pnl": candidate["train_pnl"],
            "test_pnl": candidate["test_pnl"],
            "test_sharpe": candidate["test_sharpe"],
            "avg_entry": validation["validation"]["final_holdout"]["avg_entry"],
            "test_pnl_per_trade": candidate["test_pnl_per_trade"],
            "readiness": validation["readiness"],
            "decision_score": chosen["decision_score"],
            "recent_trades": recent["trades"],
            "recent_win_rate": recent["win_rate"],
            "recent_pnl_per_trade": recent["pnl_per_trade"],
        },
        "top_5": [
            {
                "name": row["name"],
                "params": row["params"],
                "decision_score": row["decision_score"],
                "readiness": row["validation"]["readiness"],
                "test_win_rate": row["search_candidate"]["test_wr"],
                "test_trades": row["search_candidate"]["test_trades"],
                "test_sharpe": row["search_candidate"]["test_sharpe"],
                "recent_win_rate": row["recent"]["stats"]["win_rate"],
                "recent_trades": row["recent"]["stats"]["trades"],
                "recent_pnl_per_trade": row["recent"]["stats"]["pnl_per_trade"],
            }
            for row in chosen.get("rankings", [])[:5]
        ],
        "selection_notes": chosen.get("notes", []),
    }


def _compact_candidate(row: dict[str, Any]) -> dict[str, Any]:
    return {
        "name": row["name"],
        "params": row["params"],
        "train_trades": row["train_trades"],
        "test_trades": row["test_trades"],
        "train_pnl": row["train_pnl"],
        "test_pnl": row["test_pnl"],
        "test_pnl_per_trade": row["test_pnl_per_trade"],
        "test_sharpe": row["test_sharpe"],
        "rejection_reasons": row.get("rejection_reasons", []),
    }


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--coin", action="append", choices=COINS, default=[])
    parser.add_argument("--shortlist-size", type=int, default=12)
    parser.add_argument("--folds", type=int, default=5)
    parser.add_argument("--holdout-pct", type=float, default=0.2)
    parser.add_argument("--months", type=int, default=6)
    parser.add_argument("--sims", type=int, default=1000)
    parser.add_argument("--start-balance", type=float, default=100.0)
    parser.add_argument("--bet-pct", type=float, default=0.10)
    parser.add_argument("--write-results", action="store_true")
    args = parser.parse_args()

    selected_coins = args.coin or ["btc", "eth"]
    report: dict[str, Any] = {
        "created_at": datetime.now(timezone.utc).isoformat(),
        "coins": {},
    }
    results_payload: dict[str, Any] = {}

    for coin in selected_coins:
        shortlist, eligible_count, raw_profitable_count, raw_profitable_top, manifest, features, db_path = shortlist_candidates(
            coin,
            limit=args.shortlist_size,
        )
        decision = rank_candidates(
            coin,
            shortlist,
            manifest,
            features,
            folds=args.folds,
            holdout_pct=args.holdout_pct,
        )

        coin_report = {
            "dataset": {
                "db_path": str(db_path),
                "markets": len(manifest),
                "promotion_eligible_candidates": eligible_count,
                "raw_profitable_candidates": raw_profitable_count,
            },
            "decision": decision.to_dict(),
            "raw_profitable_top": [_compact_candidate(row) for row in raw_profitable_top],
        }

        if decision.chosen:
            coin_report["backtest"] = _backtest_summary(
                coin,
                decision.chosen,
                months=args.months,
                sims=args.sims,
                start_balance=args.start_balance,
                bet_pct=args.bet_pct,
            )
            results_payload[coin] = _results_entry(
                {
                    **decision.chosen,
                    "rankings": decision.rankings,
                }
            )
        report["coins"][coin] = coin_report

    REPORTS_DIR.mkdir(parents=True, exist_ok=True)
    stamp = datetime.now(timezone.utc).strftime("%Y%m%dT%H%M%SZ")
    report_path = REPORTS_DIR / f"{stamp}-champions.json"
    report_path.write_text(json.dumps(report, indent=2))

    if args.write_results:
        RESULTS_PATH.write_text(json.dumps(results_payload, indent=2))

    print(json.dumps({"report_path": str(report_path), "coins": list(report["coins"].keys())}, indent=2))


if __name__ == "__main__":
    main()
