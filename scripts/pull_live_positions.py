#!/usr/bin/env python3
"""Pull live bot order/fill state from AWS to local append-only files.

This records bot-owned state from the runtime order-store SQLite database. It
does not call Polymarket and it does not mutate live execution state.
"""

from __future__ import annotations

import argparse
import csv
import json
import os
import sqlite3
import subprocess
import sys
import tempfile
import time
from pathlib import Path
from typing import Any


DEFAULT_REMOTE_DB = (
    "/home/ubuntu/go/polymarket-agent/"
    "data/runtime/btc-5m-bonereaper-tinylive/order-store.sqlite"
)
DEFAULT_OUT_DIR = "data/local/live_positions"
DEFAULT_HOST = "34.242.101.97"
DEFAULT_USER = "ubuntu"
DEFAULT_KEY = "~/.ssh/whale_pair_dublin_ed25519.pem"

ORDER_COLUMNS = [
    "client_order_id",
    "run_id",
    "venue_order_id",
    "market_id",
    "instrument_id",
    "side",
    "limit_price",
    "reduce_only",
    "original_qty",
    "remaining_qty",
    "filled_qty",
    "status",
    "submitted_at_ms",
    "last_update_ms",
    "reason",
    "strategy_tag",
    "quote_level_tag",
    "accounting_lane",
]

POSITION_EVENT_COLUMNS = [
    "observed_at_ms",
    "event_id",
    "client_order_id",
    "run_id",
    "venue_order_id",
    "market_id",
    "instrument_id",
    "side",
    "limit_price",
    "fill_delta_qty",
    "fill_delta_notional_usd",
    "cumulative_filled_qty",
    "status",
    "submitted_at_ms",
    "last_update_ms",
    "reason",
    "strategy_tag",
    "quote_level_tag",
    "accounting_lane",
]


def now_ms() -> int:
    return int(time.time() * 1000)


def run_checked(cmd: list[str]) -> None:
    subprocess.run(cmd, check=True)


def pull_remote_db(args: argparse.Namespace, local_db: Path) -> None:
    key = os.path.expanduser(args.key)
    remote = f"{args.user}@{args.host}:{args.remote_db}"
    cmd = [
        "scp",
        "-q",
        "-i",
        key,
        "-P",
        str(args.port),
        "-o",
        "StrictHostKeyChecking=accept-new",
        remote,
        str(local_db),
    ]
    run_checked(cmd)


def load_rows(db_path: Path) -> list[dict[str, Any]]:
    con = sqlite3.connect(f"file:{db_path}?mode=ro", uri=True)
    con.row_factory = sqlite3.Row
    try:
        rows = con.execute(
            """
            SELECT
                client_order_id,
                run_id,
                venue_order_id,
                market_id,
                instrument_id,
                side,
                limit_price,
                reduce_only,
                original_qty,
                remaining_qty,
                filled_qty,
                status,
                submitted_at_ms,
                last_update_ms,
                reason,
                strategy_tag,
                quote_level_tag,
                accounting_lane
            FROM orders
            ORDER BY submitted_at_ms ASC, client_order_id ASC
            """
        ).fetchall()
        return [dict(row) for row in rows]
    finally:
        con.close()


def event_id(row: dict[str, Any]) -> str:
    return "|".join(
        [
            str(row["client_order_id"]),
            str(row["last_update_ms"]),
            str(row["status"]),
            f"{float(row['filled_qty']):.12f}",
        ]
    )


def load_seen(path: Path) -> dict[str, Any]:
    if not path.exists():
        return {"seen_event_ids": set(), "last_filled_qty_by_order": {}}
    try:
        data = json.loads(path.read_text())
    except json.JSONDecodeError:
        return {"seen_event_ids": set(), "last_filled_qty_by_order": {}}
    return {
        "seen_event_ids": set(data.get("seen_event_ids", [])),
        "last_filled_qty_by_order": {
            str(key): float(value)
            for key, value in data.get("last_filled_qty_by_order", {}).items()
        },
    }


def save_seen(path: Path, seen: set[str], last_filled_qty_by_order: dict[str, float]) -> None:
    # Keep the state bounded. The append-only JSONL/CSV files remain the audit
    # source; this file is only a de-dupe cache for polling.
    ordered = sorted(seen)
    if len(ordered) > 200_000:
        ordered = ordered[-200_000:]
    path.write_text(
        json.dumps(
            {
                "seen_event_ids": ordered,
                "last_filled_qty_by_order": last_filled_qty_by_order,
            },
            indent=2,
            sort_keys=True,
        )
    )


def append_jsonl(path: Path, records: list[dict[str, Any]]) -> None:
    if not records:
        return
    with path.open("a", encoding="utf-8") as handle:
        for record in records:
            handle.write(json.dumps(record, sort_keys=True, separators=(",", ":")) + "\n")


def append_csv(path: Path, records: list[dict[str, Any]]) -> None:
    if not records:
        return
    write_header = not path.exists()
    with path.open("a", newline="", encoding="utf-8") as handle:
        writer = csv.DictWriter(handle, fieldnames=ORDER_COLUMNS + ["observed_at_ms"])
        if write_header:
            writer.writeheader()
        for record in records:
            writer.writerow({key: record.get(key) for key in writer.fieldnames})


def append_position_events_csv(path: Path, records: list[dict[str, Any]]) -> None:
    if not records:
        return
    write_header = not path.exists()
    with path.open("a", newline="", encoding="utf-8") as handle:
        writer = csv.DictWriter(handle, fieldnames=POSITION_EVENT_COLUMNS)
        if write_header:
            writer.writeheader()
        for record in records:
            writer.writerow({key: record.get(key) for key in writer.fieldnames})


def write_latest_positions_csv(path: Path, snapshot: dict[str, Any]) -> None:
    positions = snapshot.get("positions", [])
    fieldnames = [
        "observed_at_ms",
        "market_id",
        "instrument_id",
        "accounting_lane",
        "filled_qty",
        "filled_notional_usd",
        "avg_price",
        "order_count",
    ]
    with path.open("w", newline="", encoding="utf-8") as handle:
        writer = csv.DictWriter(handle, fieldnames=fieldnames)
        writer.writeheader()
        for position in positions:
            row = dict(position)
            row["observed_at_ms"] = snapshot.get("observed_at_ms")
            writer.writerow({key: row.get(key) for key in fieldnames})


def build_position_event(
    row: dict[str, Any],
    observed_at_ms: int,
    fill_delta_qty: float,
) -> dict[str, Any]:
    limit_price = float(row["limit_price"] or 0.0)
    event = {key: row.get(key) for key in POSITION_EVENT_COLUMNS}
    event["observed_at_ms"] = observed_at_ms
    event["event_id"] = event_id(row)
    event["fill_delta_qty"] = fill_delta_qty
    event["fill_delta_notional_usd"] = fill_delta_qty * limit_price
    event["cumulative_filled_qty"] = float(row["filled_qty"] or 0.0)
    return event


def build_position_snapshot(rows: list[dict[str, Any]], observed_at_ms: int) -> dict[str, Any]:
    positions: dict[tuple[str, str, str], dict[str, Any]] = {}
    for row in rows:
        filled_qty = float(row["filled_qty"] or 0.0)
        if filled_qty <= 0.0:
            continue
        key = (
            str(row["market_id"]),
            str(row["instrument_id"]),
            str(row["accounting_lane"]),
        )
        current = positions.setdefault(
            key,
            {
                "market_id": row["market_id"],
                "instrument_id": row["instrument_id"],
                "accounting_lane": row["accounting_lane"],
                "filled_qty": 0.0,
                "filled_notional_usd": 0.0,
                "order_count": 0,
            },
        )
        current["filled_qty"] += filled_qty
        current["filled_notional_usd"] += filled_qty * float(row["limit_price"] or 0.0)
        current["order_count"] += 1

    normalized = []
    for item in positions.values():
        qty = item["filled_qty"]
        notional = item["filled_notional_usd"]
        item["avg_price"] = notional / qty if qty > 0 else None
        normalized.append(item)

    normalized.sort(
        key=lambda item: (
            str(item["market_id"]),
            str(item["accounting_lane"]),
            str(item["instrument_id"]),
        )
    )
    return {
        "observed_at_ms": observed_at_ms,
        "source": "bot_order_store",
        "note": "bot-owned fills only; manual UI inventory is not included",
        "positions": normalized,
    }


def sync_once(args: argparse.Namespace) -> int:
    out_dir = Path(args.out_dir)
    out_dir.mkdir(parents=True, exist_ok=True)
    state_path = out_dir / "state.json"
    orders_jsonl = out_dir / "orders.jsonl"
    fills_jsonl = out_dir / "fills.jsonl"
    orders_csv = out_dir / "orders.csv"
    position_events_jsonl = out_dir / "position_events.jsonl"
    position_events_csv = out_dir / "position_events.csv"
    latest_positions_csv = out_dir / "latest_positions.csv"
    snapshots_jsonl = out_dir / "positions_snapshot.jsonl"

    observed_at_ms = now_ms()
    with tempfile.TemporaryDirectory(prefix="pm-live-order-store-") as tmp_dir:
        local_db = Path(tmp_dir) / "order-store.sqlite"
        pull_remote_db(args, local_db)
        rows = load_rows(local_db)

    state = load_seen(state_path)
    seen = state["seen_event_ids"]
    last_filled_qty_by_order = state["last_filled_qty_by_order"]
    new_records = []
    new_fills = []
    position_events = []
    for row in rows:
        eid = event_id(row)
        client_order_id = str(row["client_order_id"])
        filled_qty = float(row["filled_qty"] or 0.0)
        previous_filled_qty = float(last_filled_qty_by_order.get(client_order_id, 0.0))
        fill_delta_qty = max(0.0, filled_qty - previous_filled_qty)
        if fill_delta_qty > 0.0:
            position_events.append(build_position_event(row, observed_at_ms, fill_delta_qty))
        last_filled_qty_by_order[client_order_id] = max(previous_filled_qty, filled_qty)

        if eid in seen:
            continue
        seen.add(eid)
        record = dict(row)
        record["observed_at_ms"] = observed_at_ms
        new_records.append(record)
        if filled_qty > 0.0:
            new_fills.append(record)

    append_jsonl(orders_jsonl, new_records)
    append_jsonl(fills_jsonl, new_fills)
    append_jsonl(position_events_jsonl, position_events)
    append_csv(orders_csv, new_records)
    append_position_events_csv(position_events_csv, position_events)
    snapshot = build_position_snapshot(rows, observed_at_ms)
    append_jsonl(snapshots_jsonl, [snapshot])
    write_latest_positions_csv(latest_positions_csv, snapshot)
    save_seen(state_path, seen, last_filled_qty_by_order)
    return len(new_records)


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(
        description="Pull Bonereaper live bot orders/fills from AWS to local files."
    )
    parser.add_argument("--host", default=os.environ.get("AWS_LIVE_HOST", DEFAULT_HOST))
    parser.add_argument("--user", default=os.environ.get("AWS_LIVE_USER", DEFAULT_USER))
    parser.add_argument("--port", type=int, default=int(os.environ.get("AWS_LIVE_PORT", "22")))
    parser.add_argument("--key", default=os.environ.get("AWS_LIVE_KEY_PATH", DEFAULT_KEY))
    parser.add_argument("--remote-db", default=os.environ.get("PM_LIVE_REMOTE_DB", DEFAULT_REMOTE_DB))
    parser.add_argument("--out-dir", default=os.environ.get("PM_LIVE_LOCAL_OUT_DIR", DEFAULT_OUT_DIR))
    parser.add_argument("--poll-seconds", type=float, default=0.0)
    return parser.parse_args()


def main() -> int:
    args = parse_args()
    if args.poll_seconds <= 0.0:
        count = sync_once(args)
        print(f"synced {count} new order events into {args.out_dir}")
        return 0

    while True:
        try:
            count = sync_once(args)
            print(f"synced {count} new order events into {args.out_dir}", flush=True)
        except KeyboardInterrupt:
            return 130
        except Exception as exc:  # noqa: BLE001 - operator-facing poller
            print(f"live position sync failed: {exc}", file=sys.stderr, flush=True)
        time.sleep(args.poll_seconds)


if __name__ == "__main__":
    raise SystemExit(main())
