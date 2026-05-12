#!/usr/bin/env python3
"""Validate one raw Telonex/Binance backtest window.

This is intentionally narrower than `backtest_runner`: it checks the source
rows needed to trust a replay fixture before any strategy code runs.
"""

from __future__ import annotations

import argparse
import datetime as dt
import json
from pathlib import Path
from typing import Any

import pyarrow.parquet as pq
import pyarrow.compute as pc


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--raw-root", required=True, type=Path)
    parser.add_argument("--slug", required=True)
    parser.add_argument("--yes-asset", required=True)
    parser.add_argument("--no-asset", required=True)
    parser.add_argument("--date", required=True)
    parser.add_argument("--output", type=Path)
    return parser.parse_args()


def slug_window_ms(slug: str) -> tuple[int, int]:
    start_s = int(slug.rsplit("-", 1)[1])
    return start_s * 1000, (start_s + 300) * 1000


def read_rows(path: Path, columns: list[str] | None = None) -> list[dict[str, Any]]:
    if not path.exists():
        return []
    rows: list[dict[str, Any]] = []
    for file in path.rglob("*.parquet"):
        parquet = pq.ParquetFile(file)
        for batch in parquet.iter_batches(batch_size=65_536, columns=columns):
            rows.extend(batch.to_pylist())
    return rows


def read_filtered_rows(
    path: Path,
    columns: list[str],
    timestamp_col: str,
    start_value: int,
    end_value: int,
    equals: dict[str, str] | None = None,
) -> list[dict[str, Any]]:
    if not path.exists():
        return []
    rows: list[dict[str, Any]] = []
    for file in path.rglob("*.parquet"):
        table = pq.ParquetFile(file).read(columns=columns)
        mask = pc.and_(
            pc.greater_equal(table[timestamp_col], start_value),
            pc.less(table[timestamp_col], end_value),
        )
        for name, value in (equals or {}).items():
            mask = pc.and_(mask, pc.equal(table[name], value))
        filtered = table.filter(mask)
        if filtered.num_rows:
            rows.extend(filtered.to_pylist())
    return rows


def f(value: Any) -> float | None:
    if value is None or value == "":
        return None
    try:
        return float(value)
    except (TypeError, ValueError):
        return None


def levels(row: dict[str, Any], side: str) -> list[tuple[float, float]]:
    out: list[tuple[float, float]] = []
    for idx in range(25):
        price = f(row.get(f"{side}_price_{idx}"))
        size = f(row.get(f"{side}_size_{idx}"))
        if price is None and size is None:
            break
        if price is None or size is None:
            continue
        out.append((price, size))
    return out


def validate_asset(
    raw_root: Path,
    date: str,
    slug: str,
    asset_id: str,
    start_ms: int,
    end_ms: int,
) -> dict[str, Any]:
    path = (
        raw_root
        / "telonex"
        / "exchange=polymarket"
        / "channel=book_snapshot_25"
        / f"date={date}"
        / f"asset_id={asset_id}"
    )
    columns = [
        "timestamp_us",
        "local_timestamp_us",
        "slug",
        "asset_id",
        "bid_price_0",
        "bid_size_0",
        "ask_price_0",
        "ask_size_0",
    ]
    rows = read_filtered_rows(
        path,
        columns,
        "local_timestamp_us",
        start_ms * 1000,
        end_ms * 1000,
        {"slug": slug, "asset_id": asset_id},
    )

    invalid_prices = 0
    invalid_sizes = 0
    empty_books = 0
    crossed = []
    locked = 0
    timestamps: dict[int, int] = {}

    for row in rows:
        ts = int(row.get("local_timestamp_us", 0))
        timestamps[ts] = timestamps.get(ts, 0) + 1
        bids = levels(row, "bid")
        asks = levels(row, "ask")
        if not bids and not asks:
            empty_books += 1
            continue
        for price, size in bids + asks:
            if price < 0.0 or price > 1.0:
                invalid_prices += 1
            if size < 0.0:
                invalid_sizes += 1
        best_bid = max((p for p, _ in bids), default=None)
        best_ask = min((p for p, _ in asks), default=None)
        if best_bid is None or best_ask is None:
            continue
        if best_bid > best_ask:
            crossed.append(
                {
                    "local_timestamp_us": ts,
                    "exchange_timestamp_us": int(row.get("timestamp_us", 0)),
                    "best_bid": best_bid,
                    "best_ask": best_ask,
                    "spread": best_ask - best_bid,
                }
            )
        elif best_bid == best_ask:
            locked += 1

    crossed_spans_us = crossed_durations_us(rows)
    duplicate_timestamp_rows = sum(count for count in timestamps.values() if count > 1)
    return {
        "asset_id": asset_id,
        "snapshot_rows": len(rows),
        "unique_local_timestamps": len(timestamps),
        "duplicate_timestamp_rows": duplicate_timestamp_rows,
        "empty_books": empty_books,
        "invalid_prices": invalid_prices,
        "invalid_sizes": invalid_sizes,
        "locked_rows": locked,
        "crossed_rows": len(crossed),
        "crossed_span_count": len(crossed_spans_us),
        "crossed_span_p50_us": percentile(crossed_spans_us, 0.50),
        "crossed_span_p90_us": percentile(crossed_spans_us, 0.90),
        "crossed_span_p99_us": percentile(crossed_spans_us, 0.99),
        "crossed_span_max_us": max(crossed_spans_us) if crossed_spans_us else 0,
        "crossed_spans_over_1s": sum(span > 1_000_000 for span in crossed_spans_us),
        "crossed_examples": crossed[:10],
    }


def crossed_durations_us(rows: list[dict[str, Any]]) -> list[int]:
    ordered = sorted(rows, key=lambda row: int(row.get("local_timestamp_us", 0)))
    spans: list[int] = []
    open_ts: int | None = None
    for row in ordered:
        ts = int(row.get("local_timestamp_us", 0))
        bid = f(row.get("bid_price_0"))
        ask = f(row.get("ask_price_0"))
        is_crossed = bid is not None and ask is not None and bid > ask
        if is_crossed and open_ts is None:
            open_ts = ts
        elif not is_crossed and open_ts is not None:
            spans.append(ts - open_ts)
            open_ts = None
    if open_ts is not None and ordered:
        spans.append(int(ordered[-1].get("local_timestamp_us", 0)) - open_ts)
    return spans


def percentile(values: list[int], q: float) -> int:
    if not values:
        return 0
    ordered = sorted(values)
    return ordered[min(len(ordered) - 1, int(len(ordered) * q))]


def validate_btc(raw_root: Path, date: str, start_ms: int, end_ms: int) -> dict[str, Any]:
    path = (
        raw_root
        / "binance"
        / "exchange=binance"
        / "channel=agg_trades"
        / "symbol=BTCUSDT"
        / f"date={date}"
    )
    rows = read_filtered_rows(
        path,
        ["transact_time_ms", "price"],
        "transact_time_ms",
        start_ms * 1000,
        end_ms * 1000,
    )
    prices = [f(row.get("price")) for row in rows]
    prices = [price for price in prices if price is not None]
    return {
        "tick_rows": len(rows),
        "first_price": prices[0] if prices else None,
        "last_price": prices[-1] if prices else None,
        "min_price": min(prices) if prices else None,
        "max_price": max(prices) if prices else None,
    }


def classify(report: dict[str, Any]) -> str:
    if report["btc"]["tick_rows"] == 0:
        return "reject:no_btc_ticks"
    for leg in ("yes", "no"):
        asset = report["books"][leg]
        if asset["snapshot_rows"] == 0:
            return f"reject:no_{leg}_book_snapshots"
        if asset["invalid_prices"] or asset["invalid_sizes"]:
            return f"reject:invalid_{leg}_levels"
        if asset["crossed_spans_over_1s"] > 0 or asset["crossed_span_max_us"] > 250_000:
            return f"reject:persistent_crossed_{leg}_book"
    return "pass"


def main() -> int:
    args = parse_args()
    start_ms, end_ms = slug_window_ms(args.slug)
    report = {
        "slug": args.slug,
        "date": args.date,
        "window_utc": {
            "start": dt.datetime.fromtimestamp(start_ms / 1000, dt.UTC).isoformat(),
            "end": dt.datetime.fromtimestamp(end_ms / 1000, dt.UTC).isoformat(),
        },
        "books": {
            "yes": validate_asset(args.raw_root, args.date, args.slug, args.yes_asset, start_ms, end_ms),
            "no": validate_asset(args.raw_root, args.date, args.slug, args.no_asset, start_ms, end_ms),
        },
        "btc": validate_btc(args.raw_root, args.date, start_ms, end_ms),
    }
    report["status"] = classify(report)
    rendered = json.dumps(report, indent=2, sort_keys=True)
    if args.output:
        args.output.parent.mkdir(parents=True, exist_ok=True)
        args.output.write_text(rendered + "\n", encoding="utf-8")
    print(rendered)
    return 0 if report["status"] == "pass" else 2


if __name__ == "__main__":
    raise SystemExit(main())
