"""Persistent storage: SQLite for trades + MEMORY.md sync for cross-session learning."""

from __future__ import annotations

import json
import logging
import sqlite3
from datetime import datetime
from pathlib import Path

from models.trade import Signal, SignalSource, Trade, TradeResult

logger = logging.getLogger(__name__)

SCHEMA = """
CREATE TABLE IF NOT EXISTS trades (
    id TEXT PRIMARY KEY,
    market_id TEXT NOT NULL,
    market_question TEXT,
    outcome TEXT,
    side TEXT,
    source TEXT,
    fair_value REAL,
    market_price REAL,
    edge REAL,
    confidence REAL,
    reasoning TEXT,
    size_usd REAL,
    price REAL,
    token_id TEXT,
    paper INTEGER DEFAULT 0,
    order_id TEXT,
    api_cost_usd REAL DEFAULT 0,
    created_at TEXT DEFAULT CURRENT_TIMESTAMP
);

CREATE TABLE IF NOT EXISTS results (
    trade_id TEXT PRIMARY KEY,
    market_id TEXT NOT NULL,
    resolved INTEGER DEFAULT 0,
    won INTEGER DEFAULT 0,
    pnl_usd REAL DEFAULT 0,
    resolved_at TEXT,
    FOREIGN KEY (trade_id) REFERENCES trades(id)
);

CREATE TABLE IF NOT EXISTS portfolio_snapshots (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    balance_usd REAL,
    total_invested REAL,
    unrealized_pnl REAL,
    realized_pnl REAL,
    total_api_cost REAL,
    num_open_positions INTEGER,
    num_trades INTEGER,
    win_rate REAL,
    created_at TEXT DEFAULT CURRENT_TIMESTAMP
);

CREATE TABLE IF NOT EXISTS learnings (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    category TEXT,
    lesson TEXT,
    source TEXT,
    trade_ids TEXT,
    created_at TEXT DEFAULT CURRENT_TIMESTAMP
);

CREATE TABLE IF NOT EXISTS event_log (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    window_id TEXT NOT NULL,
    timestamp TEXT DEFAULT CURRENT_TIMESTAMP,
    event_type TEXT NOT NULL,
    log_odds REAL,
    p_up REAL,
    btc_price REAL,
    details TEXT
);
"""


class MemoryStore:
    def __init__(self, db_path: str = "trades.db"):
        self.db_path = db_path
        self.conn = sqlite3.connect(db_path)
        self.conn.row_factory = sqlite3.Row
        self._init_schema()

    def _init_schema(self):
        self.conn.executescript(SCHEMA)
        self.conn.commit()

    def save_trade(self, trade: Trade):
        self.conn.execute(
            """INSERT OR REPLACE INTO trades
            (id, market_id, market_question, outcome, side, source,
             fair_value, market_price, edge, confidence, reasoning,
             size_usd, price, token_id, paper, order_id, api_cost_usd)
            VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)""",
            (
                trade.id,
                trade.signal.market_id,
                trade.signal.market_question,
                trade.signal.outcome.value,
                trade.signal.side.value,
                trade.signal.source.value,
                trade.signal.fair_value,
                trade.signal.market_price,
                trade.signal.edge,
                trade.signal.confidence,
                trade.signal.reasoning,
                trade.size_usd,
                trade.price,
                trade.token_id,
                1 if trade.paper else 0,
                trade.order_id,
                trade.api_cost_usd,
            ),
        )
        self.conn.commit()

    def save_result(self, result: TradeResult):
        self.conn.execute(
            """INSERT OR REPLACE INTO results
            (trade_id, market_id, resolved, won, pnl_usd, resolved_at)
            VALUES (?, ?, ?, ?, ?, ?)""",
            (
                result.trade_id,
                result.market_id,
                1 if result.resolved else 0,
                1 if result.won else 0,
                result.pnl_usd,
                result.resolved_at.isoformat() if result.resolved_at else None,
            ),
        )
        self.conn.commit()

    def save_snapshot(self, snapshot):
        self.conn.execute(
            """INSERT INTO portfolio_snapshots
            (balance_usd, total_invested, unrealized_pnl, realized_pnl,
             total_api_cost, num_open_positions, num_trades, win_rate)
            VALUES (?, ?, ?, ?, ?, ?, ?, ?)""",
            (
                snapshot.balance_usd,
                snapshot.total_invested,
                snapshot.unrealized_pnl,
                snapshot.realized_pnl,
                snapshot.total_api_cost,
                snapshot.num_open_positions,
                snapshot.num_trades,
                snapshot.win_rate,
            ),
        )
        self.conn.commit()

    def get_open_trades(self) -> list[dict]:
        """Return all paper trades that have not been resolved yet."""
        rows = self.conn.execute(
            """SELECT t.*
            FROM trades t
            LEFT JOIN results r ON t.id = r.trade_id
            WHERE t.paper = 1
              AND (r.trade_id IS NULL OR r.resolved = 0)""",
        ).fetchall()
        return [dict(r) for r in rows]

    def mark_trade_resolved(self, trade_id: str, market_id: str, won: bool, pnl: float):
        """Mark a paper trade as resolved with its P&L."""
        result = TradeResult(
            trade_id=trade_id,
            market_id=market_id,
            resolved=True,
            won=won,
            pnl_usd=pnl,
            resolved_at=datetime.utcnow(),
        )
        self.save_result(result)
        return result

    def get_strategy_stats(self, source: str) -> dict:
        """Get win rate and average PnL for a strategy."""
        rows = self.conn.execute(
            """SELECT r.won, r.pnl_usd, t.edge, t.confidence
            FROM results r
            JOIN trades t ON r.trade_id = t.id
            WHERE t.source = ? AND r.resolved = 1""",
            (source,),
        ).fetchall()

        if not rows:
            return {"trades": 0, "win_rate": 0, "avg_pnl": 0, "total_pnl": 0}

        wins = sum(1 for r in rows if r["won"])
        total_pnl = sum(r["pnl_usd"] for r in rows)

        return {
            "trades": len(rows),
            "win_rate": wins / len(rows),
            "avg_pnl": total_pnl / len(rows),
            "total_pnl": total_pnl,
        }

    def get_recent_learnings(self, limit: int = 10) -> list[dict]:
        rows = self.conn.execute(
            "SELECT * FROM learnings ORDER BY created_at DESC LIMIT ?",
            (limit,),
        ).fetchall()
        return [dict(r) for r in rows]

    def save_learning(self, category: str, lesson: str, source: str, trade_ids: list[str]):
        self.conn.execute(
            "INSERT INTO learnings (category, lesson, source, trade_ids) VALUES (?, ?, ?, ?)",
            (category, lesson, source, json.dumps(trade_ids)),
        )
        self.conn.commit()

    def sync_to_memory_md(self, memory_path: str):
        """Sync key learnings and stats to MEMORY.md for cross-session Claude learning.

        Appends a trading section to the memory file with:
        - Overall stats
        - Per-strategy performance
        - Key learnings from resolved trades
        """
        stats = {}
        for source in SignalSource:
            s = self.get_strategy_stats(source.value)
            if s["trades"] > 0:
                stats[source.value] = s

        if not stats:
            return

        learnings = self.get_recent_learnings(5)

        section = "\n## Polymarket Trading Agent\n"
        section += f"- **Last sync**: {datetime.utcnow().isoformat()}\n"

        for strategy, s in stats.items():
            section += (
                f"- **{strategy}**: {s['trades']} trades, "
                f"{s['win_rate']:.0%} win rate, "
                f"${s['total_pnl']:+.2f} total PnL\n"
            )

        if learnings:
            section += "- **Key learnings**:\n"
            for l in learnings:
                section += f"  - [{l['category']}] {l['lesson']}\n"

        path = Path(memory_path)
        if path.exists():
            content = path.read_text()
            # Replace existing trading section or append
            marker = "## Polymarket Trading Agent"
            if marker in content:
                before = content[: content.index(marker)]
                # Find the next ## section after our marker
                rest = content[content.index(marker) :]
                next_section = rest.find("\n## ", 1)
                after = rest[next_section:] if next_section >= 0 else ""
                content = before + section + after
            else:
                content += "\n" + section
        else:
            content = f"# Project Memory\n{section}"

        path.write_text(content)
        logger.info(f"Synced trading stats to {memory_path}")

    def save_event(
        self,
        window_id: str,
        event_type: str,
        log_odds: float | None = None,
        p_up: float | None = None,
        btc_price: float | None = None,
        details: dict | None = None,
    ) -> None:
        self.conn.execute(
            """INSERT INTO event_log (window_id, event_type, log_odds, p_up, btc_price, details)
            VALUES (?, ?, ?, ?, ?, ?)""",
            (
                window_id,
                event_type,
                log_odds,
                p_up,
                btc_price,
                json.dumps(details) if details else None,
            ),
        )
        self.conn.commit()

    def get_events_for_window(self, window_id: str) -> list[dict]:
        rows = self.conn.execute(
            "SELECT * FROM event_log WHERE window_id = ? ORDER BY id",
            (window_id,),
        ).fetchall()
        return [dict(r) for r in rows]

    def close(self) -> None:
        self.conn.close()
