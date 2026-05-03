#!/usr/bin/env python3
"""Export runtime env + market context for BTC 5m paper/live trading.

This replaces the missing historical helper scripts referenced by the stale
paper launcher. It supports two sources:

- live: Gamma API discovery of active/upcoming BTC 5m markets
- db: local wallet_research.db fallback for offline preparation
"""

from __future__ import annotations

import argparse
import json
import re
import sqlite3
import sys
import urllib.parse
import urllib.request
from dataclasses import dataclass
from datetime import datetime, timezone
from pathlib import Path
from typing import Any


GAMMA_API = "https://gamma-api.polymarket.com/markets"
SLUG_PREFIX = "btc-updown-5m-"
WINDOW_MS = 5 * 60 * 1000


def now_ms() -> int:
    return int(datetime.now(tz=timezone.utc).timestamp() * 1000)


def floor_window_start_ms(value_ms: int) -> int:
    return value_ms - (value_ms % WINDOW_MS)


def parse_iso_ms(value: Any) -> int | None:
    if not value or not isinstance(value, str):
        return None
    try:
        return int(datetime.fromisoformat(value.replace("Z", "+00:00")).timestamp() * 1000)
    except ValueError:
        return None


def parse_start_ms_from_slug(slug: str | None) -> int | None:
    if not slug:
        return None
    match = re.search(r"(\d{10})$", slug)
    if not match:
        return None
    return int(match.group(1)) * 1000


def parse_token_ids(raw: Any) -> list[str]:
    if raw is None:
        return []
    if isinstance(raw, list):
        return [str(item) for item in raw if str(item).strip()]
    if isinstance(raw, str):
        raw = raw.strip()
        if not raw:
            return []
        try:
            loaded = json.loads(raw)
        except json.JSONDecodeError:
            return [part.strip() for part in raw.split(",") if part.strip()]
        return parse_token_ids(loaded)
    return []


def pick_float(record: dict[str, Any], *keys: str) -> float | None:
    for key in keys:
        value = record.get(key)
        if value is None:
            continue
        try:
            return float(value)
        except (TypeError, ValueError):
            continue
    return None


@dataclass
class MarketRecord:
    market_id: str
    slug: str
    instrument_ids: list[str]
    event_start_time_ms: int | None
    event_end_time_ms: int | None
    price_to_beat: float | None
    final_price: float | None

    @property
    def is_active(self) -> bool:
        if self.event_start_time_ms is None or self.event_end_time_ms is None:
            return False
        now = now_ms()
        return self.event_start_time_ms <= now <= self.event_end_time_ms


def normalize_market_record(record: dict[str, Any]) -> MarketRecord | None:
    slug = str(record.get("slug") or "").strip()
    market_id = str(record.get("id") or record.get("market_id") or "").strip()
    if not slug or not market_id or not slug.startswith(SLUG_PREFIX):
        return None

    token_ids = parse_token_ids(
        record.get("clobTokenIds")
        or record.get("clobTokenIdsJson")
        or record.get("token_ids_json")
        or record.get("tokenIds")
    )
    if len(token_ids) < 2:
        return None

    start_ms = (
        parse_iso_ms(record.get("event_start_time"))
        or parse_iso_ms(record.get("eventStartTime"))
        or parse_iso_ms(record.get("startTime"))
        or parse_iso_ms(record.get("startDate"))
        or parse_iso_ms(record.get("start_date"))
        or parse_start_ms_from_slug(slug)
    )
    end_ms = (
        parse_iso_ms(record.get("endDate"))
        or parse_iso_ms(record.get("end_date"))
        or parse_iso_ms(record.get("endTime"))
        or parse_iso_ms(record.get("end_time"))
    )
    if start_ms is None and end_ms is not None:
        start_ms = end_ms - WINDOW_MS
    if end_ms is None and start_ms is not None:
        end_ms = start_ms + WINDOW_MS

    return MarketRecord(
        market_id=market_id,
        slug=slug,
        instrument_ids=token_ids[:2],
        event_start_time_ms=start_ms,
        event_end_time_ms=end_ms,
        price_to_beat=pick_float(record, "priceToBeat", "price_to_beat"),
        final_price=pick_float(record, "finalPrice", "final_price"),
    )


def fetch_market_by_slug(slug: str) -> list[MarketRecord]:
    params = urllib.parse.urlencode({"slug": slug})
    url = f"{GAMMA_API}?{params}"
    request = urllib.request.Request(
        url,
        headers={
            "User-Agent": "polymarket-agent/1.0",
            "Accept": "application/json",
        },
    )
    with urllib.request.urlopen(request, timeout=20) as response:
        payload = json.loads(response.read().decode("utf-8"))
    if not isinstance(payload, list):
        return []
    batch = [normalize_market_record(item) for item in payload if isinstance(item, dict)]
    return [item for item in batch if item is not None]


def fetch_live_markets(include_prev: int, include_next: int) -> list[MarketRecord]:
    current_start_ms = floor_window_start_ms(now_ms())
    candidate_slugs = []
    for offset in range(-include_prev, include_next + 1):
        start_ms = current_start_ms + (offset * WINDOW_MS)
        start_seconds = start_ms // 1000
        candidate_slugs.append(f"{SLUG_PREFIX}{start_seconds}")

    records: list[MarketRecord] = []
    for slug in candidate_slugs:
        records.extend(fetch_market_by_slug(slug))
    return dedupe_records(records)


def fetch_db_markets(db_path: Path, limit: int = 12) -> list[MarketRecord]:
    conn = sqlite3.connect(str(db_path))
    conn.row_factory = sqlite3.Row
    rows = conn.execute(
        """
        SELECT
            market_id,
            slug,
            token_ids_json,
            event_start_time,
            end_time,
            price_to_beat,
            final_price
        FROM wallet_market_catalog
        WHERE asset = 'BTC' AND market_family = 'updown_5m'
        ORDER BY end_time DESC
        LIMIT ?
        """,
        (limit,),
    ).fetchall()
    records = []
    for row in rows:
        records.append(
            normalize_market_record(
                {
                    "id": row["market_id"],
                    "slug": row["slug"],
                    "token_ids_json": row["token_ids_json"],
                    "event_start_time": row["event_start_time"],
                    "end_time": row["end_time"],
                    "price_to_beat": row["price_to_beat"],
                    "final_price": row["final_price"],
                }
            )
        )
    conn.close()
    return dedupe_records([item for item in records if item is not None])


def dedupe_records(records: list[MarketRecord]) -> list[MarketRecord]:
    deduped: dict[str, MarketRecord] = {}
    for record in records:
        deduped[record.market_id] = record
    return sorted(
        deduped.values(),
        key=lambda item: (
            item.event_start_time_ms or 0,
            item.event_end_time_ms or 0,
            item.market_id,
        ),
    )


def select_runtime_markets(
    records: list[MarketRecord],
    include_prev: int = 1,
    include_next: int = 1,
) -> list[MarketRecord]:
    now = now_ms()
    previous = [
        item
        for item in records
        if item.event_end_time_ms is not None and item.event_end_time_ms < now
    ]
    active = [
        item
        for item in records
        if item.event_start_time_ms is not None
        and item.event_end_time_ms is not None
        and item.event_start_time_ms <= now <= item.event_end_time_ms
    ]
    upcoming = [
        item
        for item in records
        if item.event_start_time_ms is not None and item.event_start_time_ms > now
    ]
    previous.sort(key=lambda item: item.event_end_time_ms or 0, reverse=True)
    upcoming.sort(key=lambda item: item.event_start_time_ms or 0)
    selected = list(reversed(previous[:include_prev]))
    selected.extend(active)
    selected.extend(upcoming[:include_next])
    if not selected and records:
        selected = records[:1]
    return dedupe_records(selected)


def market_context_payload(markets: list[MarketRecord], source: str) -> dict[str, Any]:
    generated_at = now_ms()
    return {
        "version": "btc_5m_mm_v1",
        "source": source,
        "source_generated_at_ms": generated_at,
        "markets": [
            {
                "market_id": record.market_id,
                "instrument_ids": record.instrument_ids,
                "price_to_beat": record.price_to_beat,
                "final_price": record.final_price,
                "event_start_time_ms": record.event_start_time_ms,
                "event_end_time_ms": record.event_end_time_ms,
            }
            for record in markets
        ],
        "active_btc_5m_windows": [
            {
                "market_id": record.market_id,
                "instrument_ids": record.instrument_ids,
                "price_to_beat": record.price_to_beat,
                "final_price": record.final_price,
                "event_start_time_ms": record.event_start_time_ms,
                "event_end_time_ms": record.event_end_time_ms,
            }
            for record in markets
            if record.is_active
        ],
    }


def runtime_env_lines(markets: list[MarketRecord]) -> list[str]:
    asset_ids: list[str] = []
    instrument_market_pairs: list[str] = []
    market_ids: list[str] = []
    latest = markets[0] if markets else None
    if markets:
        latest = min(
            markets,
            key=lambda record: (
                record.event_end_time_ms if record.is_active else float("inf"),
                record.event_start_time_ms or 0,
            ),
        )
    for market in markets:
        market_ids.append(market.market_id)
        for token_id in market.instrument_ids:
            asset_ids.append(token_id)
            instrument_market_pairs.append(f"{token_id}:{market.market_id}")

    lines = [
        f"PM_BTC_5M_ASSET_IDS={','.join(asset_ids)}",
        f"PM_BTC_5M_INSTRUMENT_MARKETS={','.join(instrument_market_pairs)}",
        f"PM_BTC_5M_USER_MARKETS={','.join(market_ids)}",
    ]
    if latest is not None:
        lines.append(f"PM_BTC_5M_LATEST_MARKET_SLUG={latest.slug}")
        if latest.event_end_time_ms is not None:
            end_iso = datetime.fromtimestamp(
                latest.event_end_time_ms / 1000.0,
                tz=timezone.utc,
            ).isoformat()
            lines.append(f"PM_BTC_5M_LATEST_MARKET_END_TIME={end_iso}")
    return lines


def write_text(path: Path, text: str) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(text)


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--source", choices=("live", "db"), default="live")
    parser.add_argument("--db", type=Path, default=Path("data/research/wallet_research/unlawful-shear/wallet_research.db"))
    parser.add_argument("--context-out", type=Path, required=True)
    parser.add_argument("--env-out", type=Path, required=True)
    parser.add_argument("--include-prev", type=int, default=1)
    parser.add_argument("--include-next", type=int, default=1)
    args = parser.parse_args()

    if args.source == "live":
        records = fetch_live_markets(
            include_prev=args.include_prev,
            include_next=args.include_next,
        )
        source_name = "gamma-api:exact-btc-5m-slugs"
    else:
        records = fetch_db_markets(args.db)
        source_name = f"sqlite:{args.db}"

    selected = select_runtime_markets(
        records,
        include_prev=args.include_prev,
        include_next=args.include_next,
    )
    if not selected:
        print("no BTC 5m markets found for runtime export", file=sys.stderr)
        return 1

    context = market_context_payload(selected, source_name)
    write_text(args.context_out, json.dumps(context, indent=2) + "\n")
    write_text(args.env_out, "\n".join(runtime_env_lines(selected)) + "\n")

    summary = {
        "source": source_name,
        "market_count": len(selected),
        "slugs": [record.slug for record in selected],
        "active_market_count": sum(1 for record in selected if record.is_active),
        "context_out": str(args.context_out),
        "env_out": str(args.env_out),
    }
    print(json.dumps(summary, indent=2))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
