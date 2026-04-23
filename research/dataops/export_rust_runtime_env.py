#!/usr/bin/env python3
"""Export Rust runtime env values for the latest BTC 5m market."""

from __future__ import annotations

import argparse
import json
import shlex
import sqlite3
from pathlib import Path


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser()
    parser.add_argument("--db", required=True, help="Path to wallet_research.db")
    parser.add_argument(
        "--series-slug",
        default="btc-up-or-down-5m",
        help="Filter series_slug for export",
    )
    parser.add_argument(
        "--out",
        help="Optional path to write KEY=VALUE lines",
    )
    return parser.parse_args()


def main() -> None:
    args = parse_args()
    conn = sqlite3.connect(Path(args.db))
    conn.row_factory = sqlite3.Row
    row = conn.execute(
        """
        SELECT market_id, slug, token_ids_json, event_start_time, closed_time, end_time
        FROM wallet_market_catalog
        WHERE series_slug = ?
          AND token_ids_json IS NOT NULL
          AND token_ids_json != ''
        ORDER BY event_start_time DESC, market_id DESC
        LIMIT 1
        """,
        (args.series_slug,),
    ).fetchone()
    if row is None:
        raise SystemExit(f"no market row found for series_slug={args.series_slug}")

    token_ids = json.loads(row["token_ids_json"])
    mapping = ",".join(f"{token_id}:{row['market_id']}" for token_id in token_ids)
    payload = {
        "WHALE_PAIR_ASSET_IDS": ",".join(token_ids),
        "WHALE_PAIR_INSTRUMENT_MARKETS": mapping,
        "WHALE_PAIR_USER_MARKETS": str(row["market_id"]),
        "WHALE_PAIR_LATEST_MARKET_SLUG": row["slug"],
        "WHALE_PAIR_LATEST_MARKET_END_TIME": row["closed_time"] or row["end_time"],
    }

    lines = [f"{key}={shlex.quote(str(value))}" for key, value in payload.items()]
    if args.out:
        out_path = Path(args.out)
        out_path.parent.mkdir(parents=True, exist_ok=True)
        out_path.write_text("\n".join(lines) + "\n")
    print(json.dumps(payload, indent=2))


if __name__ == "__main__":
    main()
