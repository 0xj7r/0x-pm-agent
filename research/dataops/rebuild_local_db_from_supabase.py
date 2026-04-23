"""Rebuild local coin SQLite databases from Supabase markets and snapshots."""
from __future__ import annotations

import argparse
import json
import logging
from pathlib import Path

from shared.constants import COINS, db_path
from shared.db import init_coin_db
from shared.supabase_client import SupabaseClient

logger = logging.getLogger(__name__)
SUPABASE_PAGE_LIMIT = 1000


def _replace_db_from_temp(temp_path: Path, final_path: Path) -> None:
    if final_path.exists():
        final_path.unlink()
    temp_path.replace(final_path)


def rebuild_coin_db_from_supabase(
    coin: str,
    batch_size: int = 5000,
) -> dict[str, int | str]:
    client = SupabaseClient()
    final_path = db_path(coin)
    temp_path = final_path.with_suffix(".supabase.tmp.db")
    if temp_path.exists():
        temp_path.unlink()

    effective_batch_size = min(batch_size, SUPABASE_PAGE_LIMIT)
    conn = init_coin_db(temp_path)
    replaced = False
    try:
        markets = client.load_markets(coin)
        conn.executemany(
            """
            INSERT OR REPLACE INTO markets (
                market_id, slug, market_type, start_time, end_time,
                price_start, price_end, winner, final_volume, final_liquidity
            ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
            """,
            [
                (
                    row["market_id"],
                    row.get("slug", ""),
                    row.get("market_type", ""),
                    row.get("start_time", ""),
                    row.get("end_time", ""),
                    row.get("price_start"),
                    row.get("price_end"),
                    row.get("winner"),
                    row.get("final_volume"),
                    row.get("final_liquidity"),
                )
                for row in markets
            ],
        )
        conn.commit()

        snapshot_rows = 0
        offset = 0
        while True:
            batch = client.load_coin_snapshots_page(
                coin,
                limit=effective_batch_size,
                offset=offset,
            )
            if not batch:
                break
            conn.executemany(
                """
                INSERT OR REPLACE INTO snapshots (
                    market_id, time, price, price_up, price_down
                ) VALUES (?, ?, ?, ?, ?)
                """,
                [
                    (
                        row["market_id"],
                        row["time"],
                        row.get("price"),
                        row.get("price_up"),
                        row.get("price_down"),
                    )
                    for row in batch
                ],
            )
            conn.commit()
            snapshot_rows += len(batch)
            offset += len(batch)
            logger.info("[%s] snapshots synced: %s", coin.upper(), snapshot_rows)
            if len(batch) < effective_batch_size:
                break

        conn.close()
        _replace_db_from_temp(temp_path, final_path)
        replaced = True
        return {
            "coin": coin,
            "markets": len(markets),
            "snapshots": snapshot_rows,
            "db_path": str(final_path),
        }
    finally:
        if not replaced:
            conn.close()
        client.close()
        if temp_path.exists():
            temp_path.unlink()


def main() -> None:
    logging.basicConfig(level=logging.INFO, format="%(asctime)s %(levelname)s %(message)s")
    parser = argparse.ArgumentParser()
    parser.add_argument("--coin", action="append", choices=COINS, default=[])
    parser.add_argument("--batch-size", type=int, default=1000)
    args = parser.parse_args()

    selected_coins = args.coin or ["btc", "eth"]
    summary = {
        "coins": [
            rebuild_coin_db_from_supabase(coin, batch_size=args.batch_size)
            for coin in selected_coins
        ]
    }
    print(json.dumps(summary, indent=2))


if __name__ == "__main__":
    main()
