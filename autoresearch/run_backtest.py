"""CLI for running backtests against strategy_config.json.

Used by the autoresearch loop to evaluate strategy mutations.
Supports real (PolyBackTest snapshots) and synthetic modes.

Usage:
    python autoresearch/run_backtest.py --config strategy_config.json
    python autoresearch/run_backtest.py --config strategy_config.json --mode synthetic --windows 200
"""
from __future__ import annotations

import argparse
import json
import random
import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).parent.parent))

from backtesting.btc_backtest import BacktestResult, run_backtest_snapshots
from backtesting.historical_data import DB_PATH
from strategies.strategy_config import load_strategy_config


def _print_result(result: BacktestResult, label: str, as_json: bool) -> None:
    if as_json:
        data = {
            "mode": label,
            "num_trades": result.num_trades,
            "wins": result.wins,
            "losses": result.losses,
            "win_rate": round(result.win_rate, 4),
            "total_pnl": round(result.total_pnl, 2),
            "ev_per_trade": round(result.ev_per_trade, 4),
            "sharpe": round(result.sharpe, 4),
            "max_drawdown": round(result.max_drawdown, 4),
            "starting_balance": result.starting_balance,
            "ending_balance": round(result.ending_balance, 2),
        }
        if result.trades:
            strategies = {}
            for t in result.trades:
                s = t.get("strategy", "unknown")
                if s not in strategies:
                    strategies[s] = {"trades": 0, "wins": 0, "pnl": 0.0}
                strategies[s]["trades"] += 1
                if t["won"]:
                    strategies[s]["wins"] += 1
                strategies[s]["pnl"] += t["pnl"]
            data["by_strategy"] = strategies
        print(json.dumps(data))
    else:
        print(f"{'='*50}")
        print(f"BACKTEST RESULTS [{label}]")
        print(f"{'='*50}")
        print(f"Trades:          {result.num_trades}")
        print(f"Wins:            {result.wins}")
        print(f"Losses:          {result.losses}")
        print(f"Win Rate:        {result.win_rate:.1%}")
        print(f"Total PnL:       ${result.total_pnl:+.2f}")
        print(f"EV per Trade:    ${result.ev_per_trade:.4f}")
        print(f"Sharpe Ratio:    {result.sharpe:.4f}")
        print(f"Max Drawdown:    {result.max_drawdown:.1%}")
        print(f"Starting Balance:${result.starting_balance:.2f}")
        print(f"Ending Balance:  ${result.ending_balance:.2f}")
        print(f"{'='*50}")
        if result.trades:
            strategies: dict[str, list] = {}
            for t in result.trades:
                s = t.get("strategy", "unknown")
                strategies.setdefault(s, []).append(t)
            for s, trades in strategies.items():
                wins = sum(1 for t in trades if t["won"])
                pnl = sum(t["pnl"] for t in trades)
                print(f"  {s.upper()}: {len(trades)} trades, "
                      f"{wins}/{len(trades)} wins, ${pnl:+.2f}")


def main() -> None:
    parser = argparse.ArgumentParser(description="Run BTC sniper backtest")
    parser.add_argument("--config", required=True, help="Path to strategy_config.json")
    parser.add_argument("--balance", type=float, default=100.0, help="Starting balance")
    parser.add_argument("--json", action="store_true", help="Output as JSON")
    parser.add_argument("--db", type=str, default=None, help="Path to historical.db")
    args = parser.parse_args()

    cfg = load_strategy_config(args.config)
    db_path = Path(args.db) if args.db else DB_PATH

    if not db_path.exists():
        print(f"No data at {db_path}. Run: python backtesting/historical_data.py --limit 50",
              file=sys.stderr)
        sys.exit(1)

    result = run_backtest_snapshots(cfg, db_path=db_path, starting_balance=args.balance)
    _print_result(result, "real snapshots", args.json)


if __name__ == "__main__":
    main()
