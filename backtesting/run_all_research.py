"""Run autoresearch per coin, store best strategies to JSON config.

Usage:
    python backtesting/run_all_research.py
"""
from __future__ import annotations

import json
import logging
import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).parent.parent))

from backtesting.autoresearch import run_autoresearch

logger = logging.getLogger(__name__)

BASE_DIR = Path(__file__).parent
OUTPUT = BASE_DIR / "strategy_results.json"
COINS = ["btc", "eth", "sol"]


def main():
    logging.basicConfig(level=logging.INFO, format="%(asctime)s %(levelname)s %(message)s")

    all_results = {}

    for coin in COINS:
        db_path = BASE_DIR / f"{coin}.db"
        if not db_path.exists():
            logger.warning(f"No DB for {coin}, skipping")
            continue

        logger.info(f"Running autoresearch for {coin.upper()}...")
        results = run_autoresearch(db_path, test_pct=0.3, min_markets=50, coin=coin)

        if not results:
            logger.warning(f"No profitable strategies for {coin}")
            continue

        top = results[:10]
        coin_results = {
            "best_strategy": {
                "name": top[0].name,
                "params": top[0].params,
                "train_win_rate": round(top[0].train_win_rate, 4),
                "test_win_rate": round(top[0].test_win_rate, 4),
                "train_trades": top[0].train_trades,
                "test_trades": top[0].test_trades,
                "train_pnl": round(top[0].train_pnl, 4),
                "test_pnl": round(top[0].test_pnl, 4),
                "avg_entry": round(top[0].avg_entry, 4),
                "test_pnl_per_trade": round(top[0].test_pnl_per_trade, 4),
            },
            "top_5": [
                {
                    "name": r.name,
                    "params": r.params,
                    "train_win_rate": round(r.train_win_rate, 4),
                    "test_win_rate": round(r.test_win_rate, 4),
                    "train_trades": r.train_trades,
                    "test_trades": r.test_trades,
                    "avg_entry": round(r.avg_entry, 4),
                    "test_pnl_per_trade": round(r.test_pnl_per_trade, 4),
                }
                for r in top[:5]
            ],
            "total_profitable_strategies": len(results),
        }
        all_results[coin] = coin_results

        logger.info(f"[{coin.upper()}] Best: {top[0].name} {top[0].params} "
                    f"(test WR={top[0].test_win_rate:.0%}, "
                    f"${top[0].test_pnl_per_trade:+.4f}/trade)")

    OUTPUT.write_text(json.dumps(all_results, indent=2))
    logger.info(f"Results saved to {OUTPUT}")

    print(f"\n{'='*70}")
    print("STRATEGY SUMMARY")
    print(f"{'='*70}\n")
    for coin, data in all_results.items():
        best = data["best_strategy"]
        print(f"{coin.upper()}: {best['name']} {best['params']}")
        print(f"  Train: {best['train_trades']} trades, {best['train_win_rate']:.0%} WR")
        print(f"  Test:  {best['test_trades']} trades, {best['test_win_rate']:.0%} WR")
        print(f"  Avg entry: ${best['avg_entry']:.3f}, P&L/trade: ${best['test_pnl_per_trade']:+.4f}")
        print()


if __name__ == "__main__":
    main()
