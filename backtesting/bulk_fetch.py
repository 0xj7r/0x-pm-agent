"""Fast bulk fetcher: pulls all market headers, then snapshots only for qualifying markets.

Markets qualify if the BTC move from start to end exceeds the threshold,
since only those could have triggered the strategy.

Usage:
    python backtesting/bulk_fetch.py --threshold 0.06
"""
from __future__ import annotations

import argparse
import logging
import sqlite3
import sys
import time
from pathlib import Path

import httpx

sys.path.insert(0, str(Path(__file__).parent.parent))
from backtesting.historical_data import init_db, _headers, fetch_snapshots, DB_PATH, API_BASE

logger = logging.getLogger(__name__)

RATE_LIMIT = 0.25


def fetch_all_market_headers(
    client: httpx.Client,
    market_type: str = "5m",
) -> list[dict]:
    """Fetch ALL market headers (no snapshots) via pagination."""
    all_markets = []
    offset = 0
    while True:
        resp = client.get(
            f"{API_BASE}/v2/markets",
            headers=_headers(),
            params={"coin": "btc", "market_type": market_type, "limit": 50, "offset": offset},
            timeout=15,
        )
        if resp.status_code != 200:
            logger.warning(f"HTTP {resp.status_code} at offset {offset}, stopping")
            break
        batch = resp.json().get("markets", [])
        if not batch:
            break
        resolved = [m for m in batch if m.get("winner")]
        all_markets.extend(resolved)
        if len(batch) < 50:
            break
        offset += 50
        time.sleep(RATE_LIMIT)
        if offset % 500 == 0:
            logger.info(f"  Fetched {offset} market headers ({len(all_markets)} resolved)...")
    return all_markets


def bulk_fetch(
    move_threshold: float = 0.06,
    market_type: str = "5m",
    db_path: Path = DB_PATH,
) -> tuple[int, int]:
    conn = init_db(db_path)

    existing = {
        r[0] for r in conn.execute("SELECT market_id FROM markets").fetchall()
    }

    with httpx.Client() as client:
        logger.info("Phase 1: Fetching all market headers...")
        all_markets = fetch_all_market_headers(client, market_type)
        logger.info(f"Found {len(all_markets)} resolved markets total")

        new_markets = [m for m in all_markets if m["market_id"] not in existing]
        logger.info(f"{len(new_markets)} new markets to process")

        qualifying = []
        for m in new_markets:
            btc_start = m.get("btc_price_start")
            btc_end = m.get("btc_price_end")
            if btc_start and btc_end and btc_start > 0:
                move = abs((btc_end - btc_start) / btc_start * 100)
                if move >= move_threshold:
                    qualifying.append(m)

        logger.info(f"{len(qualifying)} markets have BTC move >= {move_threshold}% "
                    f"(need snapshots)")

        non_qualifying = [m for m in new_markets if m not in qualifying]

        # Store non-qualifying markets (headers only, no snapshots needed)
        for m in non_qualifying:
            conn.execute(
                """INSERT OR REPLACE INTO markets
                   (market_id, slug, market_type, start_time, end_time,
                    btc_price_start, btc_price_end, winner,
                    final_volume, final_liquidity)
                   VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)""",
                (m["market_id"], m.get("slug", ""), m.get("market_type", ""),
                 m.get("start_time", ""), m.get("end_time", ""),
                 m.get("btc_price_start"), m.get("btc_price_end"),
                 m.get("winner"), m.get("final_volume"), m.get("final_liquidity")),
            )
        conn.commit()
        logger.info(f"Stored {len(non_qualifying)} non-qualifying market headers")

        # Fetch snapshots for qualifying markets
        logger.info(f"Phase 2: Fetching snapshots for {len(qualifying)} qualifying markets...")
        total_snaps = 0
        for i, m in enumerate(qualifying):
            conn.execute(
                """INSERT OR REPLACE INTO markets
                   (market_id, slug, market_type, start_time, end_time,
                    btc_price_start, btc_price_end, winner,
                    final_volume, final_liquidity)
                   VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)""",
                (m["market_id"], m.get("slug", ""), m.get("market_type", ""),
                 m.get("start_time", ""), m.get("end_time", ""),
                 m.get("btc_price_start"), m.get("btc_price_end"),
                 m.get("winner"), m.get("final_volume"), m.get("final_liquidity")),
            )

            snaps = fetch_snapshots(client, m["market_id"])
            for s in snaps:
                conn.execute(
                    """INSERT OR REPLACE INTO snapshots
                       (market_id, time, btc_price, price_up, price_down)
                       VALUES (?, ?, ?, ?, ?)""",
                    (m["market_id"], s.get("time", ""),
                     s.get("btc_price"), s.get("price_up"), s.get("price_down")),
                )
                total_snaps += 1

            if (i + 1) % 10 == 0:
                conn.commit()
                logger.info(f"  [{i+1}/{len(qualifying)}] {total_snaps} snapshots")

            time.sleep(RATE_LIMIT)

    conn.commit()
    total_new = len(non_qualifying) + len(qualifying)
    logger.info(f"Done: {total_new} new markets, {total_snaps} snapshots")
    conn.close()
    return total_new, total_snaps


def main() -> None:
    logging.basicConfig(level=logging.INFO, format="%(asctime)s %(levelname)s %(message)s")
    parser = argparse.ArgumentParser()
    parser.add_argument("--threshold", type=float, default=0.06,
                        help="Min BTC move %% to fetch snapshots (use < strategy threshold)")
    parser.add_argument("--type", type=str, default="5m")
    parser.add_argument("--db", type=str, default=str(DB_PATH))
    args = parser.parse_args()
    bulk_fetch(args.threshold, args.type, Path(args.db))


if __name__ == "__main__":
    main()
