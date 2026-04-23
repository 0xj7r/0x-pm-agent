#!/usr/bin/env python3
"""Export Rust execution market context from wallet research SQLite."""

from __future__ import annotations

import argparse
import json
import sqlite3
from datetime import datetime, timezone
from pathlib import Path


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser()
    parser.add_argument("--db", required=True, help="Path to wallet_research.db")
    parser.add_argument("--out", required=True, help="Output JSON path")
    parser.add_argument(
        "--series-slug",
        default="btc-up-or-down-5m",
        help="Filter series_slug for export",
    )
    return parser.parse_args()


def iso_to_ms(value: str | None) -> int | None:
    if not value:
        return None
    normalized = value.replace("Z", "+00:00")
    try:
        dt = datetime.fromisoformat(normalized)
    except ValueError:
        return None
    if dt.tzinfo is None:
        dt = dt.replace(tzinfo=timezone.utc)
    return int(dt.timestamp() * 1000)


def main() -> None:
    args = parse_args()
    db_path = Path(args.db)
    out_path = Path(args.out)
    conn = sqlite3.connect(db_path)
    conn.row_factory = sqlite3.Row
    rows = conn.execute(
        """
        SELECT
          market_id,
          slug,
          series_slug,
          token_ids_json,
          price_to_beat,
          final_price,
          event_start_time,
          end_time,
          closed_time
        FROM wallet_market_catalog
        WHERE series_slug = ?
          AND market_id IS NOT NULL
        ORDER BY event_start_time, slug
        """,
        (args.series_slug,),
    ).fetchall()
    payload = []
    for row in rows:
        payload.append(
            {
                "market_id": str(row["market_id"]),
                "instrument_ids": json.loads(row["token_ids_json"]) if row["token_ids_json"] else [],
                "slug": row["slug"],
                "series_slug": row["series_slug"],
                "price_to_beat": row["price_to_beat"],
                "final_price": row["final_price"],
                "event_start_time_ms": iso_to_ms(row["event_start_time"]),
                "event_end_time_ms": iso_to_ms(row["closed_time"] or row["end_time"]),
            }
        )
    out_path.parent.mkdir(parents=True, exist_ok=True)
    out_path.write_text(json.dumps(payload, indent=2))
    print(
        json.dumps(
            {
                "db": str(db_path),
                "out": str(out_path),
                "rows": len(payload),
                "series_slug": args.series_slug,
            }
        )
    )


if __name__ == "__main__":
    main()
