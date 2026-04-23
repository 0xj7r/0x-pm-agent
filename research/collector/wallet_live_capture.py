#!/usr/bin/env python3
"""Continuously capture live wallet research data for a Polymarket wallet.

Focuses on forward collection for wallets we want to reverse-engineer, starting
with unlawful-shear. It records:
  - latest wallet activity delta
  - periodic accounting snapshots
  - current orderbook snapshots for active/touched markets

The historical backfill script covers the past. This collector closes the gap
for trade-adjacent market state going forward.
"""
from __future__ import annotations

import argparse
import asyncio
import json
import sqlite3
import threading
import time
from datetime import UTC, datetime
from pathlib import Path
from queue import Empty, SimpleQueue
from typing import Any

import httpx

from execution.clients.polymarket_ws import PolymarketWSClient
from research.wallet_aliases import wallet_dir_name

DATA_API = "https://data-api.polymarket.com"
GAMMA_API = "https://gamma-api.polymarket.com"
CLOB_API = "https://clob.polymarket.com"
DEFAULT_WALLET = "0xb27bc932bf8110d8f78e55da7d5f0497a18b5b82"
ROOT = Path(__file__).resolve().parent.parent.parent
SCHEMA_PATH = ROOT / "research" / "dataops" / "wallet_research_schema.sql"
RESEARCH_ROOT = ROOT / "data" / "research" / "wallet_research"


def iso_utc_now() -> str:
    return datetime.now(tz=UTC).isoformat().replace("+00:00", "Z")


def safe_float(value: Any) -> float | None:
    if value in (None, ""):
        return None
    try:
        return float(value)
    except (TypeError, ValueError):
        return None


def activity_event_id(wallet: str, row: dict[str, Any]) -> str:
    return "|".join(
        [
            wallet.lower(),
            str(row.get("transactionHash") or ""),
            str(row.get("type") or ""),
            str(row.get("slug") or ""),
            str(row.get("conditionId") or ""),
            str(row.get("side") or ""),
            str(row.get("outcome") or ""),
            str(row.get("timestamp") or ""),
            str(row.get("size") or ""),
            str(row.get("price") or ""),
        ]
    )


def extract_token_ids(market: dict[str, Any]) -> list[str]:
    ids: list[str] = []
    for key in ("clobTokenIds", "tokenIds"):
        raw = market.get(key)
        if isinstance(raw, list):
            ids.extend(str(v) for v in raw if v)
        elif isinstance(raw, str) and raw.strip():
            try:
                parsed = json.loads(raw)
            except json.JSONDecodeError:
                parsed = [part.strip() for part in raw.split(",") if part.strip()]
            if isinstance(parsed, list):
                ids.extend(str(v) for v in parsed if v)
    outcomes = market.get("outcomes")
    if isinstance(outcomes, list):
        ids.extend(str(outcome.get("tokenId")) for outcome in outcomes if outcome.get("tokenId"))
    deduped: list[str] = []
    seen: set[str] = set()
    for token_id in ids:
        if token_id and token_id not in seen:
            deduped.append(token_id)
            seen.add(token_id)
    return deduped


class WalletLiveStore:
    def __init__(self, db_path: Path) -> None:
        self._conn = sqlite3.connect(str(db_path))
        self._conn.row_factory = sqlite3.Row
        self._conn.executescript(SCHEMA_PATH.read_text())
        self._migrate_market_catalog()

    def _migrate_market_catalog(self) -> None:
        existing = {
            str(row["name"])
            for row in self._conn.execute("PRAGMA table_info(wallet_market_catalog)").fetchall()
        }
        wanted = {
            "event_start_time": "TEXT",
            "closed_time": "TEXT",
            "series_slug": "TEXT",
            "resolution_source": "TEXT",
            "price_to_beat": "REAL",
            "final_price": "REAL",
        }
        for name, col_type in wanted.items():
            if name not in existing:
                self._conn.execute(f"ALTER TABLE wallet_market_catalog ADD COLUMN {name} {col_type}")
        self._conn.commit()

    def close(self) -> None:
        self._conn.close()

    def latest_activity_ts(self, wallet: str) -> int:
        row = self._conn.execute(
            "SELECT COALESCE(MAX(event_ts), 0) AS max_ts FROM wallet_activity_raw WHERE wallet_address = ?",
            (wallet.lower(),),
        ).fetchone()
        return int(row["max_ts"] or 0)

    def known_market_slugs(self, wallet: str, family_prefix: str) -> list[str]:
        rows = self._conn.execute(
            """
            SELECT DISTINCT slug
            FROM wallet_activity_raw
            WHERE wallet_address = ? AND slug LIKE ?
            ORDER BY slug DESC
            LIMIT 64
            """,
            (wallet.lower(), f"{family_prefix}%"),
        ).fetchall()
        return [str(row["slug"]) for row in rows if row["slug"]]

    def insert_activity_rows(self, wallet: str, rows: list[dict[str, Any]]) -> int:
        inserted = 0
        for row in rows:
            event_ts = int(row.get("timestamp") or 0)
            before = self._conn.total_changes
            self._conn.execute(
                """
                INSERT OR IGNORE INTO wallet_activity_raw(
                    event_id, wallet_address, event_ts, event_iso, activity_type, side, outcome,
                    slug, event_slug, market_slug, condition_id, transaction_hash, order_id, trade_id,
                    size, usdc_size, price, raw_json
                ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
                """,
                (
                    activity_event_id(wallet, row),
                    wallet.lower(),
                    event_ts,
                    datetime.fromtimestamp(event_ts, tz=UTC).isoformat().replace("+00:00", "Z"),
                    row.get("type"),
                    row.get("side"),
                    row.get("outcome"),
                    row.get("slug"),
                    row.get("eventSlug"),
                    row.get("marketSlug") or row.get("slug"),
                    row.get("conditionId"),
                    row.get("transactionHash"),
                    row.get("orderId"),
                    row.get("tradeID") or row.get("tradeId"),
                    safe_float(row.get("size")),
                    safe_float(row.get("usdcSize")),
                    safe_float(row.get("price")),
                    json.dumps(row, separators=(",", ":")),
                ),
            )
            if self._conn.total_changes > before:
                inserted += 1
        self._conn.commit()
        return inserted

    def upsert_markets(self, rows: list[dict[str, Any]]) -> int:
        inserted = 0
        for row in rows:
            market_id = str(row.get("id") or row.get("marketId") or "")
            if not market_id:
                continue
            before = self._conn.total_changes
            self._conn.execute(
                """
                INSERT OR REPLACE INTO wallet_market_catalog(
                    market_id, slug, question, event_title, asset, market_family, start_time, end_time,
                    closed, archived, active, outcome_names_json, token_ids_json, raw_json
                ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
                """,
                (
                    market_id,
                    row.get("slug"),
                    row.get("question"),
                    row.get("eventTitle") or row.get("title"),
                    "BTC",
                    "updown_5m",
                    row.get("startDate") or row.get("startTime"),
                    row.get("endDate") or row.get("endTime"),
                    1 if row.get("closed") else 0,
                    1 if row.get("archived") else 0,
                    1 if row.get("active") else 0,
                    json.dumps(row.get("outcomes")),
                    json.dumps(extract_token_ids(row)),
                    json.dumps(row, separators=(",", ":")),
                ),
            )
            if self._conn.total_changes > before:
                inserted += 1
        self._conn.commit()
        return inserted

    def insert_accounting_snapshot(self, wallet: str, source_name: str, payload: dict[str, Any]) -> None:
        snapshot_id = f"{wallet.lower()}|{source_name}|{iso_utc_now()}"
        row_count = len(payload) if isinstance(payload, list) else None
        self._conn.execute(
            """
            INSERT OR REPLACE INTO wallet_accounting_snapshots(
                snapshot_id, wallet_address, captured_at, source_name, row_count, payload_json
            ) VALUES (?, ?, ?, ?, ?, ?)
            """,
            (
                snapshot_id,
                wallet.lower(),
                iso_utc_now(),
                source_name,
                row_count,
                json.dumps(payload, separators=(",", ":")),
            ),
        )
        self._conn.commit()

    def insert_orderbook_snapshot(
        self,
        wallet: str,
        market_id: str | None,
        slug: str | None,
        token_id: str,
        payload: dict[str, Any],
    ) -> None:
        bids = payload.get("bids") or []
        asks = payload.get("asks") or []
        best_bid = safe_float((bids[0] or {}).get("price")) if bids else None
        best_ask = safe_float((asks[0] or {}).get("price")) if asks else None
        bid_size = safe_float((bids[0] or {}).get("size")) if bids else None
        ask_size = safe_float((asks[0] or {}).get("size")) if asks else None
        captured_at = iso_utc_now()
        snapshot_id = f"{wallet.lower()}|{token_id}|{captured_at}"
        self._conn.execute(
            """
            INSERT OR REPLACE INTO wallet_orderbook_snapshots(
                snapshot_id, wallet_address, market_id, slug, token_id, captured_at, best_bid, best_ask,
                bid_size, ask_size, bids_json, asks_json, raw_json
            ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
            """,
            (
                snapshot_id,
                wallet.lower(),
                market_id,
                slug,
                token_id,
                captured_at,
                best_bid,
                best_ask,
                bid_size,
                ask_size,
                json.dumps(bids, separators=(",", ":")),
                json.dumps(asks, separators=(",", ":")),
                json.dumps(payload, separators=(",", ":")),
            ),
        )
        self._conn.commit()

    def insert_market_stream_event(
        self,
        wallet: str,
        market_id: str | None,
        slug: str | None,
        token_id: str,
        event: dict[str, Any],
    ) -> None:
        captured_at = str(event.get("captured_at") or iso_utc_now())
        raw_payload = event.get("raw")
        snapshot_key = json.dumps(
            {
                "bids": event.get("bids") or [],
                "asks": event.get("asks") or [],
                "trade_price": event.get("trade_price"),
                "trade_size": event.get("trade_size"),
                "trade_side": event.get("trade_side"),
            },
            sort_keys=True,
            separators=(",", ":"),
        )
        event_id = "|".join(
            [
                wallet.lower(),
                str(token_id),
                str(event.get("event_type") or "unknown"),
                captured_at,
                snapshot_key,
            ]
        )
        self._conn.execute(
            """
            INSERT OR IGNORE INTO wallet_market_stream_events(
                event_id, wallet_address, market_id, slug, token_id, captured_at, source,
                event_type, best_bid, best_ask, bid_size, ask_size, spread, last_trade_price,
                trade_price, trade_size, trade_side, bids_json, asks_json, raw_json
            ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
            """,
            (
                event_id,
                wallet.lower(),
                market_id,
                slug,
                token_id,
                captured_at,
                "clob_ws",
                event.get("event_type"),
                safe_float(event.get("best_bid")),
                safe_float(event.get("best_ask")),
                safe_float(event.get("bid_size")),
                safe_float(event.get("ask_size")),
                safe_float(event.get("spread")),
                safe_float(event.get("last_trade_price")),
                safe_float(event.get("trade_price")),
                safe_float(event.get("trade_size")),
                event.get("trade_side"),
                json.dumps(event.get("bids") or [], separators=(",", ":")),
                json.dumps(event.get("asks") or [], separators=(",", ":")),
                json.dumps(raw_payload, separators=(",", ":")),
            ),
        )
        self._conn.commit()


class WalletLiveClient:
    def __init__(self) -> None:
        self._http = httpx.Client(timeout=httpx.Timeout(30.0, connect=10.0))

    def close(self) -> None:
        self._http.close()

    def fetch_recent_activity(self, wallet: str, limit: int = 500) -> list[dict[str, Any]]:
        resp = self._http.get(f"{DATA_API}/activity", params={"user": wallet, "limit": str(limit), "offset": "0"})
        resp.raise_for_status()
        payload = resp.json()
        return [item for item in payload if isinstance(item, dict)] if isinstance(payload, list) else []

    def fetch_accounting_snapshot(self, wallet: str) -> dict[str, Any] | None:
        resp = self._http.get(f"{DATA_API}/v1/accounting/snapshot", params={"user": wallet})
        if resp.status_code >= 400:
            return None
        return {"content_type": resp.headers.get("content-type"), "size_bytes": len(resp.content)}

    def fetch_gamma_markets(self, slug: str) -> list[dict[str, Any]]:
        resp = self._http.get(f"{GAMMA_API}/markets", params={"slug": slug, "limit": "10"})
        resp.raise_for_status()
        payload = resp.json()
        if isinstance(payload, list):
            return [item for item in payload if isinstance(item, dict)]
        if isinstance(payload, dict) and isinstance(payload.get("markets"), list):
            return [item for item in payload["markets"] if isinstance(item, dict)]
        return []

    def fetch_book(self, token_id: str) -> dict[str, Any] | None:
        resp = self._http.get(f"{CLOB_API}/book", params={"token_id": token_id})
        if resp.status_code >= 400:
            return None
        payload = resp.json()
        return payload if isinstance(payload, dict) else None


class LiveMarketFeed:
    def __init__(self) -> None:
        self._queue: SimpleQueue[dict[str, Any]] = SimpleQueue()
        self._client = PolymarketWSClient(event_callback=self._queue.put_nowait)
        self._loop: asyncio.AbstractEventLoop | None = None
        self._thread: threading.Thread | None = None
        self._ready = threading.Event()

    def start(self) -> None:
        if self._thread is not None:
            return
        self._thread = threading.Thread(target=self._run, name="polymarket-live-feed", daemon=True)
        self._thread.start()
        self._ready.wait(timeout=5.0)

    def _run(self) -> None:
        loop = asyncio.new_event_loop()
        self._loop = loop
        asyncio.set_event_loop(loop)
        self._ready.set()
        try:
            loop.run_until_complete(self._client.connect())
        finally:
            loop.run_until_complete(loop.shutdown_asyncgens())
            loop.close()

    def sync_token_ids(self, token_ids: list[str]) -> None:
        if self._loop is None:
            return
        future = asyncio.run_coroutine_threadsafe(self._client.sync_subscriptions(token_ids), self._loop)
        future.result(timeout=10.0)

    def drain_events(self) -> list[dict[str, Any]]:
        events: list[dict[str, Any]] = []
        while True:
            try:
                events.append(self._queue.get_nowait())
            except Empty:
                return events

    def close(self) -> None:
        if self._loop is None:
            return
        future = asyncio.run_coroutine_threadsafe(self._client.close(), self._loop)
        future.result(timeout=10.0)
        if self._thread is not None:
            self._thread.join(timeout=10.0)


def save_json(path: Path, payload: Any) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(json.dumps(payload, indent=2))


def main() -> None:
    ap = argparse.ArgumentParser()
    ap.add_argument("--wallet", default=DEFAULT_WALLET)
    ap.add_argument("--family-prefix", default="btc-updown-5m-")
    ap.add_argument("--poll-seconds", type=float, default=15.0)
    ap.add_argument("--accounting-seconds", type=float, default=300.0)
    ap.add_argument("--loop-limit", type=int, default=0)
    args = ap.parse_args()

    wallet = args.wallet.lower()
    alias = wallet_dir_name(wallet)
    base_dir = RESEARCH_ROOT / alias
    raw_dir = base_dir / "live_capture"
    raw_dir.mkdir(parents=True, exist_ok=True)
    db_path = base_dir / "wallet_research.db"

    store = WalletLiveStore(db_path)
    client = WalletLiveClient()
    feed = LiveMarketFeed()
    loop_count = 0
    last_accounting_at = 0.0
    token_context: dict[str, tuple[str | None, str | None]] = {}

    try:
        feed.start()
        while True:
            loop_count += 1
            activity = client.fetch_recent_activity(wallet)
            newest_known = store.latest_activity_ts(wallet)
            fresh_activity = [row for row in activity if int(row.get("timestamp") or 0) > newest_known]
            inserted = store.insert_activity_rows(wallet, fresh_activity)

            touched_slugs = sorted(
                {
                    str(row.get("slug") or "")
                    for row in (fresh_activity or activity[:50])
                    if str(row.get("slug") or "").startswith(args.family_prefix)
                }
            )
            if not touched_slugs:
                touched_slugs = store.known_market_slugs(wallet, args.family_prefix)[:8]

            markets: list[dict[str, Any]] = []
            for slug in touched_slugs:
                markets.extend(client.fetch_gamma_markets(slug))
            store.upsert_markets(markets)

            subscribed_token_ids: list[str] = []
            for market in markets:
                market_id = str(market.get("id") or market.get("marketId") or "") or None
                slug = str(market.get("slug") or "") or None
                for token_id in extract_token_ids(market):
                    subscribed_token_ids.append(token_id)
                    token_context[token_id] = (market_id, slug)
            if subscribed_token_ids:
                feed.sync_token_ids(sorted(set(subscribed_token_ids)))

            stream_events = feed.drain_events()
            stream_payload = []
            for event in stream_events:
                token_id = str(event.get("token_id") or "")
                market_id, slug = token_context.get(token_id, (None, None))
                store.insert_market_stream_event(wallet, market_id, slug, token_id, event)
                stream_payload.append(
                    {
                        "token_id": token_id,
                        "market_id": market_id,
                        "slug": slug,
                        "captured_at": event.get("captured_at"),
                        "event_type": event.get("event_type"),
                    }
                )

            books_payload = []
            for market in markets:
                market_id = str(market.get("id") or market.get("marketId") or "") or None
                slug = str(market.get("slug") or "") or None
                for token_id in extract_token_ids(market):
                    book = client.fetch_book(token_id)
                    if not book:
                        continue
                    store.insert_orderbook_snapshot(wallet, market_id, slug, token_id, book)
                    books_payload.append(
                        {
                            "market_id": market_id,
                            "slug": slug,
                            "token_id": token_id,
                            "captured_at": iso_utc_now(),
                        }
                    )

            now = time.time()
            if now - last_accounting_at >= args.accounting_seconds:
                accounting = client.fetch_accounting_snapshot(wallet)
                if accounting is not None:
                    store.insert_accounting_snapshot(wallet, "live_probe", accounting)
                    last_accounting_at = now

            stamp = datetime.now(tz=UTC).strftime("%Y%m%dT%H%M%SZ")
            save_json(
                raw_dir / f"{stamp}.json",
                {
                    "wallet": wallet,
                    "fresh_activity_rows": len(fresh_activity),
                    "activity_rows_inserted": inserted,
                    "touched_slugs": touched_slugs,
                    "markets": [market.get("slug") for market in markets],
                    "stream_events": stream_payload,
                    "books": books_payload,
                },
            )

            if args.loop_limit and loop_count >= args.loop_limit:
                break
            time.sleep(max(1.0, args.poll_seconds))
    finally:
        feed.close()
        client.close()
        store.close()


if __name__ == "__main__":
    main()
