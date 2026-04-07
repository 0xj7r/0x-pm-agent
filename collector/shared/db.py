"""Database connection and schema management."""
from __future__ import annotations

import sqlite3
from pathlib import Path


def get_connection(db_path: Path) -> sqlite3.Connection:
    """Create a connection with standard settings."""
    conn = sqlite3.connect(str(db_path))
    conn.execute("PRAGMA journal_mode=WAL")
    conn.execute("PRAGMA synchronous=NORMAL")
    conn.row_factory = sqlite3.Row
    return conn


def init_coin_db(db_path: Path) -> sqlite3.Connection:
    """Create or open a coin DB with the standard schema."""
    conn = get_connection(db_path)
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


def load_markets(db_path: Path) -> list[dict]:
    """Load all resolved markets from a coin DB."""
    conn = get_connection(db_path)
    rows = conn.execute(
        "SELECT * FROM markets WHERE winner IS NOT NULL ORDER BY start_time"
    ).fetchall()
    result = [dict(r) for r in rows]
    conn.close()
    return result


def load_snapshots(market_id: str, db_path: Path) -> list[dict]:
    """Load snapshots for a specific market."""
    conn = get_connection(db_path)
    rows = conn.execute(
        "SELECT * FROM snapshots WHERE market_id = ? ORDER BY time",
        (market_id,),
    ).fetchall()
    result = [dict(r) for r in rows]
    conn.close()
    return result


def snapshot_price_col(db_path: Path) -> str:
    """Detect whether snapshots use 'price' or 'btc_price' column."""
    conn = get_connection(db_path)
    cols = [c[1] for c in conn.execute("PRAGMA table_info(snapshots)").fetchall()]
    conn.close()
    return "price" if "price" in cols else "btc_price"


def market_start_col(db_path: Path) -> str:
    """Detect whether markets use 'price_start' or 'btc_price_start' column."""
    conn = get_connection(db_path)
    cols = [c[1] for c in conn.execute("PRAGMA table_info(markets)").fetchall()]
    conn.close()
    return "price_start" if "price_start" in cols else "btc_price_start"
