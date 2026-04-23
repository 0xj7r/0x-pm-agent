"""Sync local SQLite market/snapshot data to Supabase."""
from __future__ import annotations

import argparse
import json
from pathlib import Path

from shared.constants import COINS, db_path
from shared.db import get_connection
from shared.supabase_client import SupabaseClient


def sync_coin(
    coin: str,
    upload_results: bool = True,
    markets_only: bool = False,
) -> None:
    client = SupabaseClient()
    conn = get_connection(db_path(coin))

    markets = [
        {
            "market_id": row["market_id"],
            "coin": coin,
            "slug": row["slug"],
            "market_type": row["market_type"],
            "start_time": row["start_time"],
            "end_time": row["end_time"],
            "price_start": row["price_start"],
            "price_end": row["price_end"],
            "winner": row["winner"],
            "final_volume": row["final_volume"],
            "final_liquidity": row["final_liquidity"],
        }
        for row in conn.execute("SELECT * FROM markets ORDER BY start_time").fetchall()
    ]
    client.upsert_markets(markets)

    if not markets_only:
        offset = 0
        batch_size = 1000
        while True:
            rows = conn.execute(
                "SELECT * FROM snapshots ORDER BY market_id, time LIMIT ? OFFSET ?",
                (batch_size, offset),
            ).fetchall()
            if not rows:
                break
            client.upsert_snapshots([dict(row) for row in rows])
            offset += batch_size

    if upload_results and not markets_only:
        results_path = Path("backtesting/strategy_results.json")
        if results_path.exists():
            data = json.loads(results_path.read_text())
            if coin in data:
                best = data[coin]["best_strategy"]
                client.upsert_strategy_result(
                    {
                        "coin": coin,
                        "strategy_name": best["name"],
                        "params": best["params"],
                        "train_win_rate": best["train_win_rate"],
                        "test_win_rate": best["test_win_rate"],
                        "total_trades": best["train_trades"] + best["test_trades"],
                    }
                )

    conn.close()
    client.close()


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--coin", action="append", choices=COINS, required=True)
    parser.add_argument("--skip-results", action="store_true")
    parser.add_argument("--markets-only", action="store_true")
    args = parser.parse_args()
    for coin in args.coin:
        sync_coin(
            coin,
            upload_results=not args.skip_results,
            markets_only=args.markets_only,
        )


if __name__ == "__main__":
    main()
