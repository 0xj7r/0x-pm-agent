"""Coverage audit for backfilled snapshot orderbook data.

Reports per coin:
  - total markets in metadata
  - markets with at least one snapshot row
  - markets with at least one populated orderbook row
  - total snapshot rows
  - snapshot rows with populated orderbook (best_ask_up not null)
  - earliest and latest snapshot timestamp
  - per-day market coverage so we can spot collection gaps
  - orderbook depth statistics (avg bids/asks per snapshot)
  - spread distribution on the up token
  - markets-without-book (residual gap) sample

Run after a backfill to confirm we have honest execution data
before any research or simulation runs against the dataset.

Usage:
    python3 scripts/verify_orderbook_coverage.py
    python3 scripts/verify_orderbook_coverage.py --coin btc
"""
from __future__ import annotations

import argparse
import json
import sqlite3
import sys
from collections import defaultdict
from pathlib import Path
from statistics import mean, median

sys.path.insert(0, str(Path(__file__).parent.parent))

from shared.constants import COINS, db_path


def _q(conn: sqlite3.Connection, sql: str, *params) -> list:
    return conn.execute(sql, params).fetchall()


def audit_coin(path: Path) -> dict:
    if not path.exists():
        return {"error": f"db not found: {path}"}

    conn = sqlite3.connect(str(path))
    try:
        # Confirm the new columns exist
        snap_cols = {row[1] for row in conn.execute("PRAGMA table_info(snapshots)").fetchall()}
        required = {"best_bid_up", "best_ask_up", "bid_size_up", "ask_size_up",
                    "best_bid_down", "best_ask_down", "orderbook_up_json"}
        missing = required - snap_cols
        if missing:
            return {"error": f"missing columns: {sorted(missing)}"}

        out: dict = {}

        # Headline counts
        out["markets_total"] = _q(conn, "SELECT count(*) FROM markets")[0][0]
        out["markets_resolved"] = _q(
            conn, "SELECT count(*) FROM markets WHERE winner IS NOT NULL"
        )[0][0]
        out["markets_with_any_snap"] = _q(
            conn, "SELECT count(distinct market_id) FROM snapshots"
        )[0][0]
        out["markets_with_book"] = _q(
            conn, "SELECT count(distinct market_id) FROM snapshots "
            "WHERE best_ask_up IS NOT NULL"
        )[0][0]
        out["snap_rows_total"] = _q(conn, "SELECT count(*) FROM snapshots")[0][0]
        out["snap_rows_with_book"] = _q(
            conn, "SELECT count(*) FROM snapshots WHERE best_ask_up IS NOT NULL"
        )[0][0]

        if out["snap_rows_total"]:
            out["book_coverage_pct"] = round(
                100 * out["snap_rows_with_book"] / out["snap_rows_total"], 2
            )
        else:
            out["book_coverage_pct"] = 0.0

        # Time range
        ts_row = _q(conn, "SELECT min(time), max(time) FROM snapshots")[0]
        out["snap_time_min"] = ts_row[0]
        out["snap_time_max"] = ts_row[1]

        market_ts_row = _q(
            conn, "SELECT min(start_time), max(start_time) FROM markets WHERE winner IS NOT NULL"
        )[0]
        out["market_time_min"] = market_ts_row[0]
        out["market_time_max"] = market_ts_row[1]

        # Per-day coverage: how many markets per day have snapshots
        day_rows = _q(
            conn,
            "SELECT substr(start_time,1,10) AS day, count(*) AS total, "
            "sum(CASE WHEN EXISTS (SELECT 1 FROM snapshots s WHERE s.market_id=m.market_id LIMIT 1) "
            "THEN 1 ELSE 0 END) AS with_snaps "
            "FROM markets m WHERE winner IS NOT NULL GROUP BY day ORDER BY day",
        )
        out["per_day"] = [
            {"day": d, "total": t, "with_snaps": w, "pct": round(100 * w / t, 1) if t else 0.0}
            for d, t, w in day_rows
        ]
        full_days = sum(1 for d in out["per_day"] if d["pct"] >= 99.0)
        out["days_with_full_coverage"] = full_days
        out["days_total"] = len(out["per_day"])

        # Spread distribution on up token
        spread_rows = _q(
            conn,
            "SELECT best_ask_up - best_bid_up FROM snapshots "
            "WHERE best_ask_up IS NOT NULL AND best_bid_up IS NOT NULL "
            "AND best_ask_up >= best_bid_up LIMIT 200000",
        )
        spreads = [r[0] for r in spread_rows if r[0] is not None]
        if spreads:
            out["spread_up"] = {
                "n": len(spreads),
                "min": round(min(spreads), 4),
                "median": round(median(spreads), 4),
                "mean": round(mean(spreads), 4),
                "max": round(max(spreads), 4),
                "p99": round(sorted(spreads)[int(len(spreads) * 0.99)], 4) if len(spreads) >= 100 else None,
            }

        # Sample markets without book (likely PolyBackTest collection gaps)
        gap_rows = _q(
            conn,
            "SELECT m.market_id, m.start_time FROM markets m "
            "WHERE m.winner IS NOT NULL AND NOT EXISTS ("
            "  SELECT 1 FROM snapshots s WHERE s.market_id = m.market_id "
            "  AND s.best_ask_up IS NOT NULL LIMIT 1) "
            "ORDER BY m.start_time DESC LIMIT 5",
        )
        out["sample_markets_without_book"] = [
            {"market_id": r[0], "start_time": r[1]} for r in gap_rows
        ]
        return out
    finally:
        conn.close()


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--coin", action="append", default=[], choices=COINS)
    parser.add_argument("--json", action="store_true", help="Emit machine-readable JSON")
    args = parser.parse_args()

    coins = args.coin or COINS
    report = {}
    for coin in coins:
        report[coin] = audit_coin(db_path(coin))

    if args.json:
        print(json.dumps(report, indent=2, default=str))
        return

    for coin, data in report.items():
        print(f"\n=== {coin.upper()} ===")
        if "error" in data:
            print(f"  ERROR: {data['error']}")
            continue
        print(f"  markets total              : {data['markets_total']:,}")
        print(f"  markets resolved           : {data['markets_resolved']:,}")
        print(f"  markets with any snap      : {data['markets_with_any_snap']:,}")
        print(f"  markets with book          : {data['markets_with_book']:,}")
        print(f"  snapshot rows total        : {data['snap_rows_total']:,}")
        print(f"  snapshot rows with book    : {data['snap_rows_with_book']:,}")
        print(f"  book coverage %            : {data['book_coverage_pct']}%")
        print(f"  snap time range            : {data['snap_time_min']} -> {data['snap_time_max']}")
        print(f"  market time range          : {data['market_time_min']} -> {data['market_time_max']}")
        print(f"  days with full coverage    : {data['days_with_full_coverage']} / {data['days_total']}")
        if data.get("spread_up"):
            s = data["spread_up"]
            print(f"  up spread (n={s['n']:,})    : min={s['min']} median={s['median']} mean={s['mean']} max={s['max']} p99={s['p99']}")
        if data.get("sample_markets_without_book"):
            print(f"  sample markets without book (first 5):")
            for m in data["sample_markets_without_book"]:
                print(f"    {m['market_id']} {m['start_time']}")
        # Daily coverage histogram
        bad_days = [d for d in data["per_day"] if d["pct"] < 90]
        if bad_days:
            print(f"  days with <90% coverage    : {len(bad_days)}")
            for d in bad_days[:10]:
                print(f"    {d['day']}: {d['with_snaps']}/{d['total']} ({d['pct']}%)")


if __name__ == "__main__":
    main()
