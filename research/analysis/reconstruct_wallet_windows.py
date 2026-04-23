#!/usr/bin/env python3
"""Reconstruct per-market BTC 5m window behavior for a tracked wallet.

This turns trade rows plus closed positions into the correct unit of analysis:
one row per market window, with both sides combined.
"""
from __future__ import annotations

import argparse
import json
import sqlite3
from collections import defaultdict
from datetime import UTC, datetime
from pathlib import Path
from statistics import median
from typing import Any

from research.wallet_aliases import wallet_dir_name

ROOT = Path(__file__).resolve().parent.parent.parent
RESEARCH_ROOT = ROOT / "data" / "research" / "wallet_research"
DEFAULT_WALLET = "0xb27bc932bf8110d8f78e55da7d5f0497a18b5b82"


def iso_to_ts(value: str | None) -> int | None:
    if not value:
        return None
    try:
        return int(datetime.fromisoformat(value.replace("Z", "+00:00")).timestamp())
    except ValueError:
        return None


def safe_float(value: Any) -> float:
    try:
        return float(value or 0.0)
    except (TypeError, ValueError):
        return 0.0


def ensure_market_catalog_schema(conn: sqlite3.Connection) -> None:
    existing = {
        str(row[1])
        for row in conn.execute("PRAGMA table_info(wallet_market_catalog)").fetchall()
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
            conn.execute(f"ALTER TABLE wallet_market_catalog ADD COLUMN {name} {col_type}")
    conn.commit()


def weighted_avg_price(rows: list[dict[str, Any]]) -> float | None:
    total_cost = sum(safe_float(row.get("usdc_size")) for row in rows)
    total_shares = sum(safe_float(row.get("size")) for row in rows)
    if total_cost <= 0 or total_shares <= 0:
        return None
    return total_cost / total_shares


def market_start_ts(slug: str) -> int | None:
    try:
        return int(slug.rsplit("-", 1)[-1])
    except ValueError:
        return None


def summarize_market(slug: str, activity_rows: list[dict[str, Any]], closed_rows: list[dict[str, Any]], market_id: str | None) -> dict[str, Any]:
    trade_rows = [row for row in activity_rows if row.get("activity_type") == "TRADE"]
    buy_rows = [row for row in trade_rows if row.get("side") == "BUY"]
    sell_rows = [row for row in trade_rows if row.get("side") == "SELL"]
    merge_rows = [row for row in activity_rows if row.get("activity_type") == "MERGE"]
    up_rows = [row for row in buy_rows if row.get("outcome") == "Up"]
    down_rows = [row for row in buy_rows if row.get("outcome") == "Down"]

    up_cost = sum(safe_float(row.get("usdc_size")) for row in up_rows)
    down_cost = sum(safe_float(row.get("usdc_size")) for row in down_rows)
    up_shares = sum(safe_float(row.get("size")) for row in up_rows)
    down_shares = sum(safe_float(row.get("size")) for row in down_rows)
    up_realized = sum(safe_float(row.get("realized_pnl")) for row in closed_rows if row.get("outcome") == "Up")
    down_realized = sum(safe_float(row.get("realized_pnl")) for row in closed_rows if row.get("outcome") == "Down")
    combined_realized = up_realized + down_realized
    outcomes = {str(row.get("outcome") or "") for row in buy_rows}

    price_map = {
        "Up": weighted_avg_price(up_rows),
        "Down": weighted_avg_price(down_rows),
    }
    present_prices = {side: price for side, price in price_map.items() if price is not None}
    cheap_leg = min(present_prices, key=present_prices.get) if present_prices else None
    expensive_leg = max(present_prices, key=present_prices.get) if present_prices else None
    cheap_cost = min(value for value in (up_cost, down_cost) if value > 0) if any(value > 0 for value in (up_cost, down_cost)) else None
    expensive_cost = max(up_cost, down_cost) if max(up_cost, down_cost) > 0 else None
    timestamps = sorted(int(row.get("event_ts") or 0) for row in activity_rows if row.get("event_ts"))
    start_ts = market_start_ts(slug)
    summary = {
        "window_key": slug,
        "slug": slug,
        "market_id": market_id,
        "start_ts": start_ts,
        "end_ts": start_ts + 300 if start_ts is not None else None,
        "first_trade_ts": timestamps[0] if timestamps else None,
        "last_trade_ts": timestamps[-1] if timestamps else None,
        "buy_rows": len(buy_rows),
        "sell_rows": len(sell_rows),
        "merge_rows": len(merge_rows),
        "up_buy_cost": up_cost,
        "down_buy_cost": down_cost,
        "up_buy_shares": up_shares,
        "down_buy_shares": down_shares,
        "up_avg_buy_price": price_map.get("Up"),
        "down_avg_buy_price": price_map.get("Down"),
        "up_realized_pnl": up_realized,
        "down_realized_pnl": down_realized,
        "combined_realized_pnl": combined_realized,
        "paired_outcomes": {"Up", "Down"}.issubset(outcomes),
        "cheap_leg": cheap_leg,
        "cheap_leg_avg_price": price_map.get(cheap_leg) if cheap_leg else None,
        "expensive_leg": expensive_leg,
        "expensive_leg_avg_price": price_map.get(expensive_leg) if expensive_leg else None,
        "hedge_cost_ratio": (cheap_cost / expensive_cost) if cheap_cost is not None and expensive_cost else None,
        "summary_json": {
            "buy_rows_by_outcome": {
                "Up": len(up_rows),
                "Down": len(down_rows),
            },
            "closed_rows": len(closed_rows),
            "closed_realized_by_outcome": {
                "Up": up_realized,
                "Down": down_realized,
            },
        },
    }
    return summary


def persist_windows(conn: sqlite3.Connection, wallet: str, rows: list[dict[str, Any]]) -> None:
    for row in rows:
        conn.execute(
            """
            INSERT OR REPLACE INTO wallet_window_reconstructions(
                window_key, wallet_address, slug, market_id, start_ts, end_ts, first_trade_ts, last_trade_ts,
                buy_rows, sell_rows, merge_rows, up_buy_cost, down_buy_cost, up_buy_shares, down_buy_shares,
                up_avg_buy_price, down_avg_buy_price, up_realized_pnl, down_realized_pnl, combined_realized_pnl,
                paired_outcomes, cheap_leg, cheap_leg_avg_price, expensive_leg, expensive_leg_avg_price,
                hedge_cost_ratio, summary_json
            ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
            """,
            (
                row["window_key"],
                wallet.lower(),
                row["slug"],
                row["market_id"],
                row["start_ts"],
                row["end_ts"],
                row["first_trade_ts"],
                row["last_trade_ts"],
                row["buy_rows"],
                row["sell_rows"],
                row["merge_rows"],
                row["up_buy_cost"],
                row["down_buy_cost"],
                row["up_buy_shares"],
                row["down_buy_shares"],
                row["up_avg_buy_price"],
                row["down_avg_buy_price"],
                row["up_realized_pnl"],
                row["down_realized_pnl"],
                row["combined_realized_pnl"],
                1 if row["paired_outcomes"] else 0,
                row["cheap_leg"],
                row["cheap_leg_avg_price"],
                row["expensive_leg"],
                row["expensive_leg_avg_price"],
                row["hedge_cost_ratio"],
                json.dumps(row["summary_json"], separators=(",", ":")),
            ),
        )
    conn.commit()


def main() -> None:
    ap = argparse.ArgumentParser()
    ap.add_argument("--wallet", default=DEFAULT_WALLET)
    ap.add_argument("--family-prefix", default="btc-updown-5m-")
    args = ap.parse_args()

    wallet = args.wallet.lower()
    alias = wallet_dir_name(wallet)
    base_dir = RESEARCH_ROOT / alias
    db_path = base_dir / "wallet_research.db"
    conn = sqlite3.connect(str(db_path))
    conn.row_factory = sqlite3.Row
    ensure_market_catalog_schema(conn)

    activity_rows = [
        dict(row)
        for row in conn.execute(
            """
            SELECT a.*, m.market_id, m.price_to_beat, m.final_price, m.event_start_time, m.closed_time
            FROM wallet_activity_raw a
            LEFT JOIN wallet_market_catalog m ON m.slug = a.slug
            WHERE a.wallet_address = ? AND a.slug LIKE ?
            ORDER BY a.event_ts
            """,
            (wallet, f"{args.family_prefix}%"),
        ).fetchall()
    ]
    closed_rows = [
        dict(row)
        for row in conn.execute(
            """
            SELECT *
            FROM wallet_closed_positions_raw
            WHERE wallet_address = ? AND slug LIKE ?
            """,
            (wallet, f"{args.family_prefix}%"),
        ).fetchall()
    ]

    by_slug_activity: dict[str, list[dict[str, Any]]] = defaultdict(list)
    for row in activity_rows:
        by_slug_activity[str(row.get("slug") or "")].append(row)
    by_slug_closed: dict[str, list[dict[str, Any]]] = defaultdict(list)
    for row in closed_rows:
        by_slug_closed[str(row.get("slug") or "")].append(row)

    windows = [
        summarize_market(
            slug,
            slug_rows,
            by_slug_closed.get(slug, []),
            slug_rows[0].get("market_id"),
        )
        for slug, slug_rows in by_slug_activity.items()
    ]
    for row in windows:
        slug_rows = by_slug_activity.get(row["slug"], [])
        market_row = slug_rows[0] if slug_rows else {}
        row["summary_json"].update(
            {
                "price_to_beat": market_row.get("price_to_beat"),
                "final_price": market_row.get("final_price"),
                "event_start_time": market_row.get("event_start_time"),
                "closed_time": market_row.get("closed_time"),
            }
        )
    windows.sort(key=lambda row: row.get("start_ts") or 0)
    persist_windows(conn, wallet, windows)

    combined = [row["combined_realized_pnl"] for row in windows]
    realized_windows = [row for row in windows if row["summary_json"].get("closed_rows", 0) > 0]
    realized_combined = [row["combined_realized_pnl"] for row in realized_windows]
    hedge_ratios = [row["hedge_cost_ratio"] for row in windows if row["hedge_cost_ratio"] is not None]
    summary = {
        "wallet": wallet,
        "wallet_alias": alias,
        "family_prefix": args.family_prefix,
        "windows": len(windows),
        "paired_windows": sum(1 for row in windows if row["paired_outcomes"]),
        "realized_windows": len(realized_windows),
        "positive_combined_windows": sum(1 for row in realized_windows if row["combined_realized_pnl"] > 0),
        "combined_realized_pnl_total": sum(realized_combined),
        "combined_realized_pnl_median": median(realized_combined) if realized_combined else None,
        "hedge_cost_ratio_median": median(hedge_ratios) if hedge_ratios else None,
        "top_positive_windows": sorted(realized_windows, key=lambda row: row["combined_realized_pnl"], reverse=True)[:10],
        "top_negative_windows": sorted(realized_windows, key=lambda row: row["combined_realized_pnl"])[:10],
    }
    output_path = base_dir / "btc_5m_window_reconstruction.json"
    output_path.write_text(json.dumps(summary, indent=2))
    conn.close()


if __name__ == "__main__":
    main()
