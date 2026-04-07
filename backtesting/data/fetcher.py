"""Unified coin data fetcher for PolyBackTest API.

Fetches market headers and snapshots for any supported coin,
stores them in per-coin SQLite databases. Supports 429 retry
with exponential backoff.

Usage:
    python backtesting/data/fetcher.py --coin btc --limit 9000
    python backtesting/data/fetcher.py --coin eth --limit 8000
    python backtesting/data/fetcher.py --coin sol --limit 5000
"""
from __future__ import annotations

import argparse
import logging
import sys
import time
from pathlib import Path

import httpx

sys.path.insert(0, str(Path(__file__).parent.parent.parent))

from shared.constants import (
    POLYBACKTEST_API_BASE,
    POLYBACKTEST_API_KEYS,
    RATE_LIMIT_DELAY,
    db_path,
)
from shared.db import init_coin_db

logger = logging.getLogger(__name__)


class CoinDataFetcher:
    def __init__(self, coin: str, market_type: str = "5m"):
        self.coin = coin
        self.market_type = market_type
        self.api_base = POLYBACKTEST_API_BASE
        self.api_key = POLYBACKTEST_API_KEYS.get(coin, POLYBACKTEST_API_KEYS["btc"])
        self.rate_limit = RATE_LIMIT_DELAY

    def _headers(self) -> dict[str, str]:
        return {"X-API-Key": self.api_key}

    def _api_get(self, client: httpx.Client, url: str, **kwargs) -> httpx.Response:
        """GET with retry on 429 and transient network failures."""
        for attempt in range(5):
            try:
                resp = client.get(url, headers=self._headers(), **kwargs)
            except httpx.HTTPError as exc:
                wait = 2 ** attempt
                logger.warning(
                    f"[{self.coin.upper()}] request failed ({exc.__class__.__name__}), "
                    f"retrying in {wait}s"
                )
                time.sleep(wait)
                continue
            if resp.status_code == 429:
                wait = 2 ** attempt
                logger.warning(f"[{self.coin.upper()}] 429, waiting {wait}s...")
                time.sleep(wait)
                continue
            return resp
        raise RuntimeError(f"[{self.coin.upper()}] failed to fetch {url} after retries")

    def fetch_headers(
        self, client: httpx.Client, limit: int = 9000
    ) -> list[dict]:
        """Fetch all resolved market headers via pagination."""
        all_markets: list[dict] = []
        offset = 0
        while len(all_markets) < limit:
            batch_size = min(limit - len(all_markets), 50)
            resp = self._api_get(
                client,
                f"{self.api_base}/v2/markets",
                params={
                    "coin": self.coin,
                    "market_type": self.market_type,
                    "limit": batch_size,
                    "offset": offset,
                },
                timeout=15,
            )
            if resp.status_code != 200:
                logger.warning(
                    f"[{self.coin.upper()}] HTTP {resp.status_code} at offset {offset}"
                )
                break
            markets = resp.json().get("markets", [])
            if not markets:
                break
            resolved = [m for m in markets if m.get("winner")]
            all_markets.extend(resolved)
            if len(markets) < batch_size:
                break
            offset += batch_size
            time.sleep(self.rate_limit)
            if offset % 500 == 0:
                logger.info(
                    f"[{self.coin.upper()}] Headers: {len(all_markets)} resolved "
                    f"at offset {offset}"
                )
        return all_markets

    def fetch_snapshots(
        self, client: httpx.Client, market_id: str
    ) -> list[dict]:
        """Fetch all snapshots for a single market."""
        all_snaps: list[dict] = []
        offset = 0
        while True:
            resp = self._api_get(
                client,
                f"{self.api_base}/v2/markets/{market_id}/snapshots",
                params={"coin": self.coin, "limit": 500, "offset": offset},
                timeout=15,
            )
            if resp.status_code != 200:
                break
            batch = resp.json().get("snapshots", [])
            if not batch:
                break
            all_snaps.extend(batch)
            if len(batch) < 500:
                break
            offset += 500
            time.sleep(self.rate_limit)
        return all_snaps

    def fetch_all(
        self, limit: int = 9000, move_threshold: float = 0.05
    ) -> tuple[int, int]:
        """Fetch headers and snapshots, store in per-coin DB.

        Returns (new_markets_stored, snapshot_rows_stored).
        """
        coin_db_path = db_path(self.coin)
        conn = init_coin_db(coin_db_path)

        existing = {
            r[0] for r in conn.execute("SELECT market_id FROM markets").fetchall()
        }
        existing_snaps = {
            r[0]
            for r in conn.execute(
                "SELECT DISTINCT market_id FROM snapshots"
            ).fetchall()
        }

        with httpx.Client(timeout=15) as client:
            logger.info(
                f"[{self.coin.upper()}] Fetching market headers (limit={limit})..."
            )
            all_markets = self.fetch_headers(client, limit)
            logger.info(
                f"[{self.coin.upper()}] Got {len(all_markets)} resolved markets"
            )

            new_markets = [
                m for m in all_markets if m["market_id"] not in existing
            ]
            need_snaps: list[dict] = []

            for m in new_markets:
                ps = (
                    m.get("btc_price_start")
                    or m.get(f"{self.coin}_price_start", 0)
                )
                pe = (
                    m.get("btc_price_end")
                    or m.get(f"{self.coin}_price_end", 0)
                )
                conn.execute(
                    "INSERT OR IGNORE INTO markets VALUES (?,?,?,?,?,?,?,?,?,?)",
                    (
                        m["market_id"],
                        m.get("slug", ""),
                        m.get("market_type", ""),
                        m.get("start_time", ""),
                        m.get("end_time", ""),
                        ps,
                        pe,
                        m.get("winner"),
                        m.get("final_volume"),
                        m.get("final_liquidity"),
                    ),
                )
                if (
                    ps
                    and pe
                    and ps > 0
                    and abs((pe - ps) / ps * 100) >= move_threshold
                ):
                    if m["market_id"] not in existing_snaps:
                        need_snaps.append(m)

            conn.commit()
            logger.info(
                f"[{self.coin.upper()}] {len(new_markets)} new headers stored, "
                f"{len(need_snaps)} need snapshots (>{move_threshold}% move)"
            )

            total_snaps = 0
            for i, m in enumerate(need_snaps):
                snaps = self.fetch_snapshots(client, m["market_id"])
                rows = []
                for s in snaps:
                    p = s.get("btc_price") or s.get(
                        f"{self.coin}_price", 0
                    )
                    rows.append(
                        (
                            m["market_id"],
                            s.get("time", ""),
                            p,
                            s.get("price_up"),
                            s.get("price_down"),
                        )
                    )
                conn.executemany(
                    "INSERT OR IGNORE INTO snapshots VALUES (?,?,?,?,?)", rows
                )
                total_snaps += len(rows)

                if (i + 1) % 20 == 0:
                    conn.commit()
                    logger.info(
                        f"[{self.coin.upper()}] Snapshots: [{i+1}/{len(need_snaps)}] "
                        f"{total_snaps:,} total"
                    )
                time.sleep(self.rate_limit)

        conn.commit()

        total_m = conn.execute("SELECT COUNT(*) FROM markets").fetchone()[0]
        total_s = conn.execute(
            "SELECT COUNT(DISTINCT market_id) FROM snapshots"
        ).fetchone()[0]
        total_rows = conn.execute("SELECT COUNT(*) FROM snapshots").fetchone()[0]
        first = conn.execute("SELECT MIN(start_time) FROM markets").fetchone()[0]
        last = conn.execute("SELECT MAX(end_time) FROM markets").fetchone()[0]
        conn.close()

        logger.info(
            f"[{self.coin.upper()}] DONE: {total_m} markets, {total_s} with snapshots, "
            f"{total_rows:,} snapshot rows, {first[:10]} to {last[:10]}"
        )
        return len(new_markets), total_snaps


def main():
    logging.basicConfig(
        level=logging.INFO, format="%(asctime)s %(levelname)s %(message)s"
    )
    parser = argparse.ArgumentParser()
    parser.add_argument("--coin", required=True, choices=["btc", "eth", "sol"])
    parser.add_argument("--limit", type=int, default=9000)
    parser.add_argument("--type", type=str, default="5m")
    parser.add_argument(
        "--move-threshold",
        type=float,
        default=0.05,
        help="Min price move %% to fetch snapshots",
    )
    args = parser.parse_args()
    fetcher = CoinDataFetcher(args.coin, args.type)
    fetcher.fetch_all(args.limit, args.move_threshold)


if __name__ == "__main__":
    main()
