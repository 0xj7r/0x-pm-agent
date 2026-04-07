"""Replay a chosen weather strategy configuration against the weather dataset."""

from __future__ import annotations

import argparse
import json
from pathlib import Path

from backtesting.weather_db import load_research_rows
from shared.constants import WEATHER_DB_PATH
from strategies.weather import WeatherStrategyParams, evaluate_weather_trade


def _latest_rows(rows: list[dict]) -> list[dict]:
    latest: dict[str, dict] = {}
    for row in rows:
        if row["observed_temp_c"] is None:
            continue
        market_id = str(row["market_id"])
        current = latest.get(market_id)
        if current is None or str(row["as_of"]) > str(current["as_of"]):
            latest[market_id] = row
    return sorted(latest.values(), key=lambda row: (row["target_date"], row["market_id"]))


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--db", type=Path, default=WEATHER_DB_PATH)
    parser.add_argument("--min-edge", type=float, required=True)
    parser.add_argument("--max-entry", type=float, required=True)
    parser.add_argument("--min-consensus", type=float, required=True)
    parser.add_argument("--kelly-fraction", type=float, default=0.25)
    parser.add_argument("--max-bankroll-pct", type=float, default=0.05)
    args = parser.parse_args()

    params = WeatherStrategyParams(
        min_edge=args.min_edge,
        max_entry=args.max_entry,
        min_consensus=args.min_consensus,
        kelly_fraction=args.kelly_fraction,
        max_bankroll_pct=args.max_bankroll_pct,
    )
    rows = _latest_rows(load_research_rows(args.db))
    trades = [trade for row in rows if (trade := evaluate_weather_trade(row, params)) is not None]
    wins = sum(1 for trade in trades if trade.won)
    by_side = {"YES": 0, "NO": 0}
    for trade in trades:
        by_side[trade.side] += 1

    payload = {
        "dataset_markets": len(rows),
        "trades": len(trades),
        "wins": wins,
        "win_rate": wins / len(trades) if trades else 0.0,
        "pnl": sum(trade.pnl for trade in trades),
        "avg_pnl_per_trade": sum(trade.pnl for trade in trades) / len(trades) if trades else 0.0,
        "avg_edge": sum(trade.edge for trade in trades) / len(trades) if trades else 0.0,
        "avg_stake_fraction": sum(trade.stake_fraction for trade in trades) / len(trades) if trades else 0.0,
        "by_side": by_side,
        "params": {
            "min_edge": args.min_edge,
            "max_entry": args.max_entry,
            "min_consensus": args.min_consensus,
            "kelly_fraction": args.kelly_fraction,
            "max_bankroll_pct": args.max_bankroll_pct,
        },
    }
    print(json.dumps(payload, indent=2))


if __name__ == "__main__":
    main()
