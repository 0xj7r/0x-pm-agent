"""CLI for running backtests against strategy_config.json.

Used by the autoresearch loop to evaluate strategy mutations.
Supports both synthetic and real (historical.db) data modes.

Usage:
    python autoresearch/run_backtest.py --config strategy_config.json
    python autoresearch/run_backtest.py --config strategy_config.json --mode real
    python autoresearch/run_backtest.py --config strategy_config.json --windows 100
"""
from __future__ import annotations

import argparse
import json
import random
import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).parent.parent))

from backtesting.btc_backtest import (
    BacktestResult,
    SimulatedWindow,
    run_backtest,
    run_backtest_real,
    load_real_windows,
)
from backtesting.historical_data import DB_PATH
from strategies.strategy_config import load_strategy_config


def generate_synthetic_windows(n: int = 200, seed: int = 42) -> list[SimulatedWindow]:
    """Generate synthetic historical windows from realistic BTC 5-min distributions.

    Real BTC 5-minute returns are approximately normally distributed with:
    - Mean: ~0 (slight positive drift in bull markets)
    - Std: ~0.15% to 0.30% depending on volatility regime

    We model this as N(0, 0.2) for price_move_pct, with occasional
    larger moves (fat tails) at +/- 0.5-1.0%.
    """
    rng = random.Random(seed)
    windows = []

    for i in range(n):
        move = rng.gauss(0, 0.2)

        if rng.random() < 0.05:
            move = rng.choice([-1, 1]) * rng.uniform(0.5, 1.5)

        direction = "UP" if move >= 0 else "DOWN"
        abs_move = abs(move)

        scenario = rng.random()

        if abs_move > 0.3 and scenario < 0.40:
            stale_price = max(0.01, 0.05 - abs_move * 0.02)
            if direction == "UP":
                up_price = stale_price
                down_price = 1.0 - stale_price
            else:
                down_price = stale_price
                up_price = 1.0 - stale_price

        elif abs_move > 0.3 and scenario < 0.65:
            if direction == "UP":
                up_price = 0.80 + rng.uniform(0, 0.15)
                down_price = 1.0 - up_price
            else:
                down_price = 0.80 + rng.uniform(0, 0.15)
                up_price = 1.0 - down_price

        elif abs_move > 0.3 and scenario < 0.80:
            reversed_dir = "DOWN" if direction == "UP" else "UP"
            stale_price = max(0.01, 0.04)
            if direction == "UP":
                up_price = stale_price
                down_price = 1.0 - stale_price
            else:
                down_price = stale_price
                up_price = 1.0 - down_price
            direction = reversed_dir

        else:
            up_price = 0.45 + rng.uniform(0, 0.10)
            down_price = 1.0 - up_price

        windows.append(SimulatedWindow(
            market_id=f"synthetic_{i:04d}",
            resolved_direction=direction,
            price_move_pct=move,
            up_price=round(max(0.01, up_price), 3),
            down_price=round(max(0.01, down_price), 3),
        ))

    return windows


def _resolve_mode(explicit: str | None) -> str:
    """Pick mode: use explicit flag, else 'real' if DB exists, else 'synthetic'."""
    if explicit:
        return explicit
    if DB_PATH.exists():
        return "real"
    return "synthetic"


def _print_result(result: BacktestResult, label: str, as_json: bool) -> None:
    if as_json:
        print(json.dumps({
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
        }))
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


def main() -> None:
    parser = argparse.ArgumentParser(description="Run BTC sniper backtest")
    parser.add_argument("--config", required=True, help="Path to strategy_config.json")
    parser.add_argument("--mode", choices=["real", "synthetic"], default=None,
                        help="Data mode (default: real if historical.db exists)")
    parser.add_argument("--windows", type=int, default=200, help="Number of synthetic windows")
    parser.add_argument("--seed", type=int, default=42, help="Random seed for reproducibility")
    parser.add_argument("--balance", type=float, default=100.0, help="Starting balance")
    parser.add_argument("--json", action="store_true", help="Output as JSON")
    args = parser.parse_args()

    cfg = load_strategy_config(args.config)
    mode = _resolve_mode(args.mode)

    if mode == "real":
        real_windows = load_real_windows()
        if not real_windows:
            print("No real data found in historical.db, falling back to synthetic",
                  file=sys.stderr)
            mode = "synthetic"
        else:
            result = run_backtest_real(cfg, real_windows, starting_balance=args.balance)
            _print_result(result, f"real, {len(real_windows)} windows", args.json)
            return

    windows = generate_synthetic_windows(args.windows, args.seed)
    result = run_backtest(cfg, windows, starting_balance=args.balance)
    _print_result(result, f"synthetic, {args.windows} windows", args.json)


if __name__ == "__main__":
    main()
