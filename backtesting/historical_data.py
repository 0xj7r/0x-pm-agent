"""Fetch historical BTC 5-min market data from PolyBackTest API.

Stores resolved markets with sub-second token price snapshots
in SQLite for backtesting with real order book data.

Usage:
    python backtesting/historical_data.py --limit 50
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
API_KEY = "pdm_CSreRRkeODfhuBa7rbTqZmnyTliLtixT"
RATE_LIMIT_DELAY = 0.25  # paid tier


def init_db(db_path: Path = DB_PATH) -> sqlite3.Connection:
    conn = sqlite3.connect(str(db_path))
    conn.execute("PRAGMA journal_mode=WAL")
    conn.execute("""
        CREATE TABLE IF NOT EXISTS markets (
            market_id TEXT PRIMARY KEY,
            slug TEXT,
            market_type TEXT,
            start_time TEXT,
            end_time TEXT,
            btc_price_start REAL,
            btc_price_end REAL,
            winner TEXT,
            final_volume REAL,
            final_liquidity REAL
        )
    """)
    conn.execute("""
        CREATE TABLE IF NOT EXISTS snapshots (
            market_id TEXT,
            time TEXT,
            btc_price REAL,
            price_up REAL,
            price_down REAL,
            PRIMARY KEY (market_id, time)
        )
    """)
    conn.commit()
    return conn


def _headers() -> dict[str, str]:
    return {"X-API-Key": API_KEY}


def fetch_markets(
    client: httpx.Client,
    market_type: str = "5m",
    limit: int = 50,
) -> list[dict]:
    """Fetch resolved BTC markets from PolyBackTest."""
    all_markets = []
    offset = 0
    while len(all_markets) < limit:
        batch_size = min(limit - len(all_markets), 50)
        resp = client.get(
            f"{API_BASE}/v2/markets",
            headers=_headers(),
            params={"coin": "btc", "market_type": market_type, "limit": batch_size, "offset": offset},
            timeout=15,
        )
        if resp.status_code == 402:
            logger.warning("Hit free tier limit at offset %d, returning %d markets", offset, len(all_markets))
            break
        resp.raise_for_status()
        data = resp.json()
        batch = data.get("markets", [])
        if not batch:
            break
        # Only keep resolved markets
        resolved = [m for m in batch if m.get("winner")]
        all_markets.extend(resolved)
        offset += batch_size
        time.sleep(RATE_LIMIT_DELAY)
    return all_markets[:limit]


def fetch_snapshots(
    client: httpx.Client,
    market_id: str,
) -> list[dict]:
    """Fetch all sub-second snapshots for a market."""
    all_snaps = []
    offset = 0
    while True:
        resp = client.get(
            f"{API_BASE}/v2/markets/{market_id}/snapshots",
            headers=_headers(),
            params={"coin": "btc", "limit": 500, "offset": offset},
            timeout=15,
        )
        resp.raise_for_status()
        data = resp.json()
        batch = data.get("snapshots", [])
        if not batch:
            break
        all_snaps.extend(batch)
        if len(batch) < 500:
            break
        offset += 500
        time.sleep(RATE_LIMIT_DELAY)
    return all_snaps


def fetch_all(
    limit: int = 50,
    market_type: str = "5m",
    db_path: Path = DB_PATH,
    progress: bool = True,
) -> tuple[int, int]:
    """Fetch markets and snapshots from PolyBackTest. Returns (markets, snapshots)."""
    conn = init_db(db_path)

    existing = {
        r[0] for r in conn.execute("SELECT market_id FROM markets").fetchall()
    }

    total_markets = 0
    total_snaps = 0

    with httpx.Client() as client:
        if progress:
            logger.info(f"Fetching up to {limit} resolved {market_type} BTC markets...")

        markets = fetch_markets(client, market_type=market_type, limit=limit)
        new_markets = [m for m in markets if m["market_id"] not in existing]

        if progress:
            logger.info(f"Found {len(markets)} markets, {len(new_markets)} new")

        for i, m in enumerate(new_markets):
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
            total_markets += 1

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

            if progress and (i + 1) % 5 == 0:
                conn.commit()
                logger.info(f"  [{i+1}/{len(new_markets)}] {total_markets} markets, {total_snaps} snapshots")

            time.sleep(RATE_LIMIT_DELAY)

    conn.commit()
    conn.close()

    if progress:
        logger.info(f"Done: {total_markets} markets, {total_snaps} snapshots stored")
    return total_markets, total_snaps


def load_markets(db_path: Path = DB_PATH) -> list[dict]:
    """Load all resolved markets from DB."""
    conn = sqlite3.connect(str(db_path))
    conn.row_factory = sqlite3.Row
    rows = conn.execute("SELECT * FROM markets WHERE winner IS NOT NULL ORDER BY start_time").fetchall()
    result = [dict(r) for r in rows]
    conn.close()
    return result


def load_snapshots(market_id: str, db_path: Path = DB_PATH) -> list[dict]:
    """Load snapshots for a specific market, time-ordered."""
    conn = sqlite3.connect(str(db_path))
    conn.row_factory = sqlite3.Row
    rows = conn.execute(
        "SELECT * FROM snapshots WHERE market_id = ? ORDER BY time",
        (market_id,),
    ).fetchall()
    result = [dict(r) for r in rows]
    conn.close()
    return result


def main() -> None:
    logging.basicConfig(level=logging.INFO, format="%(asctime)s %(levelname)s %(message)s")
    parser = argparse.ArgumentParser(description="Fetch BTC market data from PolyBackTest")
    parser.add_argument("--limit", type=int, default=50, help="Number of markets to fetch")
    parser.add_argument("--type", type=str, default="5m", help="Market type (5m, 15m, 1h)")
    parser.add_argument("--db", type=str, default=str(DB_PATH), help="Database path")
    args = parser.parse_args()
    fetch_all(limit=args.limit, market_type=args.type, db_path=Path(args.db))


if __name__ == "__main__":
    main()
