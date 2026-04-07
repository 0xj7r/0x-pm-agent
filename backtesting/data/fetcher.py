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
import json
from concurrent.futures import ThreadPoolExecutor, as_completed
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
SNAPSHOT_PAGE_LIMIT = 1000


class CoinDataFetcher:
    def __init__(
        self,
        coin: str,
        market_type: str = "5m",
        snapshot_workers: int = 6,
    ):
        self.coin = coin
        self.market_type = market_type
        self.api_base = POLYBACKTEST_API_BASE
        self.api_key = POLYBACKTEST_API_KEYS.get(coin, POLYBACKTEST_API_KEYS["btc"])
        self.rate_limit = RATE_LIMIT_DELAY
        self.snapshot_workers = max(1, snapshot_workers)

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
                retry_after = resp.headers.get("Retry-After")
                if retry_after is not None:
                    try:
                        wait = max(float(retry_after), self.rate_limit)
                    except ValueError:
                        wait = 2 ** attempt
                else:
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
                params={
                    "coin": self.coin,
                    "limit": SNAPSHOT_PAGE_LIMIT,
                    "offset": offset,
                },
                timeout=15,
            )
            if resp.status_code != 200:
                break
            batch = resp.json().get("snapshots", [])
            if not batch:
                break
            all_snaps.extend(batch)
            if len(batch) < SNAPSHOT_PAGE_LIMIT:
                break
            offset += SNAPSHOT_PAGE_LIMIT
            time.sleep(self.rate_limit)
        return all_snaps

    @staticmethod
    def _extract_book(ob: dict | None) -> tuple:
        """Pull top-of-book + full JSON from a PolyBackTest orderbook object.

        PolyBackTest returns orderbook_up / orderbook_down each shaped as
        {"bids": [{price, size}, ...], "asks": [{price, size}, ...]} with
        bids sorted descending and asks sorted ascending. We persist the
        top of book as separate columns for fast reads plus the full book
        as JSON for later slippage modelling.

        Returns a 5-tuple (best_bid, best_ask, bid_size, ask_size, json_str)
        with None for missing fields and None for json_str when ob is None.
        """
        if not ob:
            return (None, None, None, None, None)
        bids = ob.get("bids") or []
        asks = ob.get("asks") or []
        best_bid = bids[0].get("price") if bids else None
        best_ask = asks[0].get("price") if asks else None
        bid_sz = bids[0].get("size") if bids else None
        ask_sz = asks[0].get("size") if asks else None
        return (
            best_bid,
            best_ask,
            bid_sz,
            ask_sz,
            json.dumps(ob, separators=(",", ":")),
        )

    def fetch_snapshot_rows(self, market: dict) -> tuple[str, list[tuple]]:
        """Fetch and transform all snapshots for a single market.

        Returns 15-tuples matching the extended snapshots schema:
        (market_id, time, price, price_up, price_down,
         best_bid_up, best_ask_up, bid_size_up, ask_size_up,
         best_bid_down, best_ask_down, bid_size_down, ask_size_down,
         orderbook_up_json, orderbook_down_json)
        """
        with httpx.Client(timeout=15) as client:
            snaps = self.fetch_snapshots(client, market["market_id"])

        rows = []
        for s in snaps:
            p = s.get("btc_price") or s.get(f"{self.coin}_price", 0)
            up = self._extract_book(s.get("orderbook_up"))
            down = self._extract_book(s.get("orderbook_down"))
            rows.append(
                (
                    market["market_id"],
                    s.get("time", ""),
                    p,
                    s.get("price_up"),
                    s.get("price_down"),
                    up[0], up[1], up[2], up[3],
                    down[0], down[1], down[2], down[3],
                    up[4], down[4],
                )
            )
        return market["market_id"], rows

    def fetch_all(
        self, limit: int = 9000, move_threshold: float = 0.0
    ) -> tuple[int, int]:
        """Fetch headers and snapshots, store in per-coin DB.

        Returns (new_markets_stored, snapshot_rows_stored).

        move_threshold defaults to 0.0 so the snapshot sample is unconditional
        on the underlying's realized move. Filtering by realized move was an
        API-call optimization that introduced an outcome-conditional sampling
        bias: strategies trained on the filtered set only ever saw markets
        where movement was guaranteed to exist, which inflated backtest edges
        relative to live. Keep this at 0 for unbiased research backfills.
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

            for m in all_markets:
                ps = (
                    m.get("btc_price_start")
                    or m.get(f"{self.coin}_price_start", 0)
                )
                pe = (
                    m.get("btc_price_end")
                    or m.get(f"{self.coin}_price_end", 0)
                )
                if m["market_id"] not in existing:
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
            completed = 0
            logger.info(
                f"[{self.coin.upper()}] Fetching snapshots with "
                f"{self.snapshot_workers} workers"
            )
            with ThreadPoolExecutor(max_workers=self.snapshot_workers) as executor:
                futures = {
                    executor.submit(self.fetch_snapshot_rows, market): market["market_id"]
                    for market in need_snaps
                }
                for future in as_completed(futures):
                    market_id = futures[future]
                    rows = future.result()[1]
                    conn.executemany(
                        "INSERT OR IGNORE INTO snapshots VALUES "
                        "(?,?,?,?,?,?,?,?,?,?,?,?,?,?,?)",
                        rows,
                    )
                    total_snaps += len(rows)
                    completed += 1

                    if completed % 20 == 0 or completed == len(need_snaps):
                        conn.commit()
                        logger.info(
                            f"[{self.coin.upper()}] Snapshots: "
                            f"[{completed}/{len(need_snaps)}] {total_snaps:,} total"
                        )

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
        "--snapshot-workers",
        type=int,
        default=6,
        help="Concurrent workers for per-market snapshot fetching",
    )
    parser.add_argument(
        "--move-threshold",
        type=float,
        default=0.0,
        help="Min price move %% to fetch snapshots. Default 0 (no filter). "
             "Setting >0 introduces outcome-conditional sampling bias and "
             "should only be used for narrow exploratory pulls, never for "
             "research backfills.",
    )
    args = parser.parse_args()
    fetcher = CoinDataFetcher(args.coin, args.type, args.snapshot_workers)
    fetcher.fetch_all(args.limit, args.move_threshold)


if __name__ == "__main__":
    main()
