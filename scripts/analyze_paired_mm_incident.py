#!/usr/bin/env python3
"""Summarise paired-MM live execution by market from the runtime order DB.

This is an attribution tool, not a trading control path. It reads the durable
SQLite order store written by `polymarket-exec` and emits JSON suitable for
incident reviews and replay/backtest comparisons.
"""

from __future__ import annotations

import argparse
import json
import sqlite3
from collections import defaultdict
from dataclasses import asdict, dataclass, field
from pathlib import Path
from typing import Any


@dataclass
class SideSummary:
    submitted_orders: int = 0
    filled_orders: int = 0
    original_qty: float = 0.0
    filled_qty: float = 0.0
    filled_notional_usd: float = 0.0
    avg_fill_price: float | None = None


@dataclass
class MarketSummary:
    market_id: str
    submitted_orders: int = 0
    filled_orders: int = 0
    cancelled_orders: int = 0
    rejected_orders: int = 0
    open_orders: int = 0
    buy_filled_qty: float = 0.0
    buy_filled_notional_usd: float = 0.0
    sell_filled_qty: float = 0.0
    sell_filled_notional_usd: float = 0.0
    paired_qty_estimate: float = 0.0
    stranded_qty_estimate: float = 0.0
    stranded_notional_estimate_usd: float = 0.0
    same_leg_accumulation_warning: bool = False
    sell_warning: bool = False
    by_instrument: dict[str, SideSummary] = field(default_factory=dict)
    quote_tags: dict[str, int] = field(default_factory=dict)


TERMINAL_CANCELLED = {"Cancelled", "Quarantined"}
TERMINAL_REJECTED = {"Rejected"}
OPEN_STATUSES = {
    "PendingSubmit",
    "Submitted",
    "Working",
    "CancelRequested",
    "NeedsReconcile",
}


def side_summary(summary: MarketSummary, instrument_id: str) -> SideSummary:
    if instrument_id not in summary.by_instrument:
        summary.by_instrument[instrument_id] = SideSummary()
    return summary.by_instrument[instrument_id]


def load_rows(db_path: Path, since_ms: int | None, until_ms: int | None) -> list[sqlite3.Row]:
    conn = sqlite3.connect(str(db_path))
    conn.row_factory = sqlite3.Row
    clauses = []
    params: list[Any] = []
    if since_ms is not None:
        clauses.append("last_update_ms >= ?")
        params.append(since_ms)
    if until_ms is not None:
        clauses.append("last_update_ms <= ?")
        params.append(until_ms)
    where = f"WHERE {' AND '.join(clauses)}" if clauses else ""
    rows = conn.execute(
        f"""
        SELECT
            client_order_id,
            market_id,
            instrument_id,
            side,
            limit_price,
            original_qty,
            remaining_qty,
            filled_qty,
            status,
            submitted_at_ms,
            last_update_ms,
            strategy_tag,
            quote_level_tag
        FROM orders
        {where}
        ORDER BY market_id, last_update_ms, client_order_id
        """,
        params,
    ).fetchall()
    conn.close()
    return rows


def summarise(rows: list[sqlite3.Row]) -> dict[str, MarketSummary]:
    markets: dict[str, MarketSummary] = {}
    for row in rows:
        market_id = row["market_id"]
        summary = markets.setdefault(market_id, MarketSummary(market_id=market_id))
        instrument = side_summary(summary, row["instrument_id"])
        tag = row["quote_level_tag"] or "untagged"
        filled_qty = float(row["filled_qty"] or 0.0)
        original_qty = float(row["original_qty"] or 0.0)
        limit_price = float(row["limit_price"] or 0.0)
        status = row["status"]
        side = row["side"]

        summary.submitted_orders += 1
        instrument.submitted_orders += 1
        instrument.original_qty += original_qty
        summary.quote_tags[tag] = summary.quote_tags.get(tag, 0) + 1

        if status in OPEN_STATUSES:
            summary.open_orders += 1
        if status in TERMINAL_CANCELLED:
            summary.cancelled_orders += 1
        if status in TERMINAL_REJECTED:
            summary.rejected_orders += 1
        if side == "Sell":
            summary.sell_warning = True

        if filled_qty > 0:
            summary.filled_orders += 1
            instrument.filled_orders += 1
            instrument.filled_qty += filled_qty
            instrument.filled_notional_usd += filled_qty * limit_price
            if side == "Buy":
                summary.buy_filled_qty += filled_qty
                summary.buy_filled_notional_usd += filled_qty * limit_price
            elif side == "Sell":
                summary.sell_filled_qty += filled_qty
                summary.sell_filled_notional_usd += filled_qty * limit_price

    for summary in markets.values():
        filled_instruments = [
            instrument
            for instrument in summary.by_instrument.values()
            if instrument.filled_qty > 0
        ]
        for instrument in filled_instruments:
            instrument.avg_fill_price = (
                instrument.filled_notional_usd / instrument.filled_qty
                if instrument.filled_qty > 0
                else None
            )

        if len(filled_instruments) >= 2:
            quantities = sorted(
                [instrument.filled_qty for instrument in filled_instruments],
                reverse=True,
            )
            summary.paired_qty_estimate = quantities[1]
            summary.stranded_qty_estimate = max(0.0, quantities[0] - quantities[1])
            heavy = max(filled_instruments, key=lambda item: item.filled_qty)
            summary.stranded_notional_estimate_usd = (
                summary.stranded_qty_estimate * (heavy.avg_fill_price or 0.0)
            )
        elif len(filled_instruments) == 1:
            only = filled_instruments[0]
            summary.stranded_qty_estimate = only.filled_qty
            summary.stranded_notional_estimate_usd = only.filled_notional_usd

        summary.same_leg_accumulation_warning = (
            summary.stranded_qty_estimate > max(5.0, summary.paired_qty_estimate)
        )
    return markets


def to_json(markets: dict[str, MarketSummary]) -> dict[str, Any]:
    rows = []
    totals = {
        "submitted_orders": 0,
        "filled_orders": 0,
        "buy_filled_notional_usd": 0.0,
        "sell_filled_notional_usd": 0.0,
        "paired_qty_estimate": 0.0,
        "stranded_qty_estimate": 0.0,
        "stranded_notional_estimate_usd": 0.0,
        "markets_with_same_leg_warning": 0,
        "markets_with_sell_warning": 0,
    }
    for summary in markets.values():
        row = asdict(summary)
        rows.append(row)
        for key in [
            "submitted_orders",
            "filled_orders",
            "buy_filled_notional_usd",
            "sell_filled_notional_usd",
            "paired_qty_estimate",
            "stranded_qty_estimate",
            "stranded_notional_estimate_usd",
        ]:
            totals[key] += row[key]
        totals["markets_with_same_leg_warning"] += int(
            summary.same_leg_accumulation_warning
        )
        totals["markets_with_sell_warning"] += int(summary.sell_warning)

    rows.sort(
        key=lambda item: (
            item["sell_warning"],
            item["same_leg_accumulation_warning"],
            item["stranded_notional_estimate_usd"],
        ),
        reverse=True,
    )
    return {"totals": totals, "markets": rows}


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--db", required=True, type=Path, help="Runtime orders SQLite DB")
    parser.add_argument("--since-ms", type=int)
    parser.add_argument("--until-ms", type=int)
    parser.add_argument("--pretty", action="store_true")
    args = parser.parse_args()

    rows = load_rows(args.db, args.since_ms, args.until_ms)
    payload = to_json(summarise(rows))
    print(json.dumps(payload, indent=2 if args.pretty else None, sort_keys=True))


if __name__ == "__main__":
    main()
