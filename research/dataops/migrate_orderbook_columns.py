"""Idempotent migration to add orderbook columns to existing coin dbs.

PolyBackTest snapshot responses include orderbook_up and orderbook_down
objects with full bid/ask depth. The original fetcher discarded them,
so every snapshot row in btc.db and eth.db had only midpoint price_up
and price_down. This migration adds columns to hold top-of-book prices
and sizes plus the full orderbook JSON, so the patched fetcher can
persist the data going forward and honest execution simulation becomes
possible.

Safe to re-run: checks PRAGMA table_info before adding each column.

Usage:
    python3 scripts/migrate_orderbook_columns.py
    python3 scripts/migrate_orderbook_columns.py --coin btc
    python3 scripts/migrate_orderbook_columns.py --db /path/to/some.db
"""
from __future__ import annotations

import argparse
import logging
import sqlite3
import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).parent.parent))

from shared.constants import COINS, db_path

logger = logging.getLogger(__name__)

ORDERBOOK_COLUMNS: list[tuple[str, str]] = [
    ("best_bid_up", "REAL"),
    ("best_ask_up", "REAL"),
    ("bid_size_up", "REAL"),
    ("ask_size_up", "REAL"),
    ("best_bid_down", "REAL"),
    ("best_ask_down", "REAL"),
    ("bid_size_down", "REAL"),
    ("ask_size_down", "REAL"),
    ("orderbook_up_json", "TEXT"),
    ("orderbook_down_json", "TEXT"),
]


def _existing_columns(conn: sqlite3.Connection, table: str) -> set[str]:
    return {row[1] for row in conn.execute(f"PRAGMA table_info({table})").fetchall()}


def migrate_db(path: Path) -> dict[str, list[str]]:
    if not path.exists():
        logger.warning("db not found, skipping: %s", path)
        return {"added": [], "skipped": [], "error": ["db not found"]}

    conn = sqlite3.connect(str(path))
    try:
        table_exists = conn.execute(
            "SELECT name FROM sqlite_master WHERE type='table' AND name='snapshots'"
        ).fetchone() is not None
        if not table_exists:
            logger.info("  no snapshots table, skipping (fresh db will get new schema)")
            return {"added": [], "skipped": [], "missing": []}
        existing = _existing_columns(conn, "snapshots")
        added: list[str] = []
        skipped: list[str] = []
        for name, type_ in ORDERBOOK_COLUMNS:
            if name in existing:
                skipped.append(name)
                continue
            conn.execute(f"ALTER TABLE snapshots ADD COLUMN {name} {type_}")
            added.append(name)
        conn.commit()
        final = _existing_columns(conn, "snapshots")
        missing = [name for name, _ in ORDERBOOK_COLUMNS if name not in final]
        return {"added": added, "skipped": skipped, "missing": missing}
    finally:
        conn.close()


def main() -> None:
    logging.basicConfig(level=logging.INFO, format="%(asctime)s %(levelname)s %(message)s")
    parser = argparse.ArgumentParser()
    parser.add_argument("--coin", action="append", default=[], choices=COINS)
    parser.add_argument("--db", type=Path, default=None, help="Explicit db path override")
    args = parser.parse_args()

    if args.db:
        targets = [args.db]
    else:
        coins = args.coin or COINS
        targets = [db_path(coin) for coin in coins]

    for path in targets:
        logger.info("migrating %s", path)
        result = migrate_db(path)
        logger.info(
            "  added=%d skipped=%d missing=%d",
            len(result["added"]),
            len(result["skipped"]),
            len(result.get("missing", [])),
        )
        if result["added"]:
            logger.info("  added columns: %s", ", ".join(result["added"]))
        if result.get("missing"):
            logger.error("  STILL MISSING: %s", ", ".join(result["missing"]))


if __name__ == "__main__":
    main()
