"""SQLite ledger for whale-family two-sided pair strategies."""
from __future__ import annotations

import json
from datetime import datetime, timezone
from pathlib import Path
import sqlite3
from typing import Any

from strategies.whale_pair import FillDecision, OpenLot, WhalePairMarketState


def init_db(db_path: str) -> sqlite3.Connection:
    Path(db_path).parent.mkdir(parents=True, exist_ok=True)
    conn = sqlite3.connect(db_path)
    _configure_connection(conn)
    init_schema(conn)
    return conn


def _configure_connection(conn: sqlite3.Connection) -> None:
    conn.row_factory = sqlite3.Row
    conn.execute("PRAGMA journal_mode=WAL")
    conn.execute("PRAGMA synchronous=NORMAL")


def _existing_columns(conn: sqlite3.Connection, table: str) -> set[str]:
    return {
        str(row[1])
        for row in conn.execute(f"PRAGMA table_info({table})").fetchall()
    }


def _ensure_column(
    conn: sqlite3.Connection,
    *,
    table: str,
    name: str,
    type_sql: str,
) -> None:
    if name in _existing_columns(conn, table):
        return
    conn.execute(f"ALTER TABLE {table} ADD COLUMN {name} {type_sql}")


def _row_to_dict(row: sqlite3.Row | None) -> dict[str, Any] | None:
    if row is None:
        return None
    return {key: row[key] for key in row.keys()}


def _decode_action_payload(payload_json: str) -> Any:
    try:
        return json.loads(payload_json)
    except (TypeError, ValueError, json.JSONDecodeError):
        return payload_json


def _action_row_to_dict(row: sqlite3.Row) -> dict[str, Any]:
    payload_json = str(row["payload_json"])
    return {
        "id": int(row["id"]),
        "market_id": str(row["market_id"]),
        "ts": str(row["ts"]),
        "action_type": str(row["action_type"]),
        "action_ref": row["action_ref"],
        "payload_json": payload_json,
        "payload": _decode_action_payload(payload_json),
    }


def init_schema(conn: sqlite3.Connection) -> None:
    _configure_connection(conn)
    conn.execute(
        """
        CREATE TABLE IF NOT EXISTS whale_pair_markets (
            market_id TEXT PRIMARY KEY,
            condition_id TEXT NOT NULL,
            event_slug TEXT NOT NULL,
            event_id TEXT NOT NULL,
            window_start_ts INTEGER NOT NULL,
            window_end_ts INTEGER NOT NULL,
            resolved INTEGER NOT NULL DEFAULT 0,
            winning_outcome TEXT,
            merged_pnl_usd REAL,
            residual_pnl_usd REAL
        )
        """
    )
    conn.execute(
        """
        CREATE TABLE IF NOT EXISTS whale_pair_fills (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            market_id TEXT NOT NULL,
            ts TEXT NOT NULL,
            side TEXT NOT NULL,
            reason TEXT NOT NULL,
            price REAL NOT NULL,
            ask_size REAL NOT NULL,
            shares REAL NOT NULL,
            gross_cost_usd REAL NOT NULL,
            fee_usd REAL NOT NULL,
            token_id TEXT,
            order_id TEXT,
            client_order_id TEXT
        )
        """
    )
    conn.execute(
        """
        CREATE TABLE IF NOT EXISTS whale_pair_open_lots (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            market_id TEXT NOT NULL,
            fill_id INTEGER NOT NULL,
            side TEXT NOT NULL,
            opened_ts TEXT NOT NULL,
            shares_remaining REAL NOT NULL,
            gross_cost_remaining_usd REAL NOT NULL,
            fee_remaining_usd REAL NOT NULL
        )
        """
    )
    conn.execute(
        """
        CREATE TABLE IF NOT EXISTS whale_pair_matches (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            market_id TEXT NOT NULL,
            matched_ts TEXT NOT NULL,
            up_fill_id INTEGER,
            down_fill_id INTEGER,
            shares REAL NOT NULL,
            up_cost_usd REAL NOT NULL,
            up_fee_usd REAL NOT NULL,
            down_cost_usd REAL NOT NULL,
            down_fee_usd REAL NOT NULL,
            payout_usd REAL NOT NULL,
            realized_pnl_usd REAL NOT NULL
        )
        """
    )
    conn.execute(
        """
        CREATE TABLE IF NOT EXISTS whale_pair_actions (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            market_id TEXT NOT NULL,
            ts TEXT NOT NULL,
            action_type TEXT NOT NULL,
            action_ref TEXT,
            payload_json TEXT NOT NULL
        )
        """
    )
    conn.execute(
        """
        CREATE TABLE IF NOT EXISTS whale_pair_redeems (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            market_id TEXT NOT NULL,
            condition_id TEXT NOT NULL,
            winning_outcome TEXT NOT NULL,
            shares_redeemed REAL NOT NULL,
            payout_usd REAL NOT NULL,
            tx_hash TEXT,
            status TEXT,
            ts TEXT NOT NULL
        )
        """
    )
    _ensure_column(conn, table="whale_pair_fills", name="token_id", type_sql="TEXT")
    _ensure_column(conn, table="whale_pair_fills", name="order_id", type_sql="TEXT")
    _ensure_column(
        conn,
        table="whale_pair_fills",
        name="client_order_id",
        type_sql="TEXT",
    )
    _ensure_column(
        conn,
        table="whale_pair_actions",
        name="action_ref",
        type_sql="TEXT",
    )
    _ensure_column(conn, table="whale_pair_matches", name="tx_hash", type_sql="TEXT")
    _ensure_column(conn, table="whale_pair_matches", name="tx_status", type_sql="TEXT")
    conn.execute(
        """
        CREATE INDEX IF NOT EXISTS idx_whale_pair_fills_market_id
        ON whale_pair_fills (market_id, id)
        """
    )
    conn.execute(
        """
        CREATE INDEX IF NOT EXISTS idx_whale_pair_open_lots_market_side
        ON whale_pair_open_lots (market_id, side, id)
        """
    )
    conn.execute(
        """
        CREATE INDEX IF NOT EXISTS idx_whale_pair_matches_market_id
        ON whale_pair_matches (market_id, id)
        """
    )
    conn.execute(
        """
        CREATE INDEX IF NOT EXISTS idx_whale_pair_actions_market_id
        ON whale_pair_actions (market_id, id)
        """
    )
    conn.execute(
        """
        CREATE INDEX IF NOT EXISTS idx_whale_pair_actions_ref
        ON whale_pair_actions (action_ref, id)
        """
    )
    conn.execute(
        """
        CREATE INDEX IF NOT EXISTS idx_whale_pair_redeems_market_id
        ON whale_pair_redeems (market_id, id)
        """
    )
    conn.commit()


def ensure_market_row(
    conn: sqlite3.Connection,
    *,
    market_id: str,
    condition_id: str,
    event_slug: str,
    event_id: str,
    window_start_ts: int,
    window_end_ts: int,
) -> None:
    conn.execute(
        """
        INSERT OR IGNORE INTO whale_pair_markets (
            market_id, condition_id, event_slug, event_id, window_start_ts, window_end_ts, resolved
        ) VALUES (?, ?, ?, ?, ?, ?, 0)
        """,
        (
            market_id,
            condition_id,
            event_slug,
            event_id,
            window_start_ts,
            window_end_ts,
        ),
    )
    conn.commit()


def market_gross_cost_usd(conn: sqlite3.Connection, market_id: str) -> float:
    row = conn.execute(
        "SELECT COALESCE(SUM(gross_cost_usd), 0.0) FROM whale_pair_fills WHERE market_id = ?",
        (market_id,),
    ).fetchone()
    return float(row[0] or 0.0)


def build_market_state(conn: sqlite3.Connection, market_id: str) -> WhalePairMarketState:
    state = WhalePairMarketState(gross_cost_usd=market_gross_cost_usd(conn, market_id))
    rows = conn.execute(
        """
        SELECT side, shares_remaining, gross_cost_remaining_usd, fee_remaining_usd
        FROM whale_pair_open_lots
        WHERE market_id = ? AND shares_remaining > 0
        ORDER BY id
        """,
        (market_id,),
    ).fetchall()
    for side, shares_remaining, gross_cost_remaining_usd, fee_remaining_usd in rows:
        lot = OpenLot(
            side=str(side),
            shares_remaining=float(shares_remaining),
            gross_cost_remaining_usd=float(gross_cost_remaining_usd),
            fee_remaining_usd=float(fee_remaining_usd),
        )
        if str(side) == "Up":
            state.up_lots.append(lot)
        else:
            state.down_lots.append(lot)
    return state


def load_market(conn: sqlite3.Connection, market_id: str) -> dict[str, Any] | None:
    row = conn.execute(
        """
        SELECT
            market_id,
            condition_id,
            event_slug,
            event_id,
            window_start_ts,
            window_end_ts,
            resolved,
            winning_outcome,
            merged_pnl_usd,
            residual_pnl_usd
        FROM whale_pair_markets
        WHERE market_id = ?
        LIMIT 1
        """,
        (market_id,),
    ).fetchone()
    return _row_to_dict(row)


def load_fills(
    conn: sqlite3.Connection,
    market_id: str,
    *,
    side: str | None = None,
    order_id: str | None = None,
) -> list[dict[str, Any]]:
    sql = """
        SELECT
            id,
            market_id,
            ts,
            side,
            reason,
            price,
            ask_size,
            shares,
            gross_cost_usd,
            fee_usd,
            token_id,
            order_id,
            client_order_id
        FROM whale_pair_fills
        WHERE market_id = ?
    """
    params: list[Any] = [market_id]
    if side is not None:
        sql += " AND side = ?"
        params.append(side)
    if order_id is not None:
        sql += " AND order_id = ?"
        params.append(order_id)
    sql += " ORDER BY id"
    rows = conn.execute(sql, tuple(params)).fetchall()
    return [_row_to_dict(row) for row in rows if row is not None]


def load_matches(conn: sqlite3.Connection, market_id: str) -> list[dict[str, Any]]:
    rows = conn.execute(
        """
        SELECT
            id,
            market_id,
            matched_ts,
            up_fill_id,
            down_fill_id,
            shares,
            up_cost_usd,
            up_fee_usd,
            down_cost_usd,
            down_fee_usd,
            payout_usd,
            realized_pnl_usd,
            tx_hash,
            tx_status
        FROM whale_pair_matches
        WHERE market_id = ?
        ORDER BY id
        """,
        (market_id,),
    ).fetchall()
    return [_row_to_dict(row) for row in rows if row is not None]


def load_open_lots(
    conn: sqlite3.Connection,
    market_id: str,
    *,
    side: str | None = None,
    include_closed: bool = False,
) -> list[dict[str, Any]]:
    sql = """
        SELECT
            id,
            market_id,
            fill_id,
            side,
            opened_ts,
            shares_remaining,
            gross_cost_remaining_usd,
            fee_remaining_usd
        FROM whale_pair_open_lots
        WHERE market_id = ?
    """
    params: list[Any] = [market_id]
    if side is not None:
        sql += " AND side = ?"
        params.append(side)
    if not include_closed:
        sql += " AND shares_remaining > 0"
    sql += " ORDER BY id"
    rows = conn.execute(sql, tuple(params)).fetchall()
    out: list[dict[str, Any]] = []
    for row in rows:
        payload = _row_to_dict(row)
        if payload is None:
            continue
        payload["all_in_cost_remaining_usd"] = float(
            payload["gross_cost_remaining_usd"] or 0.0
        ) + float(payload["fee_remaining_usd"] or 0.0)
        out.append(payload)
    return out


def summarize_open_lots(conn: sqlite3.Connection, market_id: str) -> dict[str, Any]:
    sides = {
        "Up": {
            "lot_count": 0,
            "shares_remaining": 0.0,
            "gross_cost_remaining_usd": 0.0,
            "fee_remaining_usd": 0.0,
            "all_in_cost_remaining_usd": 0.0,
        },
        "Down": {
            "lot_count": 0,
            "shares_remaining": 0.0,
            "gross_cost_remaining_usd": 0.0,
            "fee_remaining_usd": 0.0,
            "all_in_cost_remaining_usd": 0.0,
        },
    }
    rows = conn.execute(
        """
        SELECT
            side,
            COUNT(*) AS lot_count,
            COALESCE(SUM(shares_remaining), 0.0) AS shares_remaining,
            COALESCE(SUM(gross_cost_remaining_usd), 0.0) AS gross_cost_remaining_usd,
            COALESCE(SUM(fee_remaining_usd), 0.0) AS fee_remaining_usd
        FROM whale_pair_open_lots
        WHERE market_id = ? AND shares_remaining > 0
        GROUP BY side
        """,
        (market_id,),
    ).fetchall()
    for row in rows:
        side = str(row["side"])
        if side not in sides:
            continue
        gross = float(row["gross_cost_remaining_usd"] or 0.0)
        fee = float(row["fee_remaining_usd"] or 0.0)
        sides[side] = {
            "lot_count": int(row["lot_count"] or 0),
            "shares_remaining": float(row["shares_remaining"] or 0.0),
            "gross_cost_remaining_usd": gross,
            "fee_remaining_usd": fee,
            "all_in_cost_remaining_usd": gross + fee,
        }

    total_shares = (
        sides["Up"]["shares_remaining"] + sides["Down"]["shares_remaining"]
    )
    total_gross = (
        sides["Up"]["gross_cost_remaining_usd"]
        + sides["Down"]["gross_cost_remaining_usd"]
    )
    total_fee = (
        sides["Up"]["fee_remaining_usd"]
        + sides["Down"]["fee_remaining_usd"]
    )
    return {
        "market_id": market_id,
        "lot_count": int(sides["Up"]["lot_count"] + sides["Down"]["lot_count"]),
        "total_shares_remaining": total_shares,
        "total_gross_cost_remaining_usd": total_gross,
        "total_fee_remaining_usd": total_fee,
        "total_all_in_cost_remaining_usd": total_gross + total_fee,
        "net_share_imbalance": (
            sides["Up"]["shares_remaining"] - sides["Down"]["shares_remaining"]
        ),
        "is_flat": total_shares <= 0,
        "sides": sides,
    }


def insert_fill(
    conn: sqlite3.Connection,
    market_id: str,
    fill: FillDecision,
    *,
    token_id: str | None = None,
    order_id: str | None = None,
    client_order_id: str | None = None,
    ts: str | None = None,
) -> int:
    ts = ts or datetime.now(timezone.utc).isoformat()
    cur = conn.execute(
        """
        INSERT INTO whale_pair_fills (
            market_id, ts, side, reason, price, ask_size, shares, gross_cost_usd, fee_usd,
            token_id, order_id, client_order_id
        ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
        """,
        (
            market_id,
            ts,
            fill.side,
            fill.reason,
            fill.price,
            fill.ask_size,
            fill.shares,
            fill.gross_cost_usd,
            fill.fee_usd,
            token_id,
            order_id,
            client_order_id,
        ),
    )
    fill_id = int(cur.lastrowid)
    conn.execute(
        """
        INSERT INTO whale_pair_open_lots (
            market_id, fill_id, side, opened_ts, shares_remaining,
            gross_cost_remaining_usd, fee_remaining_usd
        ) VALUES (?, ?, ?, ?, ?, ?, ?)
        """,
        (
            market_id,
            fill_id,
            fill.side,
            ts,
            fill.shares,
            fill.gross_cost_usd,
            fill.fee_usd,
        ),
    )
    conn.commit()
    return fill_id


def _oldest_open_lot(conn: sqlite3.Connection, market_id: str, side: str) -> tuple | None:
    return conn.execute(
        """
        SELECT id, fill_id, shares_remaining, gross_cost_remaining_usd, fee_remaining_usd
        FROM whale_pair_open_lots
        WHERE market_id = ? AND side = ? AND shares_remaining > 0
        ORDER BY id
        LIMIT 1
        """,
        (market_id, side),
    ).fetchone()


def match_and_merge(conn: sqlite3.Connection, market_id: str) -> int:
    return materialize_merge(
        conn,
        market_id,
        shares_to_match=None,
        tx_hash=None,
        tx_status=None,
    )


def materialize_merge(
    conn: sqlite3.Connection,
    market_id: str,
    *,
    shares_to_match: float | None,
    tx_hash: str | None,
    tx_status: str | None,
) -> int:
    remaining = float("inf") if shares_to_match is None else max(0.0, float(shares_to_match))
    matches = 0
    while remaining > 0:
        up = _oldest_open_lot(conn, market_id, "Up")
        down = _oldest_open_lot(conn, market_id, "Down")
        if up is None or down is None:
            break
        up_lot_id, up_fill_id, up_shares, up_cost, up_fee = up
        dn_lot_id, dn_fill_id, dn_shares, dn_cost, dn_fee = down
        shares = min(
            float(up_shares),
            float(dn_shares),
            remaining,
        )
        if shares <= 0:
            break
        up_ratio = shares / float(up_shares)
        down_ratio = shares / float(dn_shares)
        up_cost_used = float(up_cost) * up_ratio
        up_fee_used = float(up_fee) * up_ratio
        down_cost_used = float(dn_cost) * down_ratio
        down_fee_used = float(dn_fee) * down_ratio
        payout = shares
        conn.execute(
            """
            INSERT INTO whale_pair_matches (
                market_id, matched_ts, up_fill_id, down_fill_id, shares,
                up_cost_usd, up_fee_usd, down_cost_usd, down_fee_usd,
                payout_usd, realized_pnl_usd, tx_hash, tx_status
            ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
            """,
            (
                market_id,
                datetime.now(timezone.utc).isoformat(),
                up_fill_id,
                dn_fill_id,
                shares,
                up_cost_used,
                up_fee_used,
                down_cost_used,
                down_fee_used,
                payout,
                payout - up_cost_used - up_fee_used - down_cost_used - down_fee_used,
                tx_hash,
                tx_status,
            ),
        )
        conn.execute(
            """
            UPDATE whale_pair_open_lots
            SET shares_remaining = ?, gross_cost_remaining_usd = ?, fee_remaining_usd = ?
            WHERE id = ?
            """,
            (
                float(up_shares) - shares,
                float(up_cost) - up_cost_used,
                float(up_fee) - up_fee_used,
                up_lot_id,
            ),
        )
        conn.execute(
            """
            UPDATE whale_pair_open_lots
            SET shares_remaining = ?, gross_cost_remaining_usd = ?, fee_remaining_usd = ?
            WHERE id = ?
            """,
            (
                float(dn_shares) - shares,
                float(dn_cost) - down_cost_used,
                float(dn_fee) - down_fee_used,
                dn_lot_id,
            ),
        )
        conn.commit()
        matches += 1
        if shares_to_match is not None:
            remaining -= shares
            if remaining <= 1e-12:
                break
    return matches


def latest_match_id(conn: sqlite3.Connection, market_id: str) -> int:
    row = conn.execute(
        "SELECT COALESCE(MAX(id), 0) FROM whale_pair_matches WHERE market_id = ?",
        (market_id,),
    ).fetchone()
    return int(row[0] or 0)


def matched_shares_after(conn: sqlite3.Connection, market_id: str, match_id_floor: int) -> float:
    row = conn.execute(
        """
        SELECT COALESCE(SUM(shares), 0.0)
        FROM whale_pair_matches
        WHERE market_id = ? AND id > ?
        """,
        (market_id, match_id_floor),
    ).fetchone()
    return float(row[0] or 0.0)


def record_action(
    conn: sqlite3.Connection,
    market_id: str,
    action_type: str,
    payload_json: str,
    *,
    action_ref: str | None = None,
    ts: str | None = None,
) -> int:
    cur = conn.execute(
        """
        INSERT INTO whale_pair_actions (market_id, ts, action_type, action_ref, payload_json)
        VALUES (?, ?, ?, ?, ?)
        """,
        (
            market_id,
            ts or datetime.now(timezone.utc).isoformat(),
            action_type,
            action_ref,
            payload_json,
        ),
    )
    conn.commit()
    return int(cur.lastrowid)


def load_actions(
    conn: sqlite3.Connection,
    *,
    market_id: str | None = None,
    action_type: str | None = None,
    action_ref: str | None = None,
    after_id: int | None = None,
    limit: int | None = None,
    newest_first: bool = False,
) -> list[dict[str, Any]]:
    sql = """
        SELECT id, market_id, ts, action_type, action_ref, payload_json
        FROM whale_pair_actions
        WHERE 1 = 1
    """
    params: list[Any] = []
    if market_id is not None:
        sql += " AND market_id = ?"
        params.append(market_id)
    if action_type is not None:
        sql += " AND action_type = ?"
        params.append(action_type)
    if action_ref is not None:
        sql += " AND action_ref = ?"
        params.append(action_ref)
    if after_id is not None:
        sql += " AND id > ?"
        params.append(after_id)
    sql += " ORDER BY id DESC" if newest_first else " ORDER BY id ASC"
    if limit is not None and limit > 0:
        sql += " LIMIT ?"
        params.append(limit)
    rows = conn.execute(sql, tuple(params)).fetchall()
    out = [_action_row_to_dict(row) for row in rows]
    if newest_first:
        return out
    return out


def latest_action(
    conn: sqlite3.Connection,
    *,
    market_id: str,
    action_type: str | None = None,
) -> dict[str, Any] | None:
    rows = load_actions(
        conn,
        market_id=market_id,
        action_type=action_type,
        limit=1,
        newest_first=True,
    )
    return rows[0] if rows else None


def load_reconciliation_candidates(
    conn: sqlite3.Connection,
    *,
    limit: int | None = None,
) -> list[dict[str, Any]]:
    sql = """
        WITH lot_summary AS (
            SELECT
                market_id,
                COUNT(*) AS open_lot_count,
                COALESCE(SUM(shares_remaining), 0.0) AS total_shares_remaining,
                COALESCE(SUM(gross_cost_remaining_usd), 0.0) AS total_gross_cost_remaining_usd,
                COALESCE(SUM(fee_remaining_usd), 0.0) AS total_fee_remaining_usd,
                COALESCE(SUM(CASE WHEN side = 'Up' THEN shares_remaining ELSE 0.0 END), 0.0) AS up_shares_remaining,
                COALESCE(SUM(CASE WHEN side = 'Down' THEN shares_remaining ELSE 0.0 END), 0.0) AS down_shares_remaining
            FROM whale_pair_open_lots
            WHERE shares_remaining > 0
            GROUP BY market_id
        ),
        fill_summary AS (
            SELECT
                market_id,
                COUNT(*) AS fill_count,
                COALESCE(MAX(id), 0) AS latest_fill_id
            FROM whale_pair_fills
            GROUP BY market_id
        ),
        match_summary AS (
            SELECT
                market_id,
                COUNT(*) AS match_count,
                COALESCE(SUM(shares), 0.0) AS matched_shares,
                COALESCE(MAX(id), 0) AS latest_match_id
            FROM whale_pair_matches
            GROUP BY market_id
        ),
        latest_action_ids AS (
            SELECT market_id, MAX(id) AS latest_action_id
            FROM whale_pair_actions
            GROUP BY market_id
        )
        SELECT
            m.market_id,
            m.condition_id,
            m.event_slug,
            m.event_id,
            m.window_start_ts,
            m.window_end_ts,
            m.resolved,
            m.winning_outcome,
            m.merged_pnl_usd,
            m.residual_pnl_usd,
            COALESCE(ls.open_lot_count, 0) AS open_lot_count,
            COALESCE(ls.total_shares_remaining, 0.0) AS total_shares_remaining,
            COALESCE(ls.total_gross_cost_remaining_usd, 0.0) AS total_gross_cost_remaining_usd,
            COALESCE(ls.total_fee_remaining_usd, 0.0) AS total_fee_remaining_usd,
            COALESCE(ls.up_shares_remaining, 0.0) AS up_shares_remaining,
            COALESCE(ls.down_shares_remaining, 0.0) AS down_shares_remaining,
            COALESCE(fs.fill_count, 0) AS fill_count,
            COALESCE(fs.latest_fill_id, 0) AS latest_fill_id,
            COALESCE(ms.match_count, 0) AS match_count,
            COALESCE(ms.matched_shares, 0.0) AS matched_shares,
            COALESCE(ms.latest_match_id, 0) AS latest_match_id,
            a.id AS latest_action_id,
            a.ts AS latest_action_ts,
            a.action_type AS latest_action_type,
            a.action_ref AS latest_action_ref,
            a.payload_json AS latest_action_payload_json
        FROM whale_pair_markets m
        LEFT JOIN lot_summary ls
            ON ls.market_id = m.market_id
        LEFT JOIN fill_summary fs
            ON fs.market_id = m.market_id
        LEFT JOIN match_summary ms
            ON ms.market_id = m.market_id
        LEFT JOIN latest_action_ids lai
            ON lai.market_id = m.market_id
        LEFT JOIN whale_pair_actions a
            ON a.id = lai.latest_action_id
        WHERE m.resolved = 0 OR COALESCE(ls.total_shares_remaining, 0.0) > 0
        ORDER BY m.window_end_ts ASC, m.market_id ASC
    """
    params: list[Any] = []
    if limit is not None and limit > 0:
        sql += " LIMIT ?"
        params.append(limit)
    rows = conn.execute(sql, tuple(params)).fetchall()
    out: list[dict[str, Any]] = []
    for row in rows:
        payload = _row_to_dict(row)
        if payload is None:
            continue
        payload["resolved"] = bool(payload["resolved"])
        latest_action_payload = payload.pop("latest_action_payload_json", None)
        if latest_action_payload is not None:
            payload["latest_action_payload"] = _decode_action_payload(
                str(latest_action_payload)
            )
        else:
            payload["latest_action_payload"] = None
        payload["total_all_in_cost_remaining_usd"] = float(
            payload["total_gross_cost_remaining_usd"] or 0.0
        ) + float(payload["total_fee_remaining_usd"] or 0.0)
        payload["net_share_imbalance"] = float(
            payload["up_shares_remaining"] or 0.0
        ) - float(payload["down_shares_remaining"] or 0.0)
        out.append(payload)
    return out


def load_unresolved_markets(
    conn: sqlite3.Connection,
    *,
    now_ts: int | None = None,
    require_window_ended: bool = False,
    limit: int | None = None,
) -> list[dict[str, Any]]:
    """Return markets with resolved = 0.

    require_window_ended filters to windows whose window_end_ts has passed
    (caller supplies now_ts). Ordered by window_end_ts ASC so callers can
    drain resolution work oldest-first.
    """
    sql = """
        SELECT
            market_id,
            condition_id,
            event_slug,
            event_id,
            window_start_ts,
            window_end_ts,
            resolved,
            winning_outcome,
            merged_pnl_usd,
            residual_pnl_usd
        FROM whale_pair_markets
        WHERE resolved = 0
    """
    params: list[Any] = []
    if require_window_ended and now_ts is not None:
        sql += " AND window_end_ts <= ?"
        params.append(int(now_ts))
    sql += " ORDER BY window_end_ts ASC, market_id ASC"
    if limit is not None and limit > 0:
        sql += " LIMIT ?"
        params.append(int(limit))
    rows = conn.execute(sql, tuple(params)).fetchall()
    out: list[dict[str, Any]] = []
    for row in rows:
        payload = _row_to_dict(row)
        if payload is None:
            continue
        payload["resolved"] = bool(payload["resolved"])
        out.append(payload)
    return out


def open_inventory(
    conn: sqlite3.Connection,
    market_id: str,
    side: str,
) -> dict[str, Any]:
    """Aggregate open inventory for a single (market_id, side)."""
    row = conn.execute(
        """
        SELECT
            COUNT(*) AS lot_count,
            COALESCE(SUM(shares_remaining), 0.0) AS shares_remaining,
            COALESCE(SUM(gross_cost_remaining_usd), 0.0) AS gross_cost_remaining_usd,
            COALESCE(SUM(fee_remaining_usd), 0.0) AS fee_remaining_usd
        FROM whale_pair_open_lots
        WHERE market_id = ? AND side = ? AND shares_remaining > 0
        """,
        (market_id, side),
    ).fetchone()
    lot_count = int(row["lot_count"] or 0) if row is not None else 0
    shares = float(row["shares_remaining"] or 0.0) if row is not None else 0.0
    gross = float(row["gross_cost_remaining_usd"] or 0.0) if row is not None else 0.0
    fee = float(row["fee_remaining_usd"] or 0.0) if row is not None else 0.0
    avg_all_in = (gross + fee) / shares if shares > 0 else None
    return {
        "market_id": market_id,
        "side": side,
        "lot_count": lot_count,
        "shares_remaining": shares,
        "gross_cost_remaining_usd": gross,
        "fee_remaining_usd": fee,
        "all_in_cost_remaining_usd": gross + fee,
        "avg_all_in_cost_per_share": avg_all_in,
    }


def merge_ready_inventory(
    conn: sqlite3.Connection,
    market_id: str,
) -> dict[str, Any]:
    """Return how many shares are pair-matchable right now.

    The ledger's on-chain merge step burns the smaller of (Up shares, Down
    shares) 1-for-1 into $1 USDC. "Pair-matchable" is therefore
    min(up_shares_remaining, down_shares_remaining) across still-open lots.
    Residual is what's left on the larger side after the pair is taken.
    """
    up = open_inventory(conn, market_id, "Up")
    down = open_inventory(conn, market_id, "Down")
    pair_shares = min(up["shares_remaining"], down["shares_remaining"])
    payout_usd = pair_shares
    pair_cost_usd = 0.0
    if pair_shares > 0:
        if up["shares_remaining"] > 0:
            up_ratio = pair_shares / up["shares_remaining"]
            pair_cost_usd += (
                up["gross_cost_remaining_usd"] + up["fee_remaining_usd"]
            ) * up_ratio
        if down["shares_remaining"] > 0:
            down_ratio = pair_shares / down["shares_remaining"]
            pair_cost_usd += (
                down["gross_cost_remaining_usd"] + down["fee_remaining_usd"]
            ) * down_ratio
    residual_side: str | None = None
    residual_shares = 0.0
    if up["shares_remaining"] > down["shares_remaining"]:
        residual_side = "Up"
        residual_shares = up["shares_remaining"] - down["shares_remaining"]
    elif down["shares_remaining"] > up["shares_remaining"]:
        residual_side = "Down"
        residual_shares = down["shares_remaining"] - up["shares_remaining"]
    return {
        "market_id": market_id,
        "pair_shares": pair_shares,
        "pair_cost_usd": pair_cost_usd,
        "pair_payout_usd": payout_usd,
        "pair_pnl_usd": payout_usd - pair_cost_usd,
        "residual_side": residual_side,
        "residual_shares": residual_shares,
        "up": up,
        "down": down,
    }


def recent_actions(
    conn: sqlite3.Connection,
    *,
    market_id: str | None = None,
    action_type: str | None = None,
    limit: int = 50,
) -> list[dict[str, Any]]:
    """Most recent actions first. Thin wrapper over load_actions."""
    if limit <= 0:
        return []
    return load_actions(
        conn,
        market_id=market_id,
        action_type=action_type,
        limit=limit,
        newest_first=True,
    )


def unmatched_fills(
    conn: sqlite3.Connection,
    market_id: str,
    *,
    side: str | None = None,
) -> list[dict[str, Any]]:
    """Fills whose open lot still carries shares_remaining > 0.

    Reconciliation read. A fill that has been fully matched or redeemed
    should not appear here. If a fill row exists without a corresponding
    open-lot row (shouldn't happen, but is the canonical "data drift"
    failure), it is skipped so this helper never reports phantom work.
    """
    sql = """
        SELECT
            f.id,
            f.market_id,
            f.ts,
            f.side,
            f.reason,
            f.price,
            f.ask_size,
            f.shares,
            f.gross_cost_usd,
            f.fee_usd,
            f.token_id,
            f.order_id,
            f.client_order_id,
            ol.id AS open_lot_id,
            ol.shares_remaining,
            ol.gross_cost_remaining_usd,
            ol.fee_remaining_usd
        FROM whale_pair_fills f
        INNER JOIN whale_pair_open_lots ol
            ON ol.fill_id = f.id AND ol.market_id = f.market_id
        WHERE f.market_id = ? AND ol.shares_remaining > 0
    """
    params: list[Any] = [market_id]
    if side is not None:
        sql += " AND f.side = ?"
        params.append(side)
    sql += " ORDER BY f.id"
    rows = conn.execute(sql, tuple(params)).fetchall()
    return [_row_to_dict(row) for row in rows if row is not None]


def reconcile_market(
    conn: sqlite3.Connection,
    market_id: str,
) -> dict[str, Any]:
    """Compact reconciliation snapshot for one market.

    Rolls up the numbers an operator needs to decide "did the ledger and
    the chain agree after restart" without issuing many separate queries.
    """
    market = load_market(conn, market_id)
    fills = load_fills(conn, market_id)
    fill_shares_by_side = {"Up": 0.0, "Down": 0.0}
    gross_cost_by_side = {"Up": 0.0, "Down": 0.0}
    fee_by_side = {"Up": 0.0, "Down": 0.0}
    for row in fills:
        side = str(row["side"])
        if side in fill_shares_by_side:
            fill_shares_by_side[side] += float(row["shares"] or 0.0)
            gross_cost_by_side[side] += float(row["gross_cost_usd"] or 0.0)
            fee_by_side[side] += float(row["fee_usd"] or 0.0)
    matches = load_matches(conn, market_id)
    matched_shares = sum(float(row["shares"] or 0.0) for row in matches)
    realized_pnl = sum(float(row["realized_pnl_usd"] or 0.0) for row in matches)
    open_summary = summarize_open_lots(conn, market_id)
    merge_ready = merge_ready_inventory(conn, market_id)
    redeems = load_redeems(conn, market_id=market_id)
    redeemed_shares = sum(float(row["shares_redeemed"] or 0.0) for row in redeems)
    redeem_payout = sum(float(row["payout_usd"] or 0.0) for row in redeems)
    latest = latest_action(conn, market_id=market_id)
    return {
        "market_id": market_id,
        "market": market,
        "fill_count": len(fills),
        "fill_shares_by_side": fill_shares_by_side,
        "gross_cost_by_side": gross_cost_by_side,
        "fee_by_side": fee_by_side,
        "total_gross_cost_usd": sum(gross_cost_by_side.values()),
        "total_fee_usd": sum(fee_by_side.values()),
        "match_count": len(matches),
        "matched_shares": matched_shares,
        "realized_pnl_usd": realized_pnl,
        "open_summary": open_summary,
        "merge_ready": merge_ready,
        "redeem_count": len(redeems),
        "redeemed_shares": redeemed_shares,
        "redeem_payout_usd": redeem_payout,
        "latest_action": latest,
    }


def restore_market_state(
    conn: sqlite3.Connection,
    market_id: str,
) -> dict[str, Any]:
    """Startup-restore helper: rebuild everything the live bot needs.

    Returns the strategy-facing `WhalePairMarketState` plus ledger-side
    bookkeeping (latest_match_id, latest_action, fill counts, unresolved
    flag) so a restarting bot can resume without re-reading the schema.
    """
    state = build_market_state(conn, market_id)
    market = load_market(conn, market_id)
    latest_match = latest_match_id(conn, market_id)
    latest = latest_action(conn, market_id=market_id)
    merge_ready = merge_ready_inventory(conn, market_id)
    open_summary = summarize_open_lots(conn, market_id)
    return {
        "market_id": market_id,
        "market": market,
        "state": state,
        "gross_cost_usd": state.gross_cost_usd,
        "up_shares_remaining": open_summary["sides"]["Up"]["shares_remaining"],
        "down_shares_remaining": open_summary["sides"]["Down"]["shares_remaining"],
        "merge_ready_shares": merge_ready["pair_shares"],
        "residual_side": merge_ready["residual_side"],
        "residual_shares": merge_ready["residual_shares"],
        "latest_match_id": latest_match,
        "latest_action": latest,
        "is_resolved": bool(market["resolved"]) if market is not None else False,
    }


def restore_all_active(
    conn: sqlite3.Connection,
) -> list[dict[str, Any]]:
    """Startup-restore helper: every market that still needs attention.

    Covers both (a) unresolved markets and (b) resolved markets that
    still have open inventory (awaiting residual redeem). Callers can
    iterate this list at boot to rehydrate per-market state.
    """
    candidates = load_reconciliation_candidates(conn)
    return [restore_market_state(conn, row["market_id"]) for row in candidates]


def mark_market_resolved(
    conn: sqlite3.Connection,
    market_id: str,
    *,
    winning_outcome: str | None,
    merged_pnl_usd: float | None = None,
    residual_pnl_usd: float | None = None,
) -> None:
    """Flip resolved flag, stamp outcome, store pnl breakouts."""
    conn.execute(
        """
        UPDATE whale_pair_markets
        SET resolved = 1,
            winning_outcome = ?,
            merged_pnl_usd = ?,
            residual_pnl_usd = ?
        WHERE market_id = ?
        """,
        (winning_outcome, merged_pnl_usd, residual_pnl_usd, market_id),
    )
    conn.commit()


def record_redeem(
    conn: sqlite3.Connection,
    market_id: str,
    *,
    condition_id: str,
    winning_outcome: str,
    shares_redeemed: float,
    payout_usd: float,
    tx_hash: str | None = None,
    status: str | None = None,
    ts: str | None = None,
) -> int:
    """Record a redeem event. 1:1 payout of shares_redeemed USDC expected."""
    cur = conn.execute(
        """
        INSERT INTO whale_pair_redeems (
            market_id, condition_id, winning_outcome, shares_redeemed,
            payout_usd, tx_hash, status, ts
        ) VALUES (?, ?, ?, ?, ?, ?, ?, ?)
        """,
        (
            market_id,
            condition_id,
            winning_outcome,
            float(shares_redeemed),
            float(payout_usd),
            tx_hash,
            status,
            ts or datetime.now(timezone.utc).isoformat(),
        ),
    )
    conn.commit()
    return int(cur.lastrowid)


def load_redeems(
    conn: sqlite3.Connection,
    *,
    market_id: str | None = None,
) -> list[dict[str, Any]]:
    sql = """
        SELECT id, market_id, condition_id, winning_outcome, shares_redeemed,
               payout_usd, tx_hash, status, ts
        FROM whale_pair_redeems
    """
    params: list[Any] = []
    if market_id is not None:
        sql += " WHERE market_id = ?"
        params.append(market_id)
    sql += " ORDER BY id"
    rows = conn.execute(sql, tuple(params)).fetchall()
    return [_row_to_dict(row) for row in rows if row is not None]


def annotate_match_tx(
    conn: sqlite3.Connection,
    match_id: int,
    *,
    tx_hash: str | None,
    tx_status: str | None,
) -> None:
    """Stamp a merge tx hash/status onto a match row post-submission."""
    conn.execute(
        """
        UPDATE whale_pair_matches
        SET tx_hash = ?, tx_status = ?
        WHERE id = ?
        """,
        (tx_hash, tx_status, int(match_id)),
    )
    conn.commit()
