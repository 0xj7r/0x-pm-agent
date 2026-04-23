#!/usr/bin/env python3
"""Collect and persist wallet-research data for a Polymarket wallet.

Scope for the first tranche:
  - full activity backfill in time buckets
  - closed positions
  - accounting snapshot archive
  - market metadata for all touched slugs
  - current orderbook snapshots for touched token ids
  - BTC price series for the wallet's active time range

This targets unlawful-shear by default, but the pipeline is reusable for
other tracked wallets.
"""
from __future__ import annotations

import argparse
import csv
import io
import json
import sqlite3
import time
import zipfile
from collections import defaultdict
from dataclasses import dataclass
from datetime import UTC, datetime, timedelta
from pathlib import Path
from typing import Any

import httpx

from research.wallet_aliases import wallet_dir_name

DATA_API = "https://data-api.polymarket.com"
GAMMA_API = "https://gamma-api.polymarket.com"
CLOB_API = "https://clob.polymarket.com"
BINANCE_API = "https://api.binance.com"
DEFAULT_WALLET = "0xb27bc932bf8110d8f78e55da7d5f0497a18b5b82"
ROOT = Path(__file__).resolve().parent.parent.parent
SCHEMA_PATH = ROOT / "research" / "dataops" / "wallet_research_schema.sql"
RESEARCH_ROOT = ROOT / "data" / "research" / "wallet_research"


def parse_date(value: str) -> datetime:
    return datetime.strptime(value, "%Y-%m-%d").replace(tzinfo=UTC)


def ts(dt: datetime) -> int:
    return int(dt.timestamp())


def iso_utc(dt: datetime) -> str:
    return dt.isoformat().replace("+00:00", "Z")


def iso_from_ts(value: int | float | str | None) -> str | None:
    if value is None or value == "":
        return None
    return datetime.fromtimestamp(int(float(value)), tz=UTC).isoformat().replace("+00:00", "Z")


def json_dumps(value: Any) -> str:
    return json.dumps(value, sort_keys=True, separators=(",", ":"))


def infer_asset(slug: str) -> str:
    lowered = (slug or "").lower()
    if lowered.startswith("btc-") or lowered.startswith("bitcoin-"):
        return "BTC"
    if lowered.startswith("eth-") or lowered.startswith("ethereum-"):
        return "ETH"
    if lowered.startswith("sol-") or lowered.startswith("solana-"):
        return "SOL"
    if lowered.startswith("xrp-"):
        return "XRP"
    return "OTHER"


def infer_family(slug: str) -> str:
    lowered = (slug or "").lower()
    if "updown-5m" in lowered:
        return "updown_5m"
    if "updown-15m" in lowered:
        return "updown_15m"
    if "updown-1h" in lowered:
        return "updown_1h"
    if "updown" in lowered:
        return "updown_other"
    return "other"


def safe_float(value: Any) -> float | None:
    if value is None or value == "":
        return None
    try:
        return float(value)
    except (TypeError, ValueError):
        return None


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
        for outcome in outcomes:
            token = outcome.get("tokenId")
            if token:
                ids.append(str(token))
    deduped: list[str] = []
    seen: set[str] = set()
    for token_id in ids:
        if token_id not in seen:
            seen.add(token_id)
            deduped.append(token_id)
    return deduped


def activity_event_id(wallet: str, row: dict[str, Any]) -> str:
    parts = [
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
    return "|".join(parts)


def closed_position_id(wallet: str, row: dict[str, Any]) -> str:
    for key in ("id", "positionId"):
        value = row.get(key)
        if value not in (None, ""):
            return f"{wallet.lower()}|{value}"
    parts = [
        wallet.lower(),
        str(row.get("slug") or ""),
        str(row.get("outcome") or ""),
        str(row.get("endDate") or row.get("timestamp") or ""),
        str(row.get("realizedPnl") or ""),
        str(row.get("shares") or row.get("size") or ""),
    ]
    return "|".join(parts)


@dataclass(frozen=True)
class Window:
    start: datetime
    end: datetime

    @property
    def label(self) -> str:
        return f"{self.start.strftime('%Y%m%dT%H%M%SZ')}_{self.end.strftime('%Y%m%dT%H%M%SZ')}"


class WalletResearchStore:
    def __init__(self, db_path: Path) -> None:
        self._db_path = db_path
        self._db_path.parent.mkdir(parents=True, exist_ok=True)
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

    def start_run(self, wallet: str, config: dict[str, Any]) -> int:
        now = iso_utc(datetime.now(tz=UTC))
        cur = self._conn.execute(
            """
            INSERT INTO collection_runs(wallet_address, started_at, status, config_json)
            VALUES (?, ?, ?, ?)
            """,
            (wallet.lower(), now, "running", json.dumps(config, indent=2)),
        )
        self._conn.commit()
        return int(cur.lastrowid)

    def finish_run(
        self,
        run_id: int,
        *,
        status: str,
        summary: dict[str, Any] | None = None,
        error_text: str | None = None,
    ) -> None:
        self._conn.execute(
            """
            UPDATE collection_runs
            SET completed_at = ?, status = ?, summary_json = ?, error_text = ?
            WHERE id = ?
            """,
            (
                iso_utc(datetime.now(tz=UTC)),
                status,
                json.dumps(summary, indent=2) if summary is not None else None,
                error_text,
                run_id,
            ),
        )
        self._conn.commit()

    def upsert_wallet(self, wallet: str, alias: str, first_seen_ts: int | None, last_seen_ts: int | None) -> None:
        now = iso_utc(datetime.now(tz=UTC))
        self._conn.execute(
            """
            INSERT INTO wallets(wallet_address, wallet_alias, first_seen_ts, last_seen_ts, created_at, updated_at)
            VALUES (?, ?, ?, ?, ?, ?)
            ON CONFLICT(wallet_address) DO UPDATE SET
                wallet_alias = excluded.wallet_alias,
                first_seen_ts = COALESCE(wallets.first_seen_ts, excluded.first_seen_ts),
                last_seen_ts = CASE
                    WHEN wallets.last_seen_ts IS NULL THEN excluded.last_seen_ts
                    WHEN excluded.last_seen_ts IS NULL THEN wallets.last_seen_ts
                    ELSE MAX(wallets.last_seen_ts, excluded.last_seen_ts)
                END,
                updated_at = excluded.updated_at
            """,
            (wallet.lower(), alias, first_seen_ts, last_seen_ts, now, now),
        )
        self._conn.commit()

    def insert_activity_rows(self, wallet: str, rows: list[dict[str, Any]]) -> int:
        inserted = 0
        for row in rows:
            event_ts = int(row.get("timestamp") or 0)
            payload = (
                activity_event_id(wallet, row),
                wallet.lower(),
                event_ts,
                iso_from_ts(event_ts) or "",
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
            )
            before = self._conn.total_changes
            self._conn.execute(
                """
                INSERT OR IGNORE INTO wallet_activity_raw(
                    event_id, wallet_address, event_ts, event_iso, activity_type, side, outcome,
                    slug, event_slug, market_slug, condition_id, transaction_hash, order_id, trade_id,
                    size, usdc_size, price, raw_json
                ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
                """,
                payload,
            )
            if self._conn.total_changes > before:
                inserted += 1
        self._conn.commit()
        return inserted

    def insert_closed_positions(self, wallet: str, rows: list[dict[str, Any]]) -> int:
        inserted = 0
        for row in rows:
            payload = (
                closed_position_id(wallet, row),
                wallet.lower(),
                row.get("slug"),
                row.get("outcome"),
                safe_float(row.get("realizedPnl")),
                safe_float(row.get("totalBought")),
                safe_float(row.get("avgPrice")),
                safe_float(row.get("shares") or row.get("size")),
                row.get("endDate"),
                json.dumps(row, separators=(",", ":")),
            )
            before = self._conn.total_changes
            self._conn.execute(
                """
                INSERT OR IGNORE INTO wallet_closed_positions_raw(
                    position_id, wallet_address, slug, outcome, realized_pnl, total_bought,
                    avg_price, shares, end_date_iso, raw_json
                ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
                """,
                payload,
            )
            if self._conn.total_changes > before:
                inserted += 1
        self._conn.commit()
        return inserted

    def insert_accounting_snapshot(
        self,
        wallet: str,
        *,
        snapshot_id: str,
        source_name: str,
        payload: dict[str, Any],
        row_count: int | None,
    ) -> None:
        self._conn.execute(
            """
            INSERT OR REPLACE INTO wallet_accounting_snapshots(
                snapshot_id, wallet_address, captured_at, source_name, row_count, payload_json
            ) VALUES (?, ?, ?, ?, ?, ?)
            """,
            (
                snapshot_id,
                wallet.lower(),
                iso_utc(datetime.now(tz=UTC)),
                source_name,
                row_count,
                json.dumps(payload, separators=(",", ":")),
            ),
        )
        self._conn.commit()

    def insert_markets(self, rows: list[dict[str, Any]]) -> int:
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
                    event_start_time, closed_time, series_slug, resolution_source, price_to_beat, final_price,
                    closed, archived, active, outcome_names_json, token_ids_json, raw_json
                ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
                """,
                (
                    market_id,
                    row.get("slug"),
                    row.get("question"),
                    row.get("eventTitle") or row.get("title"),
                    infer_asset(str(row.get("slug") or "")),
                    infer_family(str(row.get("slug") or "")),
                    row.get("startDate") or row.get("startTime"),
                    row.get("endDate") or row.get("endTime"),
                    row.get("eventStartTime"),
                    row.get("closedTime"),
                    row.get("seriesSlug"),
                    row.get("resolutionSource"),
                    safe_float((row.get("eventMetadata") or {}).get("priceToBeat")),
                    safe_float((row.get("eventMetadata") or {}).get("finalPrice")),
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

    def sync_market_anchor_fields_from_raw_json(self) -> int:
        rows = self._conn.execute(
            """
            SELECT market_id, raw_json
            FROM wallet_market_catalog
            WHERE price_to_beat IS NULL OR final_price IS NULL OR series_slug IS NULL
            """
        ).fetchall()
        updated = 0
        for row in rows:
            payload = json.loads(str(row["raw_json"]))
            event_meta = payload.get("eventMetadata") or {}
            before = self._conn.total_changes
            self._conn.execute(
                """
                UPDATE wallet_market_catalog
                SET event_start_time = COALESCE(event_start_time, ?),
                    closed_time = COALESCE(closed_time, ?),
                    series_slug = COALESCE(series_slug, ?),
                    resolution_source = COALESCE(resolution_source, ?),
                    price_to_beat = COALESCE(price_to_beat, ?),
                    final_price = COALESCE(final_price, ?)
                WHERE market_id = ?
                """,
                (
                    payload.get("eventStartTime"),
                    payload.get("closedTime"),
                    payload.get("seriesSlug"),
                    payload.get("resolutionSource"),
                    safe_float(event_meta.get("priceToBeat")),
                    safe_float(event_meta.get("finalPrice")),
                    str(row["market_id"]),
                ),
            )
            if self._conn.total_changes > before:
                updated += 1
        self._conn.commit()
        return updated

    def insert_orderbook_snapshot(
        self,
        wallet: str,
        *,
        market_id: str | None,
        slug: str | None,
        token_id: str,
        captured_at: str,
        book: dict[str, Any],
    ) -> None:
        bids = book.get("bids") or []
        asks = book.get("asks") or []
        best_bid = safe_float((bids[0] or {}).get("price")) if bids else None
        best_ask = safe_float((asks[0] or {}).get("price")) if asks else None
        bid_size = safe_float((bids[0] or {}).get("size")) if bids else None
        ask_size = safe_float((asks[0] or {}).get("size")) if asks else None
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
                json.dumps(book, separators=(",", ":")),
            ),
        )
        self._conn.commit()

    def insert_btc_bars(self, wallet: str, interval: str, rows: list[list[Any]]) -> int:
        inserted = 0
        for row in rows:
            open_time_ms = int(row[0])
            key = f"{wallet.lower()}|binance|BTCUSDT|{interval}|{open_time_ms}"
            before = self._conn.total_changes
            self._conn.execute(
                """
                INSERT OR IGNORE INTO wallet_btc_price_series(
                    series_key, wallet_address, exchange, symbol, interval, open_time_ms, close_time_ms,
                    open, high, low, close, volume, trade_count, raw_json
                ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
                """,
                (
                    key,
                    wallet.lower(),
                    "binance",
                    "BTCUSDT",
                    interval,
                    open_time_ms,
                    int(row[6]),
                    safe_float(row[1]),
                    safe_float(row[2]),
                    safe_float(row[3]),
                    safe_float(row[4]),
                    safe_float(row[5]),
                    int(row[8]),
                    json.dumps(row, separators=(",", ":")),
                ),
            )
            if self._conn.total_changes > before:
                inserted += 1
        self._conn.commit()
        return inserted

    def min_max_activity_ts(self, wallet: str) -> tuple[int | None, int | None]:
        row = self._conn.execute(
            """
            SELECT MIN(event_ts) AS min_ts, MAX(event_ts) AS max_ts
            FROM wallet_activity_raw
            WHERE wallet_address = ?
            """,
            (wallet.lower(),),
        ).fetchone()
        if row is None:
            return None, None
        return row["min_ts"], row["max_ts"]


class PolymarketResearchClient:
    def __init__(self, timeout: float = 60.0) -> None:
        self._http = httpx.Client(timeout=httpx.Timeout(timeout, connect=20.0))

    def close(self) -> None:
        self._http.close()

    def fetch_activity_window(self, wallet: str, start: datetime, end: datetime, limit: int = 500) -> list[dict[str, Any]]:
        rows: list[dict[str, Any]] = []
        offset = 0
        while True:
            resp = self._http.get(
                f"{DATA_API}/activity",
                params={
                    "user": wallet,
                    "start": str(ts(start)),
                    "end": str(ts(end)),
                    "limit": str(limit),
                    "offset": str(offset),
                },
            )
            if resp.status_code == 400:
                break
            resp.raise_for_status()
            batch = resp.json()
            if not isinstance(batch, list) or not batch:
                break
            rows.extend(item for item in batch if isinstance(item, dict))
            if len(batch) < limit:
                break
            offset += limit
            if offset > 20000:
                break
        return rows

    def fetch_closed_positions(self, wallet: str, limit: int = 500) -> list[dict[str, Any]]:
        rows: list[dict[str, Any]] = []
        offset = 0
        while True:
            resp = self._http.get(
                f"{DATA_API}/closed-positions",
                params={
                    "user": wallet,
                    "limit": str(limit),
                    "offset": str(offset),
                    "sortBy": "TIMESTAMP",
                    "sortDirection": "DESC",
                },
            )
            resp.raise_for_status()
            batch = resp.json()
            if not isinstance(batch, list) or not batch:
                break
            rows.extend(item for item in batch if isinstance(item, dict))
            if len(batch) < limit:
                break
            offset += limit
            if offset > 20000:
                break
        return rows

    def fetch_accounting_snapshot_bytes(self, wallet: str) -> bytes | None:
        try:
            resp = self._http.get(f"{DATA_API}/v1/accounting/snapshot", params={"user": wallet})
        except httpx.HTTPError:
            return None
        if resp.status_code >= 400:
            return None
        return resp.content

    def fetch_gamma_market(self, slug: str) -> list[dict[str, Any]]:
        for attempt in range(6):
            resp = self._http.get(f"{GAMMA_API}/events", params={"slug": slug})
            if resp.status_code == 429:
                retry_after = resp.headers.get("Retry-After")
                try:
                    wait = float(retry_after) if retry_after is not None else 1.5 * (attempt + 1)
                except ValueError:
                    wait = 1.5 * (attempt + 1)
                time.sleep(min(wait, 15.0))
                continue
            resp.raise_for_status()
            payload = resp.json()
            if isinstance(payload, list):
                markets: list[dict[str, Any]] = []
                for item in payload:
                    if not isinstance(item, dict):
                        continue
                    nested = item.get("markets")
                    if isinstance(nested, list):
                        for market in nested:
                            if isinstance(market, dict):
                                enriched = dict(market)
                                enriched.setdefault("eventTitle", item.get("title"))
                                enriched.setdefault("slug", slug)
                                enriched.setdefault("eventMetadata", item.get("eventMetadata"))
                                enriched.setdefault("seriesSlug", item.get("seriesSlug"))
                                enriched.setdefault("eventStartTime", item.get("startTime"))
                                enriched.setdefault("closedTime", item.get("closedTime"))
                                markets.append(enriched)
                return markets
            if isinstance(payload, dict):
                markets = payload.get("markets")
                if isinstance(markets, list):
                    return [item for item in markets if isinstance(item, dict)]
            return []
        return []

    def fetch_book(self, token_id: str) -> dict[str, Any] | None:
        for attempt in range(5):
            resp = self._http.get(f"{CLOB_API}/book", params={"token_id": token_id})
            if resp.status_code == 429:
                time.sleep(min(1.0 * (attempt + 1), 10.0))
                continue
            if resp.status_code >= 400:
                return None
            payload = resp.json()
            return payload if isinstance(payload, dict) else None
        return None

    def fetch_binance_klines(
        self,
        *,
        symbol: str,
        interval: str,
        start_ms: int,
        end_ms: int,
        limit: int = 1000,
    ) -> list[list[Any]]:
        rows: list[list[Any]] = []
        cursor = start_ms
        while cursor < end_ms:
            resp = self._http.get(
                f"{BINANCE_API}/api/v3/klines",
                params={
                    "symbol": symbol,
                    "interval": interval,
                    "startTime": str(cursor),
                    "endTime": str(end_ms),
                    "limit": str(limit),
                },
            )
            resp.raise_for_status()
            batch = resp.json()
            if not isinstance(batch, list) or not batch:
                break
            rows.extend(batch)
            last_open = int(batch[-1][0])
            if last_open <= cursor:
                break
            cursor = int(batch[-1][6]) + 1
            if len(batch) < limit:
                break
        return rows


def iter_windows(start: datetime, end: datetime, bucket_hours: int) -> list[Window]:
    windows: list[Window] = []
    cur = start
    step = timedelta(hours=bucket_hours)
    while cur < end:
        nxt = min(end, cur + step)
        windows.append(Window(cur, nxt))
        cur = nxt
    return windows


def parse_accounting_zip(blob: bytes) -> dict[str, Any]:
    result: dict[str, Any] = {"files": {}}
    with zipfile.ZipFile(io.BytesIO(blob)) as zf:
        for name in zf.namelist():
            raw = zf.read(name)
            if name.endswith(".json"):
                result["files"][name] = json.loads(raw.decode("utf-8"))
            elif name.endswith(".csv"):
                text = raw.decode("utf-8")
                reader = csv.DictReader(io.StringIO(text))
                result["files"][name] = list(reader)
            else:
                result["files"][name] = {"bytes": len(raw)}
    return result


def save_json(path: Path, payload: Any) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(json.dumps(payload, indent=2))


def main() -> None:
    ap = argparse.ArgumentParser()
    ap.add_argument("--wallet", default=DEFAULT_WALLET)
    ap.add_argument("--start-date", required=True)
    ap.add_argument("--end-date", required=True)
    ap.add_argument("--bucket-hours", type=int, default=12)
    ap.add_argument("--binance-interval", default="1m")
    ap.add_argument("--skip-books", action="store_true")
    ap.add_argument("--activity-only", action="store_true")
    ap.add_argument("--metadata-only", action="store_true")
    ap.add_argument("--books-only", action="store_true")
    ap.add_argument("--btc-only", action="store_true")
    ap.add_argument("--seed-existing", action="store_true")
    args = ap.parse_args()

    wallet = args.wallet.lower()
    wallet_alias = wallet_dir_name(wallet)
    base_dir = RESEARCH_ROOT / wallet_alias
    raw_dir = base_dir / "raw_capture"
    raw_dir.mkdir(parents=True, exist_ok=True)
    db_path = base_dir / "wallet_research.db"
    historical_dir = base_dir / "history" / "historical"
    current_dir = base_dir / "history" / "current"

    start = parse_date(args.start_date)
    end = parse_date(args.end_date)
    windows = iter_windows(start, end, args.bucket_hours)
    run_activity = args.activity_only or not any((args.metadata_only, args.books_only, args.btc_only))
    run_metadata = args.metadata_only or not any((args.activity_only, args.books_only, args.btc_only))
    run_books = args.books_only or (not any((args.activity_only, args.metadata_only, args.btc_only)) and not args.skip_books)
    run_btc = args.btc_only or not any((args.activity_only, args.metadata_only, args.books_only))

    store = WalletResearchStore(db_path)
    client = PolymarketResearchClient()
    run_id = store.start_run(
        wallet,
        {
            "wallet": wallet,
            "wallet_alias": wallet_alias,
            "start_date": args.start_date,
            "end_date": args.end_date,
            "bucket_hours": args.bucket_hours,
            "binance_interval": args.binance_interval,
            "skip_books": args.skip_books,
            "activity_only": args.activity_only,
            "metadata_only": args.metadata_only,
            "books_only": args.books_only,
            "btc_only": args.btc_only,
            "seed_existing": args.seed_existing,
        },
    )

    try:
        all_activity: list[dict[str, Any]] = []
        per_window_counts: list[dict[str, Any]] = []
        seen_event_ids: set[str] = set()
        inserted_activity = 0
        inserted_closed = 0
        closed_positions: list[dict[str, Any]] = []
        if args.seed_existing:
            seed_activity_paths = [
                raw_dir / "activity_all.json",
            ]
            seed_closed_paths = [
                raw_dir / "closed_positions.json",
                historical_dir / "closed_positions.json",
                current_dir / "closed_positions.json",
            ]
            for path in seed_activity_paths:
                if path.exists():
                    for row in json.loads(path.read_text()):
                        if isinstance(row, dict):
                            event_id = activity_event_id(wallet, row)
                            if event_id not in seen_event_ids:
                                seen_event_ids.add(event_id)
                                all_activity.append(row)
            for path in seed_closed_paths:
                if path.exists():
                    for row in json.loads(path.read_text()):
                        if isinstance(row, dict):
                            closed_positions.append(row)
        if run_activity:
            for window in windows:
                rows = client.fetch_activity_window(wallet, window.start, window.end)
                save_json(raw_dir / "activity_windows" / f"{window.label}.json", rows)
                per_window_counts.append(
                    {
                        "window": window.label,
                        "rows": len(rows),
                        "start": iso_utc(window.start),
                        "end": iso_utc(window.end),
                    }
                )
                for row in rows:
                    event_id = activity_event_id(wallet, row)
                    if event_id not in seen_event_ids:
                        seen_event_ids.add(event_id)
                        all_activity.append(row)
            all_activity.sort(key=lambda row: int(row.get("timestamp") or 0))
            save_json(raw_dir / "activity_all.json", all_activity)
            inserted_activity = store.insert_activity_rows(wallet, all_activity)

            first_ts = int(all_activity[0]["timestamp"]) if all_activity else None
            last_ts = int(all_activity[-1]["timestamp"]) if all_activity else None
            store.upsert_wallet(wallet, wallet_alias, first_ts, last_ts)

            network_closed_positions = client.fetch_closed_positions(wallet)
            closed_positions.extend(network_closed_positions)
            deduped_closed: list[dict[str, Any]] = []
            seen_closed_ids: set[str] = set()
            for row in closed_positions:
                row_id = closed_position_id(wallet, row)
                if row_id in seen_closed_ids:
                    continue
                seen_closed_ids.add(row_id)
                deduped_closed.append(row)
            closed_positions = deduped_closed
            save_json(raw_dir / "closed_positions.json", closed_positions)
            inserted_closed = store.insert_closed_positions(wallet, closed_positions)

        accounting_bytes = client.fetch_accounting_snapshot_bytes(wallet) if run_activity else None
        accounting_files: list[str] = []
        if accounting_bytes:
            zip_path = raw_dir / "accounting_snapshot.zip"
            zip_path.write_bytes(accounting_bytes)
            accounting_payload = parse_accounting_zip(accounting_bytes)
            save_json(raw_dir / "accounting_snapshot.json", accounting_payload)
            for source_name, payload in accounting_payload.get("files", {}).items():
                accounting_files.append(source_name)
                row_count = len(payload) if isinstance(payload, list) else None
                store.insert_accounting_snapshot(
                    wallet,
                    snapshot_id=f"{wallet}|{source_name}",
                    source_name=source_name,
                    payload=payload if isinstance(payload, dict | list) else {"value": payload},
                    row_count=row_count,
                )

        if not all_activity:
            existing_rows = store._conn.execute(
                "SELECT raw_json FROM wallet_activity_raw WHERE wallet_address = ? ORDER BY event_ts",
                (wallet,),
            ).fetchall()
            all_activity = [json.loads(str(row["raw_json"])) for row in existing_rows]

        slugs = sorted({str(row.get("slug") or "") for row in all_activity if row.get("slug")})
        markets: list[dict[str, Any]] = []
        by_slug: dict[str, list[dict[str, Any]]] = defaultdict(list)
        inserted_markets = 0
        if run_metadata or run_books:
            for slug in slugs:
                batch = client.fetch_gamma_market(slug)
                if batch:
                    by_slug[slug].extend(batch)
                    markets.extend(batch)
            save_json(raw_dir / "market_catalog.json", {"markets": markets})
            inserted_markets = store.insert_markets(markets)
            store.sync_market_anchor_fields_from_raw_json()

        book_snapshots = 0
        if run_books and not args.skip_books:
            if not markets:
                market_rows = store._conn.execute(
                    "SELECT raw_json FROM wallet_market_catalog WHERE slug LIKE ?",
                    ("btc-updown-5m-%",),
                ).fetchall()
                markets = [json.loads(str(row["raw_json"])) for row in market_rows]
            captured_at = iso_utc(datetime.now(tz=UTC))
            for market in markets:
                market_id = str(market.get("id") or market.get("marketId") or "")
                slug = str(market.get("slug") or "")
                for token_id in extract_token_ids(market):
                    book = client.fetch_book(token_id)
                    if not book:
                        continue
                    store.insert_orderbook_snapshot(
                        wallet,
                        market_id=market_id or None,
                        slug=slug or None,
                        token_id=token_id,
                        captured_at=captured_at,
                        book=book,
                    )
                    book_snapshots += 1

        min_ts, max_ts = store.min_max_activity_ts(wallet)
        btc_rows: list[list[Any]] = []
        inserted_btc = 0
        if run_btc and min_ts is not None and max_ts is not None:
            btc_rows = client.fetch_binance_klines(
                symbol="BTCUSDT",
                interval=args.binance_interval,
                start_ms=min_ts * 1000,
                end_ms=(max_ts + 60) * 1000,
            )
            save_json(raw_dir / f"btc_{args.binance_interval}.json", btc_rows)
            inserted_btc = store.insert_btc_bars(wallet, args.binance_interval, btc_rows)

        summary = {
            "wallet": wallet,
            "wallet_alias": wallet_alias,
            "activity_rows": len(all_activity),
            "activity_rows_inserted": inserted_activity,
            "closed_positions": len(closed_positions),
            "closed_positions_inserted": inserted_closed,
            "market_count": len(markets),
            "markets_inserted": inserted_markets,
            "book_snapshots": book_snapshots,
            "btc_rows": len(btc_rows),
            "btc_rows_inserted": inserted_btc,
            "accounting_files": accounting_files,
            "windows": per_window_counts,
            "db_path": str(db_path),
        }
        save_json(base_dir / "collection_summary.json", summary)
        store.finish_run(run_id, status="completed", summary=summary)
    except Exception as exc:
        store.finish_run(run_id, status="failed", error_text=str(exc))
        raise
    finally:
        client.close()
        store.close()


if __name__ == "__main__":
    main()
