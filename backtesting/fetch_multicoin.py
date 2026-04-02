"""Fetch ETH and SOL market data from PolyBackTest.

Uses the same schema as BTC, with a coin column to distinguish.

Usage:
    python backtesting/fetch_multicoin.py --coin eth --limit 500
    python backtesting/fetch_multicoin.py --coin sol --limit 500
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

logger = logging.getLogger(__name__)

DB_PATH = Path(__file__).parent / "historical.db"
API_BASE = "https://api.polybacktest.com"
API_KEY = "***POLYBACKTEST_KEY_REMOVED***"
RATE_LIMIT = 0.25


def _headers():
    return {"X-API-Key": API_KEY}


def fetch_and_store(coin: str, market_type: str, limit: int, db_path: Path):
    conn = sqlite3.connect(str(db_path))
    conn.execute("PRAGMA journal_mode=WAL")

    existing = {
        r[0] for r in conn.execute("SELECT market_id FROM markets").fetchall()
    }

    price_col = f"{coin}_price"  # snapshots use btc_price, eth_price, sol_price

    with httpx.Client() as client:
        # Phase 1: fetch market headers
        logger.info(f"Fetching {coin.upper()} {market_type} market headers...")
        all_markets = []
        offset = 0
        while len(all_markets) < limit:
            batch_size = min(limit - len(all_markets), 50)
            resp = client.get(
                f"{API_BASE}/v2/markets", headers=_headers(),
                params={"coin": coin, "market_type": market_type,
                        "limit": batch_size, "offset": offset},
                timeout=15,
            )
            if resp.status_code != 200:
                logger.warning(f"HTTP {resp.status_code} at offset {offset}")
                break
            batch = resp.json().get("markets", [])
            if not batch:
                break
            resolved = [m for m in batch if m.get("winner")]
            all_markets.extend(resolved)
            offset += batch_size
            time.sleep(RATE_LIMIT)

        new_markets = [m for m in all_markets if m["market_id"] not in existing]
        logger.info(f"Found {len(all_markets)} resolved, {len(new_markets)} new")

        # Filter: only fetch snapshots for markets with meaningful price move
        qualifying = []
        for m in new_markets:
            start = m.get("btc_price_start") or m.get(f"{coin}_price_start", 0)
            end = m.get("btc_price_end") or m.get(f"{coin}_price_end", 0)
            if start and end and start > 0:
                move = abs((end - start) / start * 100)
                if move >= 0.06:
                    qualifying.append(m)

        # Store all headers
        for m in new_markets:
            start_p = m.get("btc_price_start") or m.get(f"{coin}_price_start")
            end_p = m.get("btc_price_end") or m.get(f"{coin}_price_end")
            conn.execute(
                """INSERT OR REPLACE INTO markets
                   (market_id, slug, market_type, start_time, end_time,
                    btc_price_start, btc_price_end, winner,
                    final_volume, final_liquidity, coin)
                   VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)""",
                (m["market_id"], m.get("slug", ""), m.get("market_type", ""),
                 m.get("start_time", ""), m.get("end_time", ""),
                 start_p, end_p,
                 m.get("winner"), m.get("final_volume"), m.get("final_liquidity"),
                 coin),
            )
        conn.commit()
        logger.info(f"Stored {len(new_markets)} headers, {len(qualifying)} qualify for snapshots")

        # Phase 2: fetch snapshots for qualifying markets
        total_snaps = 0
        for i, m in enumerate(qualifying):
            snap_offset = 0
            while True:
                resp = client.get(
                    f"{API_BASE}/v2/markets/{m['market_id']}/snapshots",
                    headers=_headers(),
                    params={"coin": coin, "limit": 500, "offset": snap_offset},
                    timeout=15,
                )
                if resp.status_code != 200:
                    break
                batch = resp.json().get("snapshots", [])
                if not batch:
                    break
                for s in batch:
                    price = s.get("btc_price") or s.get(f"{coin}_price", 0)
                    conn.execute(
                        """INSERT OR REPLACE INTO snapshots
                           (market_id, time, btc_price, price_up, price_down)
                           VALUES (?, ?, ?, ?, ?)""",
                        (m["market_id"], s.get("time", ""),
                         price, s.get("price_up"), s.get("price_down")),
                    )
                    total_snaps += 1
                if len(batch) < 500:
                    break
                snap_offset += 500
                time.sleep(RATE_LIMIT)

            if (i + 1) % 10 == 0:
                conn.commit()
                logger.info(f"  [{i+1}/{len(qualifying)}] {total_snaps} snapshots")
            time.sleep(RATE_LIMIT)

    conn.commit()
    conn.close()
    logger.info(f"Done: {len(qualifying)} markets with snapshots, {total_snaps} total")


def main():
    logging.basicConfig(level=logging.INFO, format="%(asctime)s %(levelname)s %(message)s")
    parser = argparse.ArgumentParser()
    parser.add_argument("--coin", type=str, required=True, choices=["eth", "sol"])
    parser.add_argument("--limit", type=int, default=500)
    parser.add_argument("--type", type=str, default="5m")
    parser.add_argument("--db", type=str, default=str(DB_PATH))
    args = parser.parse_args()
    fetch_and_store(args.coin, args.type, args.limit, Path(args.db))


if __name__ == "__main__":
    main()
