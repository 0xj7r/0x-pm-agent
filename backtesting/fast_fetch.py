"""Fast parallel data fetcher. Separate DB per coin, no lock contention.

Usage:
    python backtesting/fast_fetch.py --coin btc --limit 9000
    python backtesting/fast_fetch.py --coin eth --limit 8000
    python backtesting/fast_fetch.py --coin sol --limit 5000

Run all three in parallel (separate processes):
    python backtesting/fast_fetch.py --coin btc --limit 9000 &
    python backtesting/fast_fetch.py --coin eth --limit 8000 &
    python backtesting/fast_fetch.py --coin sol --limit 5000 &
"""
from __future__ import annotations

import argparse
import logging
import sqlite3
import time
from pathlib import Path

import httpx

logger = logging.getLogger(__name__)

BASE_DIR = Path(__file__).parent
API_BASE = "https://api.polybacktest.com"
API_KEYS = {
    "btc": "***POLYBACKTEST_KEY_REMOVED***",
    "eth": "***POLYBACKTEST_KEY_REMOVED***",
    "sol": "***POLYBACKTEST_KEY_REMOVED***",
}
RATE_LIMIT = 0.20


def db_path_for(coin: str) -> Path:
    return BASE_DIR / f"{coin}.db"


def init_db(path: Path) -> sqlite3.Connection:
    conn = sqlite3.connect(str(path))
    conn.execute("PRAGMA journal_mode=WAL")
    conn.execute("PRAGMA synchronous=NORMAL")
    conn.execute("""
        CREATE TABLE IF NOT EXISTS markets (
            market_id TEXT PRIMARY KEY,
            slug TEXT, market_type TEXT,
            start_time TEXT, end_time TEXT,
            price_start REAL, price_end REAL,
            winner TEXT, final_volume REAL, final_liquidity REAL
        )
    """)
    conn.execute("""
        CREATE TABLE IF NOT EXISTS snapshots (
            market_id TEXT, time TEXT,
            price REAL, price_up REAL, price_down REAL,
            PRIMARY KEY (market_id, time)
        )
    """)
    conn.commit()
    return conn


def headers(coin: str = "btc"):
    return {"X-API-Key": API_KEYS.get(coin, API_KEYS["btc"])}


def api_get(client: httpx.Client, url: str, coin: str, **kwargs) -> httpx.Response:
    """GET with retry on 429."""
    for attempt in range(5):
        resp = client.get(url, headers=headers(coin), **kwargs)
        if resp.status_code == 429:
            wait = 2 ** attempt
            logger.warning(f"[{coin.upper()}] 429, waiting {wait}s...")
            time.sleep(wait)
            continue
        return resp
    return resp


def fetch_all(coin: str, market_type: str, limit: int, move_threshold: float):
    db = db_path_for(coin)
    conn = init_db(db)

    existing = {r[0] for r in conn.execute("SELECT market_id FROM markets").fetchall()}
    existing_snaps = {r[0] for r in conn.execute("SELECT DISTINCT market_id FROM snapshots").fetchall()}

    with httpx.Client(timeout=15) as client:
        # Phase 1: all market headers
        logger.info(f"[{coin.upper()}] Fetching market headers (limit={limit})...")
        all_markets = []
        offset = 0
        while len(all_markets) < limit:
            batch = min(limit - len(all_markets), 50)
            resp = api_get(client, f"{API_BASE}/v2/markets", coin,
                          params={"coin": coin, "market_type": market_type,
                                  "limit": batch, "offset": offset})
            if resp.status_code != 200:
                logger.warning(f"[{coin.upper()}] HTTP {resp.status_code} at offset {offset}")
                break
            markets = resp.json().get("markets", [])
            if not markets:
                break
            resolved = [m for m in markets if m.get("winner")]
            all_markets.extend(resolved)
            if len(markets) < batch:
                break
            offset += batch
            time.sleep(RATE_LIMIT)
            if offset % 500 == 0:
                logger.info(f"[{coin.upper()}] Headers: {len(all_markets)} resolved at offset {offset}")

        logger.info(f"[{coin.upper()}] Got {len(all_markets)} resolved markets")

        # Store headers and identify which need snapshots
        new_markets = [m for m in all_markets if m["market_id"] not in existing]
        need_snaps = []

        for m in new_markets:
            ps = m.get("btc_price_start") or m.get(f"{coin}_price_start", 0)
            pe = m.get("btc_price_end") or m.get(f"{coin}_price_end", 0)
            conn.execute(
                "INSERT OR IGNORE INTO markets VALUES (?,?,?,?,?,?,?,?,?,?)",
                (m["market_id"], m.get("slug", ""), m.get("market_type", ""),
                 m.get("start_time", ""), m.get("end_time", ""),
                 ps, pe, m.get("winner"),
                 m.get("final_volume"), m.get("final_liquidity")),
            )
            if ps and pe and ps > 0 and abs((pe - ps) / ps * 100) >= move_threshold:
                if m["market_id"] not in existing_snaps:
                    need_snaps.append(m)

        conn.commit()
        logger.info(f"[{coin.upper()}] {len(new_markets)} new headers stored, "
                    f"{len(need_snaps)} need snapshots (>{move_threshold}% move)")

        # Phase 2: snapshots for qualifying markets
        total_snaps = 0
        for i, m in enumerate(need_snaps):
            snap_offset = 0
            while True:
                resp = api_get(client,
                    f"{API_BASE}/v2/markets/{m['market_id']}/snapshots", coin,
                    params={"coin": coin, "limit": 500, "offset": snap_offset})
                if resp.status_code != 200:
                    break
                batch = resp.json().get("snapshots", [])
                if not batch:
                    break
                rows = []
                for s in batch:
                    p = s.get("btc_price") or s.get(f"{coin}_price", 0)
                    rows.append((m["market_id"], s.get("time", ""),
                                p, s.get("price_up"), s.get("price_down")))
                conn.executemany(
                    "INSERT OR IGNORE INTO snapshots VALUES (?,?,?,?,?)", rows)
                total_snaps += len(rows)
                if len(batch) < 500:
                    break
                snap_offset += 500
                time.sleep(RATE_LIMIT)

            if (i + 1) % 20 == 0:
                conn.commit()
                logger.info(f"[{coin.upper()}] Snapshots: [{i+1}/{len(need_snaps)}] "
                            f"{total_snaps:,} total")
            time.sleep(RATE_LIMIT)

    conn.commit()

    # Final stats
    total_m = conn.execute("SELECT COUNT(*) FROM markets").fetchone()[0]
    total_s = conn.execute("SELECT COUNT(DISTINCT market_id) FROM snapshots").fetchone()[0]
    total_rows = conn.execute("SELECT COUNT(*) FROM snapshots").fetchone()[0]
    first = conn.execute("SELECT MIN(start_time) FROM markets").fetchone()[0]
    last = conn.execute("SELECT MAX(end_time) FROM markets").fetchone()[0]
    conn.close()

    logger.info(f"[{coin.upper()}] DONE: {total_m} markets, {total_s} with snapshots, "
                f"{total_rows:,} snapshot rows, {first[:10]} to {last[:10]}")


def main():
    logging.basicConfig(level=logging.INFO,
                        format="%(asctime)s %(levelname)s %(message)s")
    parser = argparse.ArgumentParser()
    parser.add_argument("--coin", required=True, choices=["btc", "eth", "sol"])
    parser.add_argument("--limit", type=int, default=9000)
    parser.add_argument("--type", type=str, default="5m")
    parser.add_argument("--move-threshold", type=float, default=0.05,
                        help="Min price move %% to fetch snapshots")
    args = parser.parse_args()
    fetch_all(args.coin, args.type, args.limit, args.move_threshold)


if __name__ == "__main__":
    main()
