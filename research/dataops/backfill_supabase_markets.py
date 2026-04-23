"""Backfill Supabase markets using PolyBackTest market headers.

This is intended to repair an empty or incomplete Supabase `markets` table
without re-uploading the much larger snapshots table.
"""
from __future__ import annotations

import argparse
import json
import logging
from typing import Any

import httpx

from backtesting.data.fetcher import CoinDataFetcher
from shared.constants import COINS
from shared.supabase_client import SupabaseClient

logger = logging.getLogger(__name__)


def market_row_from_header(coin: str, header: dict[str, Any]) -> dict[str, Any]:
    return {
        "market_id": header["market_id"],
        "coin": coin,
        "slug": header.get("slug", ""),
        "market_type": header.get("market_type", ""),
        "start_time": header.get("start_time", ""),
        "end_time": header.get("end_time", ""),
        "price_start": header.get("btc_price_start")
        or header.get(f"{coin}_price_start"),
        "price_end": header.get("btc_price_end") or header.get(f"{coin}_price_end"),
        "winner": header.get("winner"),
        "final_volume": header.get("final_volume"),
        "final_liquidity": header.get("final_liquidity"),
    }


def backfill_coin_markets(coin: str, limit: int) -> dict[str, Any]:
    fetcher = CoinDataFetcher(coin)
    client = SupabaseClient()
    try:
        with httpx.Client(timeout=15) as http_client:
            headers = fetcher.fetch_headers(http_client, limit=limit)
        market_rows = [market_row_from_header(coin, header) for header in headers]
        client.upsert_markets(market_rows)
        return {
            "coin": coin,
            "fetched_headers": len(headers),
            "upserted_rows": len(market_rows),
            "first_start_time": market_rows[0]["start_time"] if market_rows else None,
            "last_start_time": market_rows[-1]["start_time"] if market_rows else None,
        }
    finally:
        client.close()


def main() -> None:
    logging.basicConfig(level=logging.INFO, format="%(asctime)s %(levelname)s %(message)s")
    parser = argparse.ArgumentParser()
    parser.add_argument("--coin", action="append", choices=COINS, default=[])
    parser.add_argument("--limit", type=int, default=9000)
    args = parser.parse_args()

    selected_coins = args.coin or ["btc", "eth"]
    summary = {
        "coins": [],
        "limit": args.limit,
    }
    for coin in selected_coins:
        logger.info("Backfilling Supabase markets for %s", coin.upper())
        summary["coins"].append(backfill_coin_markets(coin, args.limit))

    print(json.dumps(summary, indent=2))


if __name__ == "__main__":
    main()
