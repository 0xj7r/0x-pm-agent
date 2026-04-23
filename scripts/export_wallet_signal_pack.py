#!/usr/bin/env python3
"""Export per-window signal features from a wallet research SQLite database.

This is aimed at reconstructing a wallet's activation regime and intrawindow
control policy from the local research DB. It is grounded first on
`unlawful-shear`, but the schema handling is generic enough for other wallets
with the same collection layout.
"""

from __future__ import annotations

import argparse
import bisect
import json
import math
import sqlite3
import statistics
from collections import Counter, defaultdict
from dataclasses import dataclass
from datetime import datetime, timezone
from pathlib import Path
from typing import Any


def iso_now() -> str:
    return datetime.now(tz=timezone.utc).replace(microsecond=0).isoformat()


def iso_to_epoch_ms(value: str | None) -> int | None:
    if not value:
        return None
    try:
        return int(datetime.fromisoformat(value.replace("Z", "+00:00")).timestamp() * 1000)
    except ValueError:
        return None


def safe_float(value: Any) -> float | None:
    if value is None:
        return None
    try:
        return float(value)
    except (TypeError, ValueError):
        return None


def safe_int(value: Any) -> int | None:
    if value is None:
        return None
    try:
        return int(value)
    except (TypeError, ValueError):
        return None


def median_or_none(values: list[float]) -> float | None:
    cleaned = [v for v in values if v is not None and math.isfinite(v)]
    if not cleaned:
        return None
    return float(statistics.median(cleaned))


def mean_or_none(values: list[float]) -> float | None:
    cleaned = [v for v in values if v is not None and math.isfinite(v)]
    if not cleaned:
        return None
    return float(sum(cleaned) / len(cleaned))


def min_or_none(values: list[float]) -> float | None:
    cleaned = [v for v in values if v is not None and math.isfinite(v)]
    if not cleaned:
        return None
    return float(min(cleaned))


def max_or_none(values: list[float]) -> float | None:
    cleaned = [v for v in values if v is not None and math.isfinite(v)]
    if not cleaned:
        return None
    return float(max(cleaned))


def stdev_or_none(values: list[float]) -> float | None:
    cleaned = [v for v in values if v is not None and math.isfinite(v)]
    if len(cleaned) < 2:
        return None
    return float(statistics.pstdev(cleaned))


def return_bps(new: float | None, old: float | None) -> float | None:
    if new is None or old is None or old == 0:
        return None
    return (new / old - 1.0) * 10_000.0


def realized_vol_bps(closes: list[float]) -> float | None:
    if len(closes) < 2:
        return None
    log_returns = []
    for prev, curr in zip(closes[:-1], closes[1:]):
        if prev is None or curr is None or prev <= 0 or curr <= 0:
            continue
        log_returns.append(math.log(curr / prev))
    if not log_returns:
        return None
    return math.sqrt(sum(ret * ret for ret in log_returns)) * 10_000.0


@dataclass
class CandleIndex:
    rows: list[dict[str, Any]]

    def __post_init__(self) -> None:
        self.close_times = [safe_int(row["close_time_ms"]) or 0 for row in self.rows]

    def last_closed_idx(self, start_ms: int) -> int:
        return bisect.bisect_right(self.close_times, start_ms) - 1

    def features_before(self, start_ms: int) -> dict[str, Any]:
        idx = self.last_closed_idx(start_ms)
        if idx < 0:
            return {"has_btc_features": False}

        latest = self.rows[idx]
        latest_close_ms = safe_int(latest["close_time_ms"])
        latest_close = safe_float(latest["close"])
        out: dict[str, Any] = {
            "has_btc_features": latest_close is not None,
            "btc_data_lag_s": ((start_ms - latest_close_ms) / 1000.0) if latest_close_ms else None,
            "btc_last_close": latest_close,
        }
        if latest_close is None:
            return out

        for horizon in (1, 3, 5, 15):
            prev_idx = idx - horizon
            prev_close = safe_float(self.rows[prev_idx]["close"]) if prev_idx >= 0 else None
            out[f"btc_prev_{horizon}m_return_bps"] = return_bps(latest_close, prev_close)

            slice_start = max(0, idx - horizon + 1)
            sample = self.rows[slice_start : idx + 1]
            closes = [safe_float(row["close"]) for row in sample]
            highs = [safe_float(row["high"]) for row in sample]
            lows = [safe_float(row["low"]) for row in sample]
            trade_counts = [safe_int(row["trade_count"]) or 0 for row in sample]

            out[f"btc_realized_vol_{horizon}m_bps"] = realized_vol_bps(
                [c for c in closes if c is not None]
            )
            high = max_or_none([h for h in highs if h is not None])
            low = min_or_none([l for l in lows if l is not None])
            out[f"btc_range_{horizon}m_bps"] = return_bps(high, low) if high and low else None
            out[f"btc_trade_count_{horizon}m"] = sum(trade_counts) if sample else None

        return out


def table_exists(conn: sqlite3.Connection, table_name: str) -> bool:
    row = conn.execute(
        "SELECT 1 FROM sqlite_master WHERE type='table' AND name=?",
        (table_name,),
    ).fetchone()
    return row is not None


def rows_to_dicts(cursor: sqlite3.Cursor) -> list[dict[str, Any]]:
    return [dict(row) for row in cursor.fetchall()]


def load_by_slug(
    conn: sqlite3.Connection,
    table: str,
    columns: str,
    slug_filter: set[str] | None = None,
    order_by: str | None = None,
) -> dict[str, list[dict[str, Any]]]:
    sql = f"SELECT {columns} FROM {table}"
    params: list[Any] = []
    if slug_filter:
        placeholders = ",".join("?" for _ in slug_filter)
        sql += f" WHERE slug IN ({placeholders})"
        params.extend(sorted(slug_filter))
    if order_by:
        sql += f" ORDER BY {order_by}"
    grouped: dict[str, list[dict[str, Any]]] = defaultdict(list)
    cur = conn.execute(sql, params)
    for row in rows_to_dicts(cur):
        slug = row.get("slug")
        if slug:
            grouped[str(slug)].append(row)
    return grouped


def pair_book_rows(
    rows: list[dict[str, Any]],
    token_ids: list[str],
    max_gap_ms: int = 250,
) -> list[dict[str, Any]]:
    if len(token_ids) < 2:
        return []
    wanted = set(token_ids[:2])
    pending: dict[str, dict[str, Any]] = {}
    pairs: list[dict[str, Any]] = []
    sorted_rows = sorted(
        (
            {
                **row,
                "captured_at_ms": iso_to_epoch_ms(row.get("captured_at")),
            }
            for row in rows
            if row.get("token_id") in wanted
        ),
        key=lambda row: (row.get("captured_at_ms") or 0, row.get("token_id") or ""),
    )
    for row in sorted_rows:
        ts_ms = row.get("captured_at_ms")
        token_id = row.get("token_id")
        if ts_ms is None or token_id is None:
            continue

        stale = [
            other_token
            for other_token, other_row in pending.items()
            if other_row.get("captured_at_ms") is None
            or ts_ms - other_row["captured_at_ms"] > max_gap_ms
        ]
        for other_token in stale:
            pending.pop(other_token, None)

        match_token = next(
            (
                other_token
                for other_token, other_row in pending.items()
                if other_token != token_id
                and abs(ts_ms - (other_row.get("captured_at_ms") or 0)) <= max_gap_ms
            ),
            None,
        )
        if match_token is None:
            pending[str(token_id)] = row
            continue

        other = pending.pop(match_token)
        bid_1 = safe_float(other.get("best_bid"))
        ask_1 = safe_float(other.get("best_ask"))
        bid_2 = safe_float(row.get("best_bid"))
        ask_2 = safe_float(row.get("best_ask"))
        bid_size_1 = safe_float(other.get("bid_size"))
        ask_size_1 = safe_float(other.get("ask_size"))
        bid_size_2 = safe_float(row.get("bid_size"))
        ask_size_2 = safe_float(row.get("ask_size"))
        spread_1 = (ask_1 - bid_1) if ask_1 is not None and bid_1 is not None else None
        spread_2 = (ask_2 - bid_2) if ask_2 is not None and bid_2 is not None else None

        pairs.append(
            {
                "captured_at_ms": max(ts_ms, other.get("captured_at_ms") or ts_ms),
                "bid_sum": (bid_1 + bid_2) if bid_1 is not None and bid_2 is not None else None,
                "ask_sum": (ask_1 + ask_2) if ask_1 is not None and ask_2 is not None else None,
                "ask_gap": abs(ask_1 - ask_2) if ask_1 is not None and ask_2 is not None else None,
                "depth_sum": (
                    sum(v for v in (bid_size_1, ask_size_1, bid_size_2, ask_size_2) if v is not None)
                    if any(v is not None for v in (bid_size_1, ask_size_1, bid_size_2, ask_size_2))
                    else None
                ),
                "spread_sum": (
                    spread_1 + spread_2 if spread_1 is not None and spread_2 is not None else None
                ),
            }
        )
    return pairs


def summarize_book_pairs(pairs: list[dict[str, Any]], start_ms: int) -> dict[str, Any]:
    first_60 = [pair for pair in pairs if pair["captured_at_ms"] <= start_ms + 60_000]
    ask_sums_60 = [safe_float(pair.get("ask_sum")) for pair in first_60]
    bid_sums_60 = [safe_float(pair.get("bid_sum")) for pair in first_60]
    ask_gaps_60 = [safe_float(pair.get("ask_gap")) for pair in first_60]
    depth_sums_60 = [safe_float(pair.get("depth_sum")) for pair in first_60]
    spread_sums_60 = [safe_float(pair.get("spread_sum")) for pair in first_60]

    return {
        "has_book_features": len(pairs) > 0,
        "book_pair_count": len(pairs),
        "book_pair_count_first_60s": len(first_60),
        "book_first_pair_lag_s": ((pairs[0]["captured_at_ms"] - start_ms) / 1000.0) if pairs else None,
        "book_ask_sum_min_first_60s": min_or_none([v for v in ask_sums_60 if v is not None]),
        "book_ask_sum_median_first_60s": median_or_none([v for v in ask_sums_60 if v is not None]),
        "book_bid_sum_max_first_60s": max_or_none([v for v in bid_sums_60 if v is not None]),
        "book_bid_sum_median_first_60s": median_or_none([v for v in bid_sums_60 if v is not None]),
        "book_ask_gap_median_first_60s": median_or_none([v for v in ask_gaps_60 if v is not None]),
        "book_depth_sum_median_first_60s": median_or_none([v for v in depth_sums_60 if v is not None]),
        "book_spread_sum_median_first_60s": median_or_none([v for v in spread_sums_60 if v is not None]),
    }


def summarize_stream(rows: list[dict[str, Any]], start_ms: int) -> dict[str, Any]:
    if not rows:
        return {"has_stream_features": False}
    event_times = [iso_to_epoch_ms(row.get("captured_at")) for row in rows]
    event_times = [value for value in event_times if value is not None]
    spreads = [safe_float(row.get("spread")) for row in rows]
    trade_rows = [
        row
        for row in rows
        if safe_float(row.get("trade_price")) is not None or safe_float(row.get("trade_size")) is not None
    ]
    trade_sizes = [safe_float(row.get("trade_size")) for row in trade_rows]
    trade_sizes = [value for value in trade_sizes if value is not None]

    trade_buy_size = sum(
        safe_float(row.get("trade_size")) or 0.0
        for row in trade_rows
        if str(row.get("trade_side") or "").lower() == "buy"
    )
    trade_sell_size = sum(
        safe_float(row.get("trade_size")) or 0.0
        for row in trade_rows
        if str(row.get("trade_side") or "").lower() == "sell"
    )
    source_counts = Counter(str(row.get("source") or "unknown") for row in rows)
    event_type_counts = Counter(str(row.get("event_type") or "unknown") for row in rows)

    return {
        "has_stream_features": True,
        "stream_event_count": len(rows),
        "stream_book_event_count": sum(
            1 for row in rows if safe_float(row.get("best_bid")) is not None and safe_float(row.get("best_ask")) is not None
        ),
        "stream_trade_event_count": len(trade_rows),
        "stream_first_capture_lag_s": ((min(event_times) - start_ms) / 1000.0) if event_times else None,
        "stream_last_capture_lag_s": ((max(event_times) - start_ms) / 1000.0) if event_times else None,
        "stream_trade_size_total": sum(trade_sizes) if trade_sizes else None,
        "stream_trade_size_buy": trade_buy_size if trade_rows else None,
        "stream_trade_size_sell": trade_sell_size if trade_rows else None,
        "stream_spread_median": median_or_none([value for value in spreads if value is not None]),
        "stream_spread_min": min_or_none([value for value in spreads if value is not None]),
        "stream_source_counts": dict(source_counts),
        "stream_event_type_counts": dict(event_type_counts),
    }


def summarize_activity(
    rows: list[dict[str, Any]],
    start_ts: int,
    cheap_leg: str | None,
    expensive_leg: str | None,
) -> dict[str, Any]:
    trade_rows = [
        row
        for row in rows
        if str(row.get("activity_type") or "").upper() == "TRADE"
        and str(row.get("side") or "").upper() == "BUY"
    ]
    merge_rows = [row for row in rows if str(row.get("activity_type") or "").upper() == "MERGE"]
    trade_ts = sorted(safe_int(row.get("event_ts")) for row in trade_rows)
    trade_ts = [value for value in trade_ts if value is not None]

    clips_total: list[float] = []
    clips_cheap: list[float] = []
    clips_expensive: list[float] = []
    cheap_notional = 0.0
    expensive_notional = 0.0
    cheap_count = 0
    expensive_count = 0
    trade_count_30 = 0
    trade_count_60 = 0

    for row in trade_rows:
        event_ts = safe_int(row.get("event_ts")) or 0
        outcome = str(row.get("outcome") or "")
        notional = safe_float(row.get("usdc_size"))
        if notional is None:
            price = safe_float(row.get("price"))
            size = safe_float(row.get("size"))
            notional = (price * size) if price is not None and size is not None else None
        if notional is not None:
            clips_total.append(notional)
        if event_ts <= start_ts + 30:
            trade_count_30 += 1
        if event_ts <= start_ts + 60:
            trade_count_60 += 1

        if outcome == cheap_leg:
            cheap_count += 1
            if notional is not None:
                cheap_notional += notional
                clips_cheap.append(notional)
        elif outcome == expensive_leg:
            expensive_count += 1
            if notional is not None:
                expensive_notional += notional
                clips_expensive.append(notional)

    merge_ts = sorted(safe_int(row.get("event_ts")) for row in merge_rows)
    merge_ts = [value for value in merge_ts if value is not None]
    first_trade_ts = trade_ts[0] if trade_ts else None
    first_merge_ts = merge_ts[0] if merge_ts else None
    median_trade_gap = None
    if len(trade_ts) >= 2:
        gaps = [curr - prev for prev, curr in zip(trade_ts[:-1], trade_ts[1:])]
        median_trade_gap = statistics.median(gaps) / 1000.0 if gaps else None

    merge_60 = 0
    merge_120 = 0
    if first_trade_ts is not None:
        merge_60 = sum(1 for ts in merge_ts if ts <= first_trade_ts + 60)
        merge_120 = sum(1 for ts in merge_ts if ts <= first_trade_ts + 120)

    return {
        "has_activity_features": True,
        "trade_count_total": len(trade_rows),
        "trade_count_cheap": cheap_count,
        "trade_count_expensive": expensive_count,
        "trade_count_first_30s": trade_count_30,
        "trade_count_first_60s": trade_count_60,
        "notional_total_usd": sum(clips_total) if clips_total else None,
        "notional_cheap_usd": cheap_notional if clips_cheap else None,
        "notional_expensive_usd": expensive_notional if clips_expensive else None,
        "avg_clip_usd_total": mean_or_none(clips_total),
        "avg_clip_usd_cheap": mean_or_none(clips_cheap),
        "avg_clip_usd_expensive": mean_or_none(clips_expensive),
        "median_clip_usd_total": median_or_none(clips_total),
        "cheap_to_expensive_notional_ratio": (
            cheap_notional / expensive_notional if clips_cheap and clips_expensive and expensive_notional > 0 else None
        ),
        "median_inter_trade_gap_s": median_trade_gap,
        "first_merge_lag_from_start_s": (
            first_merge_ts - start_ts if first_merge_ts is not None else None
        ),
        "first_merge_lag_from_entry_s": (
            first_merge_ts - first_trade_ts if first_merge_ts is not None and first_trade_ts is not None else None
        ),
        "merge_count_total": len(merge_rows),
        "merge_count_first_60s_from_entry": merge_60,
        "merge_count_first_120s_from_entry": merge_120,
        "merges_present": bool(merge_rows),
    }


def load_windows(conn: sqlite3.Connection, limit: int | None) -> list[dict[str, Any]]:
    sql = """
        SELECT
            window_key,
            wallet_address,
            slug,
            market_id,
            start_ts,
            end_ts,
            first_trade_ts,
            last_trade_ts,
            buy_rows,
            sell_rows,
            merge_rows,
            up_buy_cost,
            down_buy_cost,
            up_buy_shares,
            down_buy_shares,
            up_avg_buy_price,
            down_avg_buy_price,
            up_realized_pnl,
            down_realized_pnl,
            combined_realized_pnl,
            paired_outcomes,
            cheap_leg,
            cheap_leg_avg_price,
            expensive_leg,
            expensive_leg_avg_price,
            hedge_cost_ratio
        FROM wallet_window_reconstructions
        WHERE paired_outcomes = 1
        ORDER BY start_ts
    """
    if limit:
        sql += f" LIMIT {int(limit)}"
    return rows_to_dicts(conn.execute(sql))


def build_pack(db_path: Path, limit: int | None = None) -> dict[str, Any]:
    conn = sqlite3.connect(str(db_path))
    conn.row_factory = sqlite3.Row

    windows = load_windows(conn, limit)
    slugs = {str(window["slug"]) for window in windows}
    wallet = next((str(window["wallet_address"]) for window in windows if window.get("wallet_address")), None)

    activity_by_slug = load_by_slug(
        conn,
        "wallet_activity_raw",
        "event_ts, activity_type, side, outcome, slug, usdc_size, size, price",
        slugs,
        "slug, event_ts",
    )
    catalog_by_slug = {
        row["slug"]: row
        for row in rows_to_dicts(
            conn.execute(
                """
                SELECT
                    market_id,
                    slug,
                    asset,
                    market_family,
                    outcome_names_json,
                    token_ids_json,
                    end_time,
                    event_start_time,
                    price_to_beat,
                    final_price
                FROM wallet_market_catalog
                """
            )
        )
        if row.get("slug") in slugs
    }
    orderbooks_by_slug = (
        load_by_slug(
            conn,
            "wallet_orderbook_snapshots",
            "captured_at, slug, token_id, best_bid, best_ask, bid_size, ask_size",
            slugs,
            "slug, captured_at",
        )
        if table_exists(conn, "wallet_orderbook_snapshots")
        else {}
    )
    streams_by_slug = (
        load_by_slug(
            conn,
            "wallet_market_stream_events",
            "captured_at, slug, token_id, source, event_type, best_bid, best_ask, spread, trade_price, trade_size, trade_side",
            slugs,
            "slug, captured_at",
        )
        if table_exists(conn, "wallet_market_stream_events")
        else {}
    )
    btc_rows = (
        rows_to_dicts(
            conn.execute(
                """
                SELECT open_time_ms, close_time_ms, open, high, low, close, volume, trade_count
                FROM wallet_btc_price_series
                ORDER BY open_time_ms
                """
            )
        )
        if table_exists(conn, "wallet_btc_price_series")
        else []
    )
    btc_index = CandleIndex(btc_rows) if btc_rows else None

    exported_windows: list[dict[str, Any]] = []
    coverage = Counter()
    hourly_summary: dict[int, list[dict[str, Any]]] = defaultdict(list)

    for window in windows:
        slug = str(window["slug"])
        start_ts = safe_int(window.get("start_ts")) or 0
        end_ts = safe_int(window.get("end_ts")) or 0
        start_ms = start_ts * 1000
        cheap_leg = window.get("cheap_leg")
        expensive_leg = window.get("expensive_leg")
        metadata = catalog_by_slug.get(slug, {})
        token_ids = []
        if metadata.get("token_ids_json"):
            try:
                token_ids = json.loads(metadata["token_ids_json"])
            except (TypeError, json.JSONDecodeError):
                token_ids = []

        activity_features = summarize_activity(
            activity_by_slug.get(slug, []),
            start_ts,
            str(cheap_leg) if cheap_leg else None,
            str(expensive_leg) if expensive_leg else None,
        )
        book_pairs = pair_book_rows(orderbooks_by_slug.get(slug, []), token_ids)
        book_features = summarize_book_pairs(book_pairs, start_ms)
        stream_features = summarize_stream(streams_by_slug.get(slug, []), start_ms)
        btc_features = btc_index.features_before(start_ms) if btc_index else {"has_btc_features": False}

        record = {
            "window_key": window["window_key"],
            "slug": slug,
            "market_id": window.get("market_id"),
            "asset": metadata.get("asset"),
            "market_family": metadata.get("market_family"),
            "start_ts": start_ts,
            "end_ts": end_ts,
            "duration_s": (end_ts - start_ts) if end_ts and start_ts else None,
            "hour_of_day_utc": datetime.fromtimestamp(start_ts, tz=timezone.utc).hour if start_ts else None,
            "day_of_week_utc": datetime.fromtimestamp(start_ts, tz=timezone.utc).weekday() if start_ts else None,
            "paired_outcomes": safe_int(window.get("paired_outcomes")),
            "buy_rows": safe_int(window.get("buy_rows")),
            "sell_rows": safe_int(window.get("sell_rows")),
            "merge_rows": safe_int(window.get("merge_rows")),
            "first_trade_ts": safe_int(window.get("first_trade_ts")),
            "last_trade_ts": safe_int(window.get("last_trade_ts")),
            "entry_lag_s": (
                (safe_int(window.get("first_trade_ts")) - start_ts) if safe_int(window.get("first_trade_ts")) is not None else None
            ),
            "entered_within_30s": (
                safe_int(window.get("first_trade_ts")) is not None
                and (safe_int(window.get("first_trade_ts")) - start_ts) <= 30
            ),
            "entered_within_60s": (
                safe_int(window.get("first_trade_ts")) is not None
                and (safe_int(window.get("first_trade_ts")) - start_ts) <= 60
            ),
            "time_remaining_after_entry_s": (
                (end_ts - safe_int(window.get("first_trade_ts")))
                if safe_int(window.get("first_trade_ts")) is not None and end_ts
                else None
            ),
            "cheap_leg": cheap_leg,
            "cheap_leg_avg_price": safe_float(window.get("cheap_leg_avg_price")),
            "expensive_leg": expensive_leg,
            "expensive_leg_avg_price": safe_float(window.get("expensive_leg_avg_price")),
            "price_gap": (
                safe_float(window.get("expensive_leg_avg_price")) - safe_float(window.get("cheap_leg_avg_price"))
                if safe_float(window.get("expensive_leg_avg_price")) is not None
                and safe_float(window.get("cheap_leg_avg_price")) is not None
                else None
            ),
            "hedge_cost_ratio": safe_float(window.get("hedge_cost_ratio")),
            "up_avg_buy_price": safe_float(window.get("up_avg_buy_price")),
            "down_avg_buy_price": safe_float(window.get("down_avg_buy_price")),
            "up_buy_cost": safe_float(window.get("up_buy_cost")),
            "down_buy_cost": safe_float(window.get("down_buy_cost")),
            "combined_realized_pnl": safe_float(window.get("combined_realized_pnl")),
            **activity_features,
            **book_features,
            **stream_features,
            **btc_features,
        }
        exported_windows.append(record)

        if record.get("has_activity_features"):
            coverage["windows_with_activity_features"] += 1
        if record.get("has_book_features"):
            coverage["windows_with_book_features"] += 1
        if record.get("has_stream_features"):
            coverage["windows_with_stream_features"] += 1
        if record.get("has_btc_features"):
            coverage["windows_with_btc_features"] += 1

        hour = record.get("hour_of_day_utc")
        if hour is not None:
            hourly_summary[int(hour)].append(record)

    regime_hourly = []
    for hour in sorted(hourly_summary):
        rows = hourly_summary[hour]
        regime_hourly.append(
            {
                "hour_of_day_utc": hour,
                "window_count": len(rows),
                "avg_buy_rows": mean_or_none([safe_float(row.get("buy_rows")) for row in rows]),
                "avg_merge_rows": mean_or_none([safe_float(row.get("merge_rows")) for row in rows]),
                "avg_hedge_cost_ratio": mean_or_none([safe_float(row.get("hedge_cost_ratio")) for row in rows]),
                "avg_entry_lag_s": mean_or_none([safe_float(row.get("entry_lag_s")) for row in rows]),
                "avg_first_merge_lag_from_entry_s": mean_or_none(
                    [safe_float(row.get("first_merge_lag_from_entry_s")) for row in rows]
                ),
                "book_feature_coverage": sum(1 for row in rows if row.get("has_book_features")) / len(rows),
                "stream_feature_coverage": sum(1 for row in rows if row.get("has_stream_features")) / len(rows),
                "btc_feature_coverage": sum(1 for row in rows if row.get("has_btc_features")) / len(rows),
            }
        )

    return {
        "metadata": {
            "wallet": wallet,
            "db_path": str(db_path),
            "generated_at": iso_now(),
            "paired_windows": len(exported_windows),
        },
        "coverage": dict(coverage),
        "regime_hourly": regime_hourly,
        "windows": exported_windows,
    }


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--db", required=True, help="Path to wallet_research.db")
    parser.add_argument("--out", help="Write JSON to this path instead of stdout")
    parser.add_argument("--limit-windows", type=int, default=None)
    args = parser.parse_args()

    db_path = Path(args.db)
    pack = build_pack(db_path, limit=args.limit_windows)
    rendered = json.dumps(pack, indent=2, sort_keys=False)

    if args.out:
        out_path = Path(args.out)
        out_path.parent.mkdir(parents=True, exist_ok=True)
        out_path.write_text(rendered + "\n")
    else:
        print(rendered)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
