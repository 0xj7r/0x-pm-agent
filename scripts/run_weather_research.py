"""Run a weather-specific research sweep against the weather SQLite store."""

from __future__ import annotations

import argparse
import json
from datetime import datetime, timezone
from pathlib import Path

from backtesting.weather_db import load_research_rows
from shared.constants import WEATHER_DB_PATH
from strategies.weather import WeatherStrategyParams, evaluate_weather_trade

BASE_DIR = Path(__file__).resolve().parent.parent
REPORTS_DIR = BASE_DIR / "autoresearch" / "reports"


def _dedupe_latest(rows: list[dict]) -> list[dict]:
    latest: dict[str, dict] = {}
    for row in rows:
        market_id = str(row["market_id"])
        if row["observed_temp_c"] is None:
            continue
        current = latest.get(market_id)
        if current is None or str(row["as_of"]) > str(current["as_of"]):
            latest[market_id] = row
    deduped = list(latest.values())
    deduped.sort(key=lambda row: (row["target_date"], row["market_id"]))
    return deduped


def _simulate(rows: list[dict], params: WeatherStrategyParams) -> dict:
    trades = [evaluate_weather_trade(row, params) for row in rows]
    executed = [trade for trade in trades if trade is not None]
    wins = sum(1 for trade in executed if trade.won)
    pnl = sum(trade.pnl for trade in executed)
    avg_edge = sum(trade.edge for trade in executed) / len(executed) if executed else 0.0
    avg_stake = sum(trade.stake_fraction for trade in executed) / len(executed) if executed else 0.0
    return {
        "trades": len(executed),
        "wins": wins,
        "win_rate": wins / len(executed) if executed else 0.0,
        "pnl": pnl,
        "pnl_per_trade": pnl / len(executed) if executed else 0.0,
        "avg_edge": avg_edge,
        "avg_stake_fraction": avg_stake,
    }


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--db", type=Path, default=WEATHER_DB_PATH)
    parser.add_argument("--min-trades", type=int, default=10)
    args = parser.parse_args()

    rows = _dedupe_latest(load_research_rows(args.db))
    split = max(1, int(len(rows) * 0.7))
    train = rows[:split]
    test = rows[split:]

    min_edge_grid = (0.05, 0.08, 0.10, 0.12)
    max_entry_grid = (0.45, 0.55, 0.65)
    min_consensus_grid = (0.60, 0.67, 0.75)
    kelly_fraction_grid = (0.10, 0.25, 0.50)

    results = []
    for min_edge in min_edge_grid:
        for max_entry in max_entry_grid:
            for min_consensus in min_consensus_grid:
                for kelly_fraction in kelly_fraction_grid:
                    params = WeatherStrategyParams(
                        min_edge=min_edge,
                        max_entry=max_entry,
                        min_consensus=min_consensus,
                        kelly_fraction=kelly_fraction,
                    )
                    train_stats = _simulate(train, params)
                    test_stats = _simulate(test, params)
                    if train_stats["trades"] < args.min_trades or test_stats["trades"] < max(3, args.min_trades // 3):
                        continue
                    if train_stats["pnl"] <= 0 or test_stats["pnl"] <= 0:
                        continue
                    results.append(
                        {
                            "params": {
                                "min_edge": min_edge,
                                "max_entry": max_entry,
                                "min_consensus": min_consensus,
                                "kelly_fraction": kelly_fraction,
                            },
                            "train": train_stats,
                            "test": test_stats,
                            "score": test_stats["pnl_per_trade"] + test_stats["win_rate"],
                        }
                    )

    results.sort(key=lambda row: row["score"], reverse=True)
    report = {
        "created_at": datetime.now(timezone.utc).isoformat(),
        "dataset": {
            "db_path": str(args.db),
            "markets_with_actuals": len(rows),
            "train_markets": len(train),
            "test_markets": len(test),
        },
        "best": results[0] if results else None,
        "top_10": results[:10],
    }

    REPORTS_DIR.mkdir(parents=True, exist_ok=True)
    stamp = datetime.now(timezone.utc).strftime("%Y%m%dT%H%M%SZ")
    report_path = REPORTS_DIR / f"{stamp}-weather.json"
    report_path.write_text(json.dumps(report, indent=2))
    print(json.dumps({"report_path": str(report_path), "candidates": len(results)}, indent=2))


if __name__ == "__main__":
    main()
