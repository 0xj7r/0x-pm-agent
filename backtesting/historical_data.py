"""Fetch historical BTC 5-min market data from Gamma API and Binance.

Stores resolved Polymarket markets and corresponding Binance klines
in a local SQLite database for backtesting with real data.

Usage:
    python backtesting/historical_data.py --days 7
"""
from __future__ import annotations

import argparse
import logging
import sqlite3
import sys
import time
from datetime import datetime, timezone
from pathlib import Path

import httpx

sys.path.insert(0, str(Path(__file__).parent.parent))

logger = logging.getLogger(__name__)

DB_PATH = Path(__file__).parent / "historical.db"
GAMMA_BASE = "https://gamma-api.polymarket.com"
BINANCE_BASE = "https://api.binance.com/api/v3"
RATE_LIMIT_DELAY = 0.5  # 2 req/s


def init_db(db_path: Path = DB_PATH) -> sqlite3.Connection:
    """Create tables if they don't exist and return a connection."""
    conn = sqlite3.connect(str(db_path))
    conn.execute("PRAGMA journal_mode=WAL")
    conn.execute("""
        CREATE TABLE IF NOT EXISTS resolved_markets (
            slug TEXT PRIMARY KEY,
            market_id TEXT,
            title TEXT,
            start_time TEXT,
            end_time TEXT,
            resolved_direction TEXT,
            up_price_at_open REAL,
            down_price_at_open REAL
        )
    """)
    conn.execute("""
        CREATE TABLE IF NOT EXISTS binance_klines (
            slug TEXT,
            timestamp_ms INTEGER,
            open REAL, high REAL, low REAL, close REAL,
            volume REAL, taker_buy_volume REAL,
            PRIMARY KEY (slug, timestamp_ms)
        )
    """)
    conn.commit()
    return conn


def generate_slugs(days: int, now: datetime | None = None) -> list[str]:
    """Generate btc-updown-5m slugs for the last N days."""
    if now is None:
        now = datetime.now(timezone.utc)
    end_ts = int(now.timestamp())
    end_ts -= end_ts % 300  # round down to 5-min boundary
    start_ts = end_ts - (days * 86400)
    slugs = []
    ts = start_ts
    while ts < end_ts:
        slugs.append(f"btc-updown-5m-{ts}")
        ts += 300
    return slugs


def _parse_resolution(event_data: dict) -> tuple[str | None, float, float]:
    """Extract resolved direction and token prices from Gamma event response."""
    import json as _json

    markets = event_data.get("markets", [])
    if not markets:
        return None, 0.0, 0.0

    market = markets[0]

    if not market.get("closed", False):
        return None, 0.0, 0.0

    outcomes_raw = market.get("outcomes", "[]")
    prices_raw = market.get("outcomePrices", "[]")

    try:
        outcomes = _json.loads(outcomes_raw) if isinstance(outcomes_raw, str) else outcomes_raw
        prices = [float(p) for p in (_json.loads(prices_raw) if isinstance(prices_raw, str) else prices_raw)]
    except (ValueError, TypeError, _json.JSONDecodeError):
        return None, 0.0, 0.0

    if len(outcomes) < 2 or len(prices) < 2:
        return None, 0.0, 0.0

    up_idx = None
    down_idx = None
    for i, name in enumerate(outcomes):
        if name.lower() == "up":
            up_idx = i
        elif name.lower() == "down":
            down_idx = i

    if up_idx is None or down_idx is None:
        return None, 0.0, 0.0

    up_price = prices[up_idx]
    down_price = prices[down_idx]

    if up_price > down_price:
        return "UP", up_price, down_price
    return "DOWN", up_price, down_price


def fetch_gamma_market(
    client: httpx.Client,
    slug: str,
) -> dict | None:
    """Fetch a single resolved market from Gamma API. Returns row dict or None."""
    try:
        resp = client.get(f"{GAMMA_BASE}/events", params={"slug": slug}, timeout=10)
        if resp.status_code == 404:
            return None
        resp.raise_for_status()
        data = resp.json()
    except (httpx.HTTPError, ValueError) as e:
        logger.debug(f"Gamma fetch failed for {slug}: {e}")
        return None

    if isinstance(data, list):
        if not data:
            return None
        event = data[0]
    else:
        event = data

    direction, up_price, down_price = _parse_resolution(event)
    if direction is None:
        return None

    markets = event.get("markets", [{}])
    market = markets[0] if markets else {}

    return {
        "slug": slug,
        "market_id": market.get("id", ""),
        "title": event.get("title", ""),
        "start_time": event.get("startDate", ""),
        "end_time": event.get("endDate", ""),
        "resolved_direction": direction,
        "up_price_at_open": up_price,
        "down_price_at_open": down_price,
    }


def fetch_binance_klines(
    client: httpx.Client,
    slug: str,
    start_ts: int,
) -> list[dict]:
    """Fetch 1-minute Binance klines covering a 5-minute window."""
    start_ms = start_ts * 1000
    end_ms = (start_ts + 300) * 1000
    try:
        resp = client.get(
            f"{BINANCE_BASE}/klines",
            params={
                "symbol": "BTCUSDT",
                "interval": "1m",
                "startTime": start_ms,
                "endTime": end_ms,
                "limit": 10,
            },
            timeout=10,
        )
        resp.raise_for_status()
        raw = resp.json()
    except (httpx.HTTPError, ValueError) as e:
        logger.debug(f"Binance fetch failed for {slug}: {e}")
        return []

    rows = []
    for candle in raw:
        rows.append({
            "slug": slug,
            "timestamp_ms": int(candle[0]),
            "open": float(candle[1]),
            "high": float(candle[2]),
            "low": float(candle[3]),
            "close": float(candle[4]),
            "volume": float(candle[5]),
            "taker_buy_volume": float(candle[9]),
        })
    return rows


def _slug_to_timestamp(slug: str) -> int:
    """Extract Unix timestamp from slug like btc-updown-5m-1712345600."""
    return int(slug.rsplit("-", 1)[-1])


def fetch_all(
    days: int,
    db_path: Path = DB_PATH,
    progress: bool = True,
) -> tuple[int, int]:
    """Fetch historical data for the last N days. Returns (markets_found, klines_stored)."""
    conn = init_db(db_path)
    slugs = generate_slugs(days)

    existing = {
        row[0]
        for row in conn.execute("SELECT slug FROM resolved_markets").fetchall()
    }
    new_slugs = [s for s in slugs if s not in existing]

    if progress:
        total = len(new_slugs)
        logger.info(f"Fetching {total} new slugs ({len(existing)} already cached)")

    markets_found = 0
    klines_stored = 0

    with httpx.Client() as client:
        for i, slug in enumerate(new_slugs):
            row = fetch_gamma_market(client, slug)
            time.sleep(RATE_LIMIT_DELAY)

            if row is None:
                continue

            conn.execute(
                """INSERT OR REPLACE INTO resolved_markets
                   (slug, market_id, title, start_time, end_time,
                    resolved_direction, up_price_at_open, down_price_at_open)
                   VALUES (?, ?, ?, ?, ?, ?, ?, ?)""",
                (
                    row["slug"], row["market_id"], row["title"],
                    row["start_time"], row["end_time"],
                    row["resolved_direction"],
                    row["up_price_at_open"], row["down_price_at_open"],
                ),
            )
            markets_found += 1

            ts = _slug_to_timestamp(slug)
            klines = fetch_binance_klines(client, slug, ts)
            time.sleep(RATE_LIMIT_DELAY)

            for kl in klines:
                conn.execute(
                    """INSERT OR REPLACE INTO binance_klines
                       (slug, timestamp_ms, open, high, low, close,
                        volume, taker_buy_volume)
                       VALUES (?, ?, ?, ?, ?, ?, ?, ?)""",
                    (
                        kl["slug"], kl["timestamp_ms"],
                        kl["open"], kl["high"], kl["low"], kl["close"],
                        kl["volume"], kl["taker_buy_volume"],
                    ),
                )
                klines_stored += 1

            if progress and (i + 1) % 50 == 0:
                conn.commit()
                logger.info(f"  [{i+1}/{len(new_slugs)}] {markets_found} markets, {klines_stored} klines")

    conn.commit()
    conn.close()

    if progress:
        logger.info(f"Done: {markets_found} markets, {klines_stored} klines stored")
    return markets_found, klines_stored


def load_windows(db_path: Path = DB_PATH) -> list[dict]:
    """Load resolved markets with computed features from kline data.

    Returns a list of dicts with fields compatible with the backtester:
      slug, market_id, resolved_direction, up_price, down_price,
      price_delta, order_flow_imbalance
    """
    conn = sqlite3.connect(str(db_path))
    conn.row_factory = sqlite3.Row

    markets = conn.execute(
        "SELECT * FROM resolved_markets ORDER BY slug"
    ).fetchall()

    windows = []
    for m in markets:
        slug = m["slug"]
        klines = conn.execute(
            "SELECT * FROM binance_klines WHERE slug = ? ORDER BY timestamp_ms",
            (slug,),
        ).fetchall()

        if not klines:
            continue

        first_open = klines[0]["open"]
        last_close = klines[-1]["close"]
        if first_open == 0:
            continue

        price_delta = (last_close - first_open) / first_open * 100

        total_vol = sum(k["volume"] for k in klines)
        taker_buy_vol = sum(k["taker_buy_volume"] for k in klines)
        ofi = (taker_buy_vol / total_vol * 2 - 1) if total_vol > 0 else 0.0

        windows.append({
            "slug": slug,
            "market_id": m["market_id"],
            "resolved_direction": m["resolved_direction"],
            "up_price": m["up_price_at_open"],
            "down_price": m["down_price_at_open"],
            "price_delta": price_delta,
            "order_flow_imbalance": ofi,
        })

    conn.close()
    return windows


def main() -> None:
    logging.basicConfig(
        level=logging.INFO,
        format="%(asctime)s %(levelname)s %(message)s",
    )
    parser = argparse.ArgumentParser(description="Fetch BTC historical data")
    parser.add_argument("--days", type=int, default=7, help="Days of history to fetch")
    parser.add_argument("--db", type=str, default=str(DB_PATH), help="Database path")
    args = parser.parse_args()
    fetch_all(args.days, db_path=Path(args.db))


if __name__ == "__main__":
    main()
