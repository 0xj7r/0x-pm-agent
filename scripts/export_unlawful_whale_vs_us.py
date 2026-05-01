#!/usr/bin/env python3
"""Export window-by-window unlawful whale-vs-us calibration artifacts.

The script compares:
- live unlawful activity from data-api.polymarket.com/activity
- local paper sleeve journals + SQLite order stores

It emits:
- a compact JSON artifact with one entry per 5m window
- a flattened CSV for quick sorting/filtering

The intent is calibration, not perfect venue truth. Local paper artifacts may be
missing or partially written; the script treats that as empty data rather than
failing the whole export.
"""

from __future__ import annotations

import argparse
import csv
import json
import re
import sqlite3
import subprocess
import sys
import urllib.parse
import urllib.request
from collections import Counter
from dataclasses import dataclass
from datetime import datetime, timezone
from pathlib import Path
from typing import Any


ACTIVITY_API = "https://data-api.polymarket.com/activity"
WINDOW_MS = 5 * 60 * 1000
DEFAULT_WHALE_WALLET = "0xb27bc932bf8110d8f78e55da7d5f0497a18b5b82"
DEFAULT_JOURNAL_TAIL_LINES = 50000
DEFAULT_JOURNAL_AUTO_FULL_MAX_BYTES = 100 * 1024 * 1024


@dataclass(frozen=True)
class SleeveConfig:
    name: str
    env_path: Path


DEFAULT_SLEEVES = [
    SleeveConfig("unlawful_baseline", Path("polymarket-exec/env/unlawful_baseline.env")),
    SleeveConfig("unlawful_broad_hours", Path("polymarket-exec/env/unlawful_broad_hours.env")),
    SleeveConfig("unlawful_press", Path("polymarket-exec/env/unlawful_press.env")),
]


def now_ms() -> int:
    return int(datetime.now(tz=timezone.utc).timestamp() * 1000)


def iso_utc(timestamp_ms: int | None) -> str | None:
    if timestamp_ms is None:
        return None
    return datetime.fromtimestamp(timestamp_ms / 1000.0, tz=timezone.utc).isoformat()


def floor_window_start_ms(timestamp_ms: int) -> int:
    return timestamp_ms - (timestamp_ms % WINDOW_MS)


def parse_window_start_ms_from_slug(slug: str | None) -> int | None:
    if not slug:
        return None
    match = re.search(r"(\d{10})$", slug)
    if not match:
        return None
    return int(match.group(1)) * 1000


def safe_float(value: Any) -> float | None:
    if value is None:
        return None
    try:
        return float(value)
    except (TypeError, ValueError):
        return None


def load_env_assignments(path: Path) -> dict[str, str]:
    values: dict[str, str] = {}
    if not path.exists():
        return values
    for raw_line in path.read_text().splitlines():
        line = raw_line.strip()
        if not line or line.startswith("#") or "=" not in line:
            continue
        key, value = line.split("=", 1)
        values[key.strip()] = value.strip()
    return values


def resolve_sleeve_paths(repo_root: Path, sleeve: SleeveConfig) -> dict[str, Path | None]:
    env_values = load_env_assignments(repo_root / sleeve.env_path)
    order_store = env_values.get("PM_BTC_5M_ORDER_STORE_PATH")
    journal_path = env_values.get("PM_BTC_5M_EXEC_JOURNAL_PATH")
    crate_root = repo_root / "polymarket-exec"

    def resolve_runtime_path(raw: str | None) -> Path | None:
        if not raw:
            return None
        candidate = Path(raw)
        if candidate.is_absolute():
            return candidate
        crate_candidate = crate_root / candidate
        repo_candidate = repo_root / candidate

        # Runtime env paths are resolved from `polymarket-exec/` because the
        # sleeve launcher `cd`s into the crate directory before starting the
        # process. Prefer that interpretation even if stale root-level artifacts
        # happen to exist too.
        if candidate.parts and candidate.parts[0] in {"data", "logs"}:
            return crate_candidate
        if crate_candidate.exists():
            return crate_candidate
        return repo_candidate

    return {
        "order_store_path": resolve_runtime_path(order_store),
        "journal_path": resolve_runtime_path(journal_path),
    }


def fetch_activity_rows(wallet: str, limit: int, max_offset: int) -> list[dict[str, Any]]:
    rows: list[dict[str, Any]] = []
    seen_keys: set[tuple[Any, ...]] = set()
    offset = 0
    while offset <= max_offset:
        params = urllib.parse.urlencode({"user": wallet, "limit": limit, "offset": offset})
        request = urllib.request.Request(
            f"{ACTIVITY_API}?{params}",
            headers={
                "User-Agent": "polymarket-agent/1.0",
                "Accept": "application/json",
            },
        )
        try:
            with urllib.request.urlopen(request, timeout=30) as response:
                payload = json.loads(response.read().decode("utf-8"))
        except Exception:
            payload = fetch_json_via_curl(f"{ACTIVITY_API}?{params}")
        if not isinstance(payload, list) or not payload:
            break
        batch_added = 0
        for item in payload:
            if not isinstance(item, dict):
                continue
            dedupe_key = (
                item.get("transactionHash"),
                item.get("timestamp"),
                item.get("type"),
                item.get("conditionId"),
                item.get("asset"),
                item.get("usdcSize"),
                item.get("price"),
            )
            if dedupe_key in seen_keys:
                continue
            seen_keys.add(dedupe_key)
            rows.append(item)
            batch_added += 1
        if len(payload) < limit or batch_added == 0:
            break
        offset += limit
    return rows


def fetch_json_via_curl(url: str) -> Any:
    completed = subprocess.run(
        ["curl", "-sS", url],
        check=True,
        capture_output=True,
        text=True,
    )
    return json.loads(completed.stdout)


def build_whale_windows(rows: list[dict[str, Any]]) -> dict[int, dict[str, Any]]:
    def phase_name(window_start_ms: int, observed_at_ms: int) -> str:
        elapsed_s = max(0, (observed_at_ms - window_start_ms) // 1000)
        if elapsed_s < 30:
            return "early"
        if elapsed_s < 210:
            return "mid"
        return "late"

    def accumulate_buy_stats(store: dict[str, dict[str, Any]], outcome: str, price: float, size: float, usdc_size: float) -> None:
        bucket = store.setdefault(
            outcome,
            {
                "trade_count": 0,
                "size_total": 0.0,
                "usdc_total": 0.0,
                "min_buy_price": None,
                "max_buy_price": None,
            },
        )
        bucket["trade_count"] += 1
        bucket["size_total"] += size
        bucket["usdc_total"] += usdc_size
        bucket["min_buy_price"] = (
            price if bucket["min_buy_price"] is None else min(bucket["min_buy_price"], price)
        )
        bucket["max_buy_price"] = (
            price if bucket["max_buy_price"] is None else max(bucket["max_buy_price"], price)
        )

    def finalize_buy_stats(store: dict[str, dict[str, Any]]) -> dict[str, dict[str, Any]]:
        finalized: dict[str, dict[str, Any]] = {}
        for outcome, stats in store.items():
            size_total = float(stats["size_total"])
            avg_buy_price = stats["usdc_total"] / size_total if size_total > 0 else None
            finalized[outcome] = {
                "trade_count": int(stats["trade_count"]),
                "size_total": round(size_total, 6),
                "usdc_total": round(float(stats["usdc_total"]), 6),
                "avg_buy_price": round(avg_buy_price, 6) if avg_buy_price is not None else None,
                "min_buy_price": round(float(stats["min_buy_price"]), 6)
                if stats["min_buy_price"] is not None
                else None,
                "max_buy_price": round(float(stats["max_buy_price"]), 6)
                if stats["max_buy_price"] is not None
                else None,
            }
        return finalized

    windows: dict[int, dict[str, Any]] = {}
    for row in rows:
        timestamp_s = row.get("timestamp")
        if timestamp_s is None:
            continue
        try:
            observed_at_ms = int(timestamp_s) * 1000
        except (TypeError, ValueError):
            continue
        window_start_ms = parse_window_start_ms_from_slug(str(row.get("slug") or "")) or floor_window_start_ms(observed_at_ms)
        window = windows.setdefault(
            window_start_ms,
            {
                "window_start_ms": window_start_ms,
                "window_start_iso": iso_utc(window_start_ms),
                "slug": row.get("slug"),
                "condition_id": row.get("conditionId"),
                "title": row.get("title"),
                "participated": False,
                "first_timestamp_ms": None,
                "last_timestamp_ms": None,
                "trade_count": 0,
                "merge_count": 0,
                "redeem_count": 0,
                "activity_count": 0,
                "rough_notional_usd": 0.0,
                "outcomes": [],
                "sides": [],
                "_buy_stats_by_outcome": {},
                "_phase_buy_stats": {"early": {}, "mid": {}, "late": {}},
            },
        )
        window["participated"] = True
        window["activity_count"] += 1
        window["slug"] = window["slug"] or row.get("slug")
        window["condition_id"] = window["condition_id"] or row.get("conditionId")
        window["title"] = window["title"] or row.get("title")
        window["first_timestamp_ms"] = (
            observed_at_ms
            if window["first_timestamp_ms"] is None
            else min(window["first_timestamp_ms"], observed_at_ms)
        )
        window["last_timestamp_ms"] = (
            observed_at_ms
            if window["last_timestamp_ms"] is None
            else max(window["last_timestamp_ms"], observed_at_ms)
        )

        activity_type = str(row.get("type") or "").upper()
        if activity_type == "TRADE":
            window["trade_count"] += 1
            usdc_size = safe_float(row.get("usdcSize")) or 0.0
            price = safe_float(row.get("price")) or 0.0
            size = safe_float(row.get("size")) or 0.0
            window["rough_notional_usd"] += usdc_size
            if (
                str(row.get("side") or "").upper() == "BUY"
                and price > 0.0
                and size > 0.0
                and usdc_size > 0.0
            ):
                outcome_name = str(row.get("outcome") or "").strip()
                if outcome_name:
                    accumulate_buy_stats(
                        window["_buy_stats_by_outcome"],
                        outcome_name,
                        price,
                        size,
                        usdc_size,
                    )
                    accumulate_buy_stats(
                        window["_phase_buy_stats"][phase_name(window_start_ms, observed_at_ms)],
                        outcome_name,
                        price,
                        size,
                        usdc_size,
                    )
        elif activity_type == "MERGE":
            window["merge_count"] += 1
        elif activity_type == "REDEEM":
            window["redeem_count"] += 1

        outcome = str(row.get("outcome") or "").strip()
        if outcome and outcome not in window["outcomes"]:
            window["outcomes"].append(outcome)
        side = str(row.get("side") or "").strip()
        if side and side not in window["sides"]:
            window["sides"].append(side)

    for window in windows.values():
        buy_summary_by_outcome = finalize_buy_stats(window.pop("_buy_stats_by_outcome"))
        phase_buy_summary_by_outcome = {
            phase: finalize_buy_stats(stats)
            for phase, stats in window.pop("_phase_buy_stats").items()
        }
        ordered_legs = sorted(
            (
                {
                    "outcome": outcome,
                    **summary,
                }
                for outcome, summary in buy_summary_by_outcome.items()
                if summary.get("avg_buy_price") is not None
            ),
            key=lambda item: item["avg_buy_price"],
        )
        window["buy_summary_by_outcome"] = buy_summary_by_outcome
        window["phase_buy_summary_by_outcome"] = phase_buy_summary_by_outcome
        window["cheap_leg"] = ordered_legs[0] if len(ordered_legs) >= 2 else None
        window["expensive_leg"] = ordered_legs[-1] if len(ordered_legs) >= 2 else None

    return windows


def read_sqlite_order_store(path: Path) -> list[sqlite3.Row]:
    if not path.exists() or path.stat().st_size == 0:
        return []
    conn = sqlite3.connect(str(path))
    conn.row_factory = sqlite3.Row
    try:
        tables = conn.execute(
            "SELECT name FROM sqlite_master WHERE type='table' AND name='orders'"
        ).fetchall()
        if not tables:
            return []
        return conn.execute(
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
                quote_level_tag
            FROM orders
            """
        ).fetchall()
    finally:
        conn.close()


def read_sqlite_signal_snapshots(path: Path) -> list[sqlite3.Row]:
    if not path.exists() or path.stat().st_size == 0:
        return []
    conn = sqlite3.connect(str(path))
    conn.row_factory = sqlite3.Row
    try:
        tables = conn.execute(
            "SELECT name FROM sqlite_master WHERE type='table' AND name='signal_snapshots'"
        ).fetchall()
        if not tables:
            return []
        columns = {
            row["name"]
            for row in conn.execute("PRAGMA table_info(signal_snapshots)").fetchall()
        }

        def select_expr(column: str) -> str:
            if column in columns:
                return column
            return f"NULL AS {column}"

        return conn.execute(
            f"""
            SELECT
                run_id,
                market_id,
                observed_at_ms,
                session_bucket,
                mode,
                aggression_tier,
                cheap_instrument_id,
                expensive_instrument_id,
                cheap_bid,
                cheap_ask,
                expensive_bid,
                expensive_ask,
                price_gap,
                books_fresh,
                both_sides_present,
                {select_expr("cheap_spread")},
                {select_expr("expensive_spread")},
                {select_expr("cheap_bid_depth_top3_qty")},
                {select_expr("cheap_ask_depth_top3_qty")},
                {select_expr("expensive_bid_depth_top3_qty")},
                {select_expr("expensive_ask_depth_top3_qty")},
                {select_expr("cheap_bid_notional_top3")},
                {select_expr("cheap_ask_notional_top3")},
                {select_expr("expensive_bid_notional_top3")},
                {select_expr("expensive_ask_notional_top3")},
                {select_expr("cheap_depth_imbalance_top3")},
                {select_expr("expensive_depth_imbalance_top3")},
                btc_last_price,
                btc_realized_vol_5m_bps,
                btc_realized_vol_15m_bps,
                btc_trade_count_5m,
                btc_trade_count_15m,
                btc_return_30s_bps,
                btc_return_60s_bps,
                btc_observed_at_ms,
                activity_10s,
                activity_30s,
                activity_60s,
                activity_age_ms,
                first_fill_ms,
                first_merge_ms,
                elapsed_s,
                time_remaining_s,
                clip_scale,
                gate_reasons
            FROM signal_snapshots
            ORDER BY observed_at_ms
            """
        ).fetchall()
    finally:
        conn.close()


def is_cancel_status(status: str) -> bool:
    return "cancel" in status.lower()


def summarize_order_rows(rows: list[sqlite3.Row]) -> dict[int, dict[str, Any]]:
    windows: dict[int, dict[str, Any]] = {}
    for row in rows:
        submitted_at_ms = int(row["submitted_at_ms"])
        window_start_ms = floor_window_start_ms(submitted_at_ms)
        window = windows.setdefault(
            window_start_ms,
            {
                "order_count": 0,
                "any_orders_submitted": False,
                "any_fills": False,
                "submitted_notional_usd": 0.0,
                "filled_notional_usd": 0.0,
                "submitted_qty": 0.0,
                "filled_qty": 0.0,
                "filled_order_count": 0,
                "canceled_order_count": 0,
                "statuses": Counter(),
                "market_ids": set(),
                "instrument_ids": set(),
                "reasons": Counter(),
                "first_submitted_at_ms": None,
                "last_submitted_at_ms": None,
            },
        )
        limit_price = safe_float(row["limit_price"]) or 0.0
        original_qty = safe_float(row["original_qty"]) or 0.0
        filled_qty = safe_float(row["filled_qty"]) or 0.0
        status = str(row["status"] or "")
        reason = str(row["reason"] or "").strip()
        window["order_count"] += 1
        window["any_orders_submitted"] = True
        window["any_fills"] = window["any_fills"] or filled_qty > 0
        window["submitted_notional_usd"] += limit_price * original_qty
        window["filled_notional_usd"] += limit_price * filled_qty
        window["submitted_qty"] += original_qty
        window["filled_qty"] += filled_qty
        if filled_qty > 0:
            window["filled_order_count"] += 1
        if is_cancel_status(status):
            window["canceled_order_count"] += 1
        window["statuses"][status] += 1
        if reason:
            window["reasons"][reason] += 1
        market_id = str(row["market_id"] or "").strip()
        if market_id:
            window["market_ids"].add(market_id)
        instrument_id = str(row["instrument_id"] or "").strip()
        if instrument_id:
            window["instrument_ids"].add(instrument_id)
        window["first_submitted_at_ms"] = (
            submitted_at_ms
            if window["first_submitted_at_ms"] is None
            else min(window["first_submitted_at_ms"], submitted_at_ms)
        )
        window["last_submitted_at_ms"] = (
            submitted_at_ms
            if window["last_submitted_at_ms"] is None
            else max(window["last_submitted_at_ms"], submitted_at_ms)
        )

    return windows


def split_gate_reasons(raw: str) -> list[str]:
    text = raw.strip()
    if not text:
        return []
    try:
        decoded = json.loads(text)
        if isinstance(decoded, list):
            return [str(item).strip() for item in decoded if str(item).strip()]
    except json.JSONDecodeError:
        pass
    return [part.strip() for part in re.split(r"[|;,]", text) if part.strip()]


def summarize_signal_snapshot_rows(rows: list[sqlite3.Row]) -> dict[int, dict[str, Any]]:
    windows: dict[int, dict[str, Any]] = {}
    for row in rows:
        observed_at_ms = int(row["observed_at_ms"])
        window_start_ms = floor_window_start_ms(observed_at_ms)
        window = windows.setdefault(
            window_start_ms,
            {
                "signal_snapshot_count": 0,
                "first_signal_at_ms": None,
                "last_signal_at_ms": None,
                "run_ids": set(),
                "market_ids": set(),
                "session_buckets": Counter(),
                "modes": Counter(),
                "aggression_tiers": Counter(),
                "gate_reasons": Counter(),
                "avg_clip_scale_sum": 0.0,
                "avg_clip_scale_count": 0,
                "max_clip_scale": None,
                "latest": None,
            },
        )
        window["signal_snapshot_count"] += 1
        window["first_signal_at_ms"] = (
            observed_at_ms
            if window["first_signal_at_ms"] is None
            else min(window["first_signal_at_ms"], observed_at_ms)
        )
        window["last_signal_at_ms"] = (
            observed_at_ms
            if window["last_signal_at_ms"] is None
            else max(window["last_signal_at_ms"], observed_at_ms)
        )
        run_id = str(row["run_id"] or "").strip()
        if run_id:
            window["run_ids"].add(run_id)
        market_id = str(row["market_id"] or "").strip()
        if market_id:
            window["market_ids"].add(market_id)
        session_bucket = str(row["session_bucket"] or "").strip()
        if session_bucket:
            window["session_buckets"][session_bucket] += 1
        mode = str(row["mode"] or "").strip()
        if mode:
            window["modes"][mode] += 1
        aggression_tier = str(row["aggression_tier"] or "").strip()
        if aggression_tier:
            window["aggression_tiers"][aggression_tier] += 1
        for reason in split_gate_reasons(str(row["gate_reasons"] or "")):
            window["gate_reasons"][reason] += 1

        clip_scale = safe_float(row["clip_scale"])
        if clip_scale is not None:
            window["avg_clip_scale_sum"] += clip_scale
            window["avg_clip_scale_count"] += 1
            window["max_clip_scale"] = (
                clip_scale
                if window["max_clip_scale"] is None
                else max(window["max_clip_scale"], clip_scale)
            )

        latest = window["latest"]
        if latest is None or observed_at_ms >= latest["observed_at_ms"]:
            window["latest"] = {
                "observed_at_ms": observed_at_ms,
                "session_bucket": session_bucket or None,
                "mode": mode or None,
                "aggression_tier": aggression_tier or None,
                "price_gap": safe_float(row["price_gap"]),
                "cheap_ask": safe_float(row["cheap_ask"]),
                "expensive_ask": safe_float(row["expensive_ask"]),
                "cheap_spread": safe_float(row["cheap_spread"]),
                "expensive_spread": safe_float(row["expensive_spread"]),
                "cheap_bid_depth_top3_qty": safe_float(row["cheap_bid_depth_top3_qty"]),
                "cheap_ask_depth_top3_qty": safe_float(row["cheap_ask_depth_top3_qty"]),
                "expensive_bid_depth_top3_qty": safe_float(row["expensive_bid_depth_top3_qty"]),
                "expensive_ask_depth_top3_qty": safe_float(row["expensive_ask_depth_top3_qty"]),
                "cheap_bid_notional_top3": safe_float(row["cheap_bid_notional_top3"]),
                "cheap_ask_notional_top3": safe_float(row["cheap_ask_notional_top3"]),
                "expensive_bid_notional_top3": safe_float(row["expensive_bid_notional_top3"]),
                "expensive_ask_notional_top3": safe_float(row["expensive_ask_notional_top3"]),
                "cheap_depth_imbalance_top3": safe_float(row["cheap_depth_imbalance_top3"]),
                "expensive_depth_imbalance_top3": safe_float(row["expensive_depth_imbalance_top3"]),
                "books_fresh": bool(int(row["books_fresh"])),
                "both_sides_present": bool(int(row["both_sides_present"])),
                "btc_last_price": safe_float(row["btc_last_price"]),
                "btc_realized_vol_5m_bps": safe_float(row["btc_realized_vol_5m_bps"]),
                "btc_realized_vol_15m_bps": safe_float(row["btc_realized_vol_15m_bps"]),
                "btc_trade_count_5m": int(row["btc_trade_count_5m"]),
                "btc_trade_count_15m": int(row["btc_trade_count_15m"]),
                "btc_return_30s_bps": safe_float(row["btc_return_30s_bps"]),
                "btc_return_60s_bps": safe_float(row["btc_return_60s_bps"]),
                "activity_10s": int(row["activity_10s"]),
                "activity_30s": int(row["activity_30s"]),
                "activity_60s": int(row["activity_60s"]),
                "activity_age_ms": int(row["activity_age_ms"]) if row["activity_age_ms"] is not None else None,
                "first_fill_ms": int(row["first_fill_ms"]) if row["first_fill_ms"] is not None else None,
                "first_merge_ms": int(row["first_merge_ms"]) if row["first_merge_ms"] is not None else None,
                "elapsed_s": int(row["elapsed_s"]) if row["elapsed_s"] is not None else None,
                "time_remaining_s": int(row["time_remaining_s"]) if row["time_remaining_s"] is not None else None,
                "clip_scale": clip_scale,
            }

    summarized: dict[int, dict[str, Any]] = {}
    for window_start_ms, window in windows.items():
        avg_clip_scale = None
        if window["avg_clip_scale_count"]:
            avg_clip_scale = window["avg_clip_scale_sum"] / window["avg_clip_scale_count"]
        summarized[window_start_ms] = {
            "signal_snapshot_count": window["signal_snapshot_count"],
            "first_signal_at_ms": window["first_signal_at_ms"],
            "last_signal_at_ms": window["last_signal_at_ms"],
            "run_ids": sorted(window["run_ids"]),
            "market_ids": sorted(window["market_ids"]),
            "session_bucket_summary": summarize_counter(window["session_buckets"]),
            "mode_summary": summarize_counter(window["modes"]),
            "aggression_tier_summary": summarize_counter(window["aggression_tiers"]),
            "gate_reason_summary": summarize_counter(window["gate_reasons"]),
            "avg_clip_scale": round(avg_clip_scale, 6) if avg_clip_scale is not None else None,
            "max_clip_scale": round(window["max_clip_scale"], 6) if window["max_clip_scale"] is not None else None,
            "latest_signal": window["latest"],
        }
    return summarized


def iter_journal_raw_lines(
    path: Path,
    mode: str,
    tail_lines: int,
) -> tuple[str, list[str]]:
    if not path.exists() or path.stat().st_size == 0:
        return "missing", []
    if mode == "skip":
        return "skip", []
    effective_mode = mode
    if mode == "auto" and path.stat().st_size > DEFAULT_JOURNAL_AUTO_FULL_MAX_BYTES:
        effective_mode = "tail"
    if effective_mode == "tail":
        completed = subprocess.run(
            ["tail", "-n", str(tail_lines), str(path)],
            check=True,
            capture_output=True,
            text=True,
        )
        return f"tail:{tail_lines}", completed.stdout.splitlines()
    return "full", path.read_text().splitlines()


def parse_runtime_command(line: dict[str, Any]) -> dict[str, Any] | None:
    command = line.get("command")
    if not isinstance(command, dict) or len(command) != 1:
        return None
    command_type, payload = next(iter(command.items()))
    if command_type != "Submit" or not isinstance(payload, dict):
        return None
    created_at_ms = payload.get("created_at_ms")
    try:
        created_at_ms = int(created_at_ms)
    except (TypeError, ValueError):
        return None
    quantity = safe_float(payload.get("quantity")) or 0.0
    limit_price = safe_float(payload.get("limit_price")) or 0.0
    return {
        "created_at_ms": created_at_ms,
        "market_id": payload.get("market_id"),
        "instrument_id": payload.get("instrument_id"),
        "client_order_id": payload.get("client_order_id"),
        "reason": str(payload.get("reason") or "").strip(),
        "quote_level_tag": payload.get("quote_level_tag"),
        "notional_usd": quantity * limit_price,
    }


def extract_reason_from_event(line: dict[str, Any]) -> str | None:
    record = line.get("record")
    if not isinstance(record, dict):
        return None
    metrics = record.get("metrics")
    if isinstance(metrics, dict):
        risk_reason = str(metrics.get("risk_reject_reason") or "").strip()
        if risk_reason:
            return risk_reason
    message = str(record.get("message") or "").strip()
    if not message:
        return None
    marker = "unlawful gate reason: "
    if marker in message:
        return message.split(marker, 1)[1].strip()
    lowered = message.lower()
    if "suppressed" in lowered or "rejected" in lowered or "window closed" in lowered:
        return message
    return None


def summarize_journal(
    path: Path,
    mode: str,
    tail_lines: int,
    start_ms: int | None = None,
    end_ms: int | None = None,
) -> tuple[dict[int, dict[str, Any]], str]:
    windows: dict[int, dict[str, Any]] = {}
    source_mode, raw_lines = iter_journal_raw_lines(path, mode=mode, tail_lines=tail_lines)
    for raw_line in raw_lines:
        raw_line = raw_line.strip()
        if not raw_line:
            continue
        try:
            line = json.loads(raw_line)
        except json.JSONDecodeError:
            continue
        kind = line.get("kind")
        if kind == "runtime_command":
            command = parse_runtime_command(line)
            if command is None:
                continue
            window_start_ms = floor_window_start_ms(command["created_at_ms"])
            if start_ms is not None and window_start_ms < start_ms:
                continue
            if end_ms is not None and window_start_ms > end_ms:
                continue
            window = windows.setdefault(
                window_start_ms,
                {
                    "command_count": 0,
                    "submit_count": 0,
                    "command_notional_usd": 0.0,
                    "market_ids": set(),
                    "instrument_ids": set(),
                    "command_reasons": Counter(),
                    "suppression_reasons": Counter(),
                    "first_command_at_ms": None,
                    "last_command_at_ms": None,
                },
            )
            window["command_count"] += 1
            window["submit_count"] += 1
            window["command_notional_usd"] += command["notional_usd"]
            if command["market_id"]:
                window["market_ids"].add(str(command["market_id"]))
            if command["instrument_id"]:
                window["instrument_ids"].add(str(command["instrument_id"]))
            if command["reason"]:
                window["command_reasons"][command["reason"]] += 1
            created_at_ms = command["created_at_ms"]
            window["first_command_at_ms"] = (
                created_at_ms
                if window["first_command_at_ms"] is None
                else min(window["first_command_at_ms"], created_at_ms)
            )
            window["last_command_at_ms"] = (
                created_at_ms
                if window["last_command_at_ms"] is None
                else max(window["last_command_at_ms"], created_at_ms)
            )
        elif kind == "runtime_event":
            record = line.get("record")
            if not isinstance(record, dict):
                continue
            observed_at_ms = record.get("observed_at_ms")
            try:
                observed_at_ms = int(observed_at_ms)
            except (TypeError, ValueError):
                continue
            window_start_ms = floor_window_start_ms(observed_at_ms)
            if start_ms is not None and window_start_ms < start_ms:
                continue
            if end_ms is not None and window_start_ms > end_ms:
                continue
            window = windows.setdefault(
                window_start_ms,
                {
                    "command_count": 0,
                    "submit_count": 0,
                    "command_notional_usd": 0.0,
                    "market_ids": set(),
                    "instrument_ids": set(),
                    "command_reasons": Counter(),
                    "suppression_reasons": Counter(),
                    "first_command_at_ms": None,
                    "last_command_at_ms": None,
                },
            )
            market_id = str(record.get("market_id") or "").strip()
            instrument_id = str(record.get("instrument_id") or "").strip()
            if market_id:
                window["market_ids"].add(market_id)
            if instrument_id:
                window["instrument_ids"].add(instrument_id)
            reason = extract_reason_from_event(line)
            if reason:
                window["suppression_reasons"][reason] += 1

    return windows, source_mode


def merge_local_window_summaries(
    order_windows: dict[int, dict[str, Any]],
    journal_windows: dict[int, dict[str, Any]],
    signal_windows: dict[int, dict[str, Any]],
) -> dict[int, dict[str, Any]]:
    merged: dict[int, dict[str, Any]] = {}
    for window_start_ms in set(order_windows) | set(journal_windows) | set(signal_windows):
        order_summary = order_windows.get(window_start_ms, {})
        journal_summary = journal_windows.get(window_start_ms, {})
        signal_summary = signal_windows.get(window_start_ms, {})

        market_ids = set(order_summary.get("market_ids", set())) | set(
            journal_summary.get("market_ids", set())
        ) | set(signal_summary.get("market_ids", set()))
        run_ids = set(signal_summary.get("run_ids", []))
        if "run_id" in order_summary:
            run_ids.add(str(order_summary["run_id"]))
        instrument_ids = set(order_summary.get("instrument_ids", set())) | set(
            journal_summary.get("instrument_ids", set())
        )

        suppression_reasons = Counter()
        suppression_reasons.update(journal_summary.get("suppression_reasons", Counter()))
        rejection_reasons = Counter()
        rejection_reasons.update(order_summary.get("reasons", Counter()))

        statuses = Counter()
        statuses.update(order_summary.get("statuses", Counter()))

        summary = {
            "window_start_ms": window_start_ms,
            "window_start_iso": iso_utc(window_start_ms),
            "any_orders_submitted": bool(order_summary.get("any_orders_submitted")) or bool(journal_summary.get("submit_count")),
            "any_fills": bool(order_summary.get("any_fills")),
            "order_count": int(order_summary.get("order_count", 0)),
            "submit_count": int(journal_summary.get("submit_count", 0)),
            "submitted_notional_usd": round(
                max(
                    float(order_summary.get("submitted_notional_usd", 0.0)),
                    float(journal_summary.get("command_notional_usd", 0.0)),
                ),
                6,
            ),
            "filled_notional_usd": round(float(order_summary.get("filled_notional_usd", 0.0)), 6),
            "submitted_qty": round(float(order_summary.get("submitted_qty", 0.0)), 6),
            "filled_qty": round(float(order_summary.get("filled_qty", 0.0)), 6),
            "filled_order_count": int(order_summary.get("filled_order_count", 0)),
            "canceled_order_count": int(order_summary.get("canceled_order_count", 0)),
            "market_ids": sorted(market_ids),
            "instrument_ids": sorted(instrument_ids),
            "status_counts": dict(sorted(statuses.items())),
            "rejection_reason_summary": summarize_counter(rejection_reasons),
            "suppression_reason_summary": summarize_counter(suppression_reasons),
            "signal_snapshot_count": int(signal_summary.get("signal_snapshot_count", 0)),
            "signal_run_ids": sorted(run_ids),
            "signal_session_bucket_summary": signal_summary.get("session_bucket_summary", []),
            "signal_mode_summary": signal_summary.get("mode_summary", []),
            "signal_aggression_tier_summary": signal_summary.get("aggression_tier_summary", []),
            "signal_gate_reason_summary": signal_summary.get("gate_reason_summary", []),
            "signal_avg_clip_scale": signal_summary.get("avg_clip_scale"),
            "signal_max_clip_scale": signal_summary.get("max_clip_scale"),
            "latest_signal": signal_summary.get("latest_signal"),
            "first_submitted_at_ms": choose_min(
                order_summary.get("first_submitted_at_ms"),
                journal_summary.get("first_command_at_ms"),
            ),
            "last_submitted_at_ms": choose_max(
                order_summary.get("last_submitted_at_ms"),
                journal_summary.get("last_command_at_ms"),
            ),
        }
        merged[window_start_ms] = summary
    return merged


def summarize_counter(counter: Counter[str], limit: int = 6) -> list[dict[str, Any]]:
    return [
        {"reason": reason, "count": count}
        for reason, count in counter.most_common(limit)
        if reason
    ]


def choose_min(left: Any, right: Any) -> int | None:
    values = [value for value in (left, right) if value is not None]
    if not values:
        return None
    return int(min(values))


def choose_max(left: Any, right: Any) -> int | None:
    values = [value for value in (left, right) if value is not None]
    if not values:
        return None
    return int(max(values))


def bucket_for_window(whale_participated: bool, sleeve_participated: bool) -> str:
    if whale_participated and sleeve_participated:
        return "both"
    if whale_participated and not sleeve_participated:
        return "whale_only"
    if not whale_participated and sleeve_participated:
        return "us_only"
    return "neither"


def infer_window_range(
    whale_windows: dict[int, dict[str, Any]],
    sleeve_windows: dict[str, dict[int, dict[str, Any]]],
    start_ms: int | None,
    end_ms: int | None,
) -> tuple[int, int]:
    if start_ms is not None and end_ms is not None:
        return floor_window_start_ms(start_ms), floor_window_start_ms(end_ms)
    observed: list[int] = []
    observed.extend(whale_windows.keys())
    for windows in sleeve_windows.values():
        observed.extend(windows.keys())
    if not observed:
        current = floor_window_start_ms(now_ms())
        return current, current
    inferred_start = min(observed)
    inferred_end = max(observed)
    if start_ms is not None:
        inferred_start = floor_window_start_ms(start_ms)
    if end_ms is not None:
        inferred_end = floor_window_start_ms(end_ms)
    return inferred_start, inferred_end


def build_comparison_windows(
    whale_windows: dict[int, dict[str, Any]],
    sleeve_windows: dict[str, dict[int, dict[str, Any]]],
    start_ms: int | None,
    end_ms: int | None,
) -> list[dict[str, Any]]:
    range_start_ms, range_end_ms = infer_window_range(whale_windows, sleeve_windows, start_ms, end_ms)
    windows: list[dict[str, Any]] = []
    current = range_start_ms
    while current <= range_end_ms:
        whale = whale_windows.get(
            current,
            {
                "window_start_ms": current,
                "window_start_iso": iso_utc(current),
                "slug": None,
                "condition_id": None,
                "title": None,
                "participated": False,
                "first_timestamp_ms": None,
                "last_timestamp_ms": None,
                "trade_count": 0,
                "merge_count": 0,
                "redeem_count": 0,
                "activity_count": 0,
                "rough_notional_usd": 0.0,
                "outcomes": [],
                "sides": [],
                "buy_summary_by_outcome": {},
                "phase_buy_summary_by_outcome": {"early": {}, "mid": {}, "late": {}},
                "cheap_leg": None,
                "expensive_leg": None,
            },
        )
        per_sleeve: dict[str, Any] = {}
        bucket_counts = Counter()
        any_us_participated = False
        for sleeve_name, summaries in sleeve_windows.items():
            summary = summaries.get(
                current,
                {
                    "window_start_ms": current,
                    "window_start_iso": iso_utc(current),
                    "any_orders_submitted": False,
                    "any_fills": False,
                    "order_count": 0,
                    "submit_count": 0,
                    "submitted_notional_usd": 0.0,
                    "filled_notional_usd": 0.0,
                    "submitted_qty": 0.0,
                    "filled_qty": 0.0,
                    "filled_order_count": 0,
                    "canceled_order_count": 0,
                    "market_ids": [],
                    "instrument_ids": [],
                    "status_counts": {},
                    "rejection_reason_summary": [],
                    "suppression_reason_summary": [],
                    "signal_snapshot_count": 0,
                    "signal_run_ids": [],
                    "signal_session_bucket_summary": [],
                    "signal_mode_summary": [],
                    "signal_aggression_tier_summary": [],
                    "signal_gate_reason_summary": [],
                    "signal_avg_clip_scale": None,
                    "signal_max_clip_scale": None,
                    "latest_signal": None,
                    "first_submitted_at_ms": None,
                    "last_submitted_at_ms": None,
                },
            )
            participated = bool(summary["any_orders_submitted"])
            any_us_participated = any_us_participated or participated
            bucket = bucket_for_window(bool(whale["participated"]), participated)
            bucket_counts[bucket] += 1
            per_sleeve[sleeve_name] = {
                **summary,
                "bucket": bucket,
            }
        overall_bucket = bucket_for_window(bool(whale["participated"]), any_us_participated)
        windows.append(
            {
                "window_start_ms": current,
                "window_start_iso": iso_utc(current),
                "market_slug": whale["slug"],
                "condition_id": whale["condition_id"],
                "title": whale["title"],
                "whale": {
                    **whale,
                    "first_timestamp_iso": iso_utc(whale.get("first_timestamp_ms")),
                    "last_timestamp_iso": iso_utc(whale.get("last_timestamp_ms")),
                    "rough_notional_usd": round(float(whale.get("rough_notional_usd", 0.0)), 6),
                },
                "sleeves": per_sleeve,
                "bucket_counts": dict(sorted(bucket_counts.items())),
                "overall_bucket": overall_bucket,
            }
        )
        current += WINDOW_MS
    return windows


def round_optional(value: float | None, digits: int = 6) -> float | None:
    return round(value, digits) if value is not None else None


def average(values: list[float]) -> float | None:
    if not values:
        return None
    return sum(values) / len(values)


def summarize_buy_levels(windows: list[dict[str, Any]]) -> dict[str, Any]:
    by_outcome: dict[str, dict[str, float | int | None]] = {}
    by_role = {
        "cheap_leg": {"size_total": 0.0, "usdc_total": 0.0, "window_count": 0},
        "expensive_leg": {"size_total": 0.0, "usdc_total": 0.0, "window_count": 0},
    }
    phase_by_outcome: dict[str, dict[str, dict[str, float | int | None]]] = {}

    for window in windows:
        whale = window["whale"]
        for outcome, stats in whale.get("buy_summary_by_outcome", {}).items():
            bucket = by_outcome.setdefault(
                outcome,
                {"trade_count": 0, "size_total": 0.0, "usdc_total": 0.0},
            )
            bucket["trade_count"] = int(bucket["trade_count"] or 0) + int(stats.get("trade_count", 0))
            bucket["size_total"] = float(bucket["size_total"] or 0.0) + float(stats.get("size_total", 0.0))
            bucket["usdc_total"] = float(bucket["usdc_total"] or 0.0) + float(stats.get("usdc_total", 0.0))

        for phase, phase_stats in whale.get("phase_buy_summary_by_outcome", {}).items():
            phase_bucket = phase_by_outcome.setdefault(phase, {})
            for outcome, stats in phase_stats.items():
                outcome_bucket = phase_bucket.setdefault(
                    outcome,
                    {"trade_count": 0, "size_total": 0.0, "usdc_total": 0.0},
                )
                outcome_bucket["trade_count"] = int(outcome_bucket["trade_count"] or 0) + int(stats.get("trade_count", 0))
                outcome_bucket["size_total"] = float(outcome_bucket["size_total"] or 0.0) + float(stats.get("size_total", 0.0))
                outcome_bucket["usdc_total"] = float(outcome_bucket["usdc_total"] or 0.0) + float(stats.get("usdc_total", 0.0))

        for role in ("cheap_leg", "expensive_leg"):
            leg = whale.get(role)
            if not isinstance(leg, dict):
                continue
            size_total = float(leg.get("size_total", 0.0))
            usdc_total = float(leg.get("usdc_total", 0.0))
            if size_total <= 0:
                continue
            by_role[role]["size_total"] += size_total
            by_role[role]["usdc_total"] += usdc_total
            by_role[role]["window_count"] += 1

    def finalize(store: dict[str, dict[str, float | int | None]]) -> dict[str, dict[str, Any]]:
        finalized: dict[str, dict[str, Any]] = {}
        for key, stats in store.items():
            size_total = float(stats["size_total"] or 0.0)
            usdc_total = float(stats["usdc_total"] or 0.0)
            avg_buy_price = usdc_total / size_total if size_total > 0 else None
            finalized[key] = {
                **({"trade_count": int(stats["trade_count"] or 0)} if "trade_count" in stats else {}),
                **({"window_count": int(stats["window_count"] or 0)} if "window_count" in stats else {}),
                "size_total": round(size_total, 6),
                "usdc_total": round(usdc_total, 6),
                "avg_buy_price": round_optional(avg_buy_price),
            }
        return finalized

    return {
        "by_outcome": finalize(by_outcome),
        "by_role": finalize(by_role),
        "by_phase_and_outcome": {
            phase: finalize(outcome_stats)
            for phase, outcome_stats in sorted(phase_by_outcome.items())
        },
    }


def summarize_microstructure_context(window: dict[str, Any], sleeve_names: list[str]) -> dict[str, Any]:
    numeric_fields = {
        "price_gap": [],
        "cheap_spread": [],
        "expensive_spread": [],
        "cheap_bid_depth_top3_qty": [],
        "cheap_ask_depth_top3_qty": [],
        "expensive_bid_depth_top3_qty": [],
        "expensive_ask_depth_top3_qty": [],
        "btc_realized_vol_5m_bps": [],
        "btc_realized_vol_15m_bps": [],
        "btc_trade_count_5m": [],
        "btc_trade_count_15m": [],
        "btc_return_30s_bps": [],
        "btc_return_60s_bps": [],
        "activity_30s": [],
        "activity_60s": [],
        "clip_scale": [],
    }
    mode_counts: Counter[str] = Counter()
    aggression_counts: Counter[str] = Counter()
    gate_reasons: Counter[str] = Counter()
    signal_snapshot_count = 0
    books_fresh_count = 0
    both_sides_present_count = 0

    for sleeve_name in sleeve_names:
        sleeve = window["sleeves"][sleeve_name]
        signal_snapshot_count += int(sleeve.get("signal_snapshot_count", 0))
        latest = sleeve.get("latest_signal")
        if not isinstance(latest, dict):
            continue
        if latest.get("mode"):
            mode_counts[str(latest["mode"])] += 1
        if latest.get("aggression_tier"):
            aggression_counts[str(latest["aggression_tier"])] += 1
        if latest.get("books_fresh") is True:
            books_fresh_count += 1
        if latest.get("both_sides_present") is True:
            both_sides_present_count += 1
        for field, values in numeric_fields.items():
            value = safe_float(latest.get(field))
            if value is not None:
                values.append(value)
        for item in sleeve.get("signal_gate_reason_summary", []):
            reason = str(item.get("reason") or "").strip()
            if reason:
                gate_reasons[reason] += int(item.get("count", 0))

    return {
        "available": any(numeric_fields[field] for field in numeric_fields) or signal_snapshot_count > 0,
        "signal_snapshot_count": signal_snapshot_count,
        "mode_summary": summarize_counter(mode_counts),
        "aggression_tier_summary": summarize_counter(aggression_counts),
        "books_fresh_sleeve_count": books_fresh_count,
        "both_sides_present_sleeve_count": both_sides_present_count,
        "avg_price_gap": round_optional(average(numeric_fields["price_gap"])),
        "avg_cheap_spread": round_optional(average(numeric_fields["cheap_spread"])),
        "avg_expensive_spread": round_optional(average(numeric_fields["expensive_spread"])),
        "avg_cheap_bid_depth_top3_qty": round_optional(average(numeric_fields["cheap_bid_depth_top3_qty"])),
        "avg_cheap_ask_depth_top3_qty": round_optional(average(numeric_fields["cheap_ask_depth_top3_qty"])),
        "avg_expensive_bid_depth_top3_qty": round_optional(average(numeric_fields["expensive_bid_depth_top3_qty"])),
        "avg_expensive_ask_depth_top3_qty": round_optional(average(numeric_fields["expensive_ask_depth_top3_qty"])),
        "avg_btc_realized_vol_5m_bps": round_optional(average(numeric_fields["btc_realized_vol_5m_bps"])),
        "avg_btc_realized_vol_15m_bps": round_optional(average(numeric_fields["btc_realized_vol_15m_bps"])),
        "avg_btc_trade_count_5m": round_optional(average(numeric_fields["btc_trade_count_5m"])),
        "avg_btc_trade_count_15m": round_optional(average(numeric_fields["btc_trade_count_15m"])),
        "avg_btc_return_30s_bps": round_optional(average(numeric_fields["btc_return_30s_bps"])),
        "avg_btc_return_60s_bps": round_optional(average(numeric_fields["btc_return_60s_bps"])),
        "avg_activity_30s": round_optional(average(numeric_fields["activity_30s"])),
        "avg_activity_60s": round_optional(average(numeric_fields["activity_60s"])),
        "avg_clip_scale": round_optional(average(numeric_fields["clip_scale"])),
        "gate_reason_summary": summarize_counter(gate_reasons),
    }


def summarize_local_sleeve_metrics(windows: list[dict[str, Any]], sleeve_name: str) -> dict[str, Any]:
    bucket_counts: Counter[str] = Counter()
    statuses: Counter[str] = Counter()
    rejection_reasons: Counter[str] = Counter()
    suppression_reasons: Counter[str] = Counter()
    gate_reasons: Counter[str] = Counter()
    price_gaps: list[float] = []
    clip_scales: list[float] = []
    btc_vol_5m: list[float] = []
    totals = {
        "order_count": 0,
        "filled_order_count": 0,
        "canceled_order_count": 0,
        "submitted_notional_usd": 0.0,
        "filled_notional_usd": 0.0,
        "submitted_qty": 0.0,
        "filled_qty": 0.0,
        "signal_snapshot_count": 0,
    }

    for window in windows:
        sleeve = window["sleeves"][sleeve_name]
        bucket_counts[str(sleeve["bucket"])] += 1
        for key in totals:
            totals[key] += float(sleeve.get(key, 0.0))
        statuses.update(sleeve.get("status_counts", {}))
        for item in sleeve.get("rejection_reason_summary", []):
            rejection_reasons[str(item["reason"])] += int(item["count"])
        for item in sleeve.get("suppression_reason_summary", []):
            suppression_reasons[str(item["reason"])] += int(item["count"])
        for item in sleeve.get("signal_gate_reason_summary", []):
            gate_reasons[str(item["reason"])] += int(item["count"])
        latest = sleeve.get("latest_signal")
        if isinstance(latest, dict):
            for field, target in (
                ("price_gap", price_gaps),
                ("clip_scale", clip_scales),
                ("btc_realized_vol_5m_bps", btc_vol_5m),
            ):
                value = safe_float(latest.get(field))
                if value is not None:
                    target.append(value)

    submitted_notional = totals["submitted_notional_usd"]
    order_count = totals["order_count"]
    return {
        "bucket_counts": dict(sorted(bucket_counts.items())),
        "order_count": int(totals["order_count"]),
        "filled_order_count": int(totals["filled_order_count"]),
        "canceled_order_count": int(totals["canceled_order_count"]),
        "submitted_notional_usd": round(totals["submitted_notional_usd"], 6),
        "filled_notional_usd": round(totals["filled_notional_usd"], 6),
        "submitted_qty": round(totals["submitted_qty"], 6),
        "filled_qty": round(totals["filled_qty"], 6),
        "fill_rate_by_notional": round_optional(totals["filled_notional_usd"] / submitted_notional if submitted_notional else None),
        "cancel_rate_by_order_count": round_optional(totals["canceled_order_count"] / order_count if order_count else None),
        "status_counts": dict(sorted(statuses.items())),
        "rejection_reason_summary": summarize_counter(rejection_reasons),
        "suppression_reason_summary": summarize_counter(suppression_reasons),
        "signal_snapshot_count": int(totals["signal_snapshot_count"]),
        "avg_signal_price_gap": round_optional(average(price_gaps)),
        "avg_signal_clip_scale": round_optional(average(clip_scales)),
        "avg_signal_btc_realized_vol_5m_bps": round_optional(average(btc_vol_5m)),
        "signal_gate_reason_summary": summarize_counter(gate_reasons),
        "pnl_metrics": {
            "available": False,
            "realized_pnl_usd": None,
            "unrealized_pnl_usd": None,
            "source": "not persisted in current order-store.sqlite schema",
        },
    }


def compact_window_reference(window: dict[str, Any], sleeve_names: list[str]) -> dict[str, Any]:
    whale = window["whale"]
    sleeve_metrics = {}
    for sleeve_name in sleeve_names:
        sleeve = window["sleeves"][sleeve_name]
        latest = sleeve.get("latest_signal")
        sleeve_metrics[sleeve_name] = {
            "bucket": sleeve["bucket"],
            "any_orders_submitted": bool(sleeve["any_orders_submitted"]),
            "any_fills": bool(sleeve["any_fills"]),
            "order_count": int(sleeve["order_count"]),
            "filled_order_count": int(sleeve["filled_order_count"]),
            "canceled_order_count": int(sleeve["canceled_order_count"]),
            "submitted_notional_usd": sleeve["submitted_notional_usd"],
            "filled_notional_usd": sleeve["filled_notional_usd"],
            "status_counts": sleeve["status_counts"],
            "latest_signal": {
                "mode": latest.get("mode"),
                "aggression_tier": latest.get("aggression_tier"),
                "price_gap": latest.get("price_gap"),
                "clip_scale": latest.get("clip_scale"),
                "btc_realized_vol_5m_bps": latest.get("btc_realized_vol_5m_bps"),
                "activity_30s": latest.get("activity_30s"),
                "books_fresh": latest.get("books_fresh"),
                "both_sides_present": latest.get("both_sides_present"),
            }
            if isinstance(latest, dict)
            else None,
            "rejection_reason_summary": sleeve["rejection_reason_summary"],
            "suppression_reason_summary": sleeve["suppression_reason_summary"],
            "signal_gate_reason_summary": sleeve["signal_gate_reason_summary"],
        }

    return {
        "window_start_ms": window["window_start_ms"],
        "window_start_iso": window["window_start_iso"],
        "market_slug": window["market_slug"],
        "condition_id": window["condition_id"],
        "overall_bucket": window["overall_bucket"],
        "whale": {
            "participated": bool(whale["participated"]),
            "trade_count": int(whale["trade_count"]),
            "merge_count": int(whale["merge_count"]),
            "redeem_count": int(whale["redeem_count"]),
            "rough_notional_usd": whale["rough_notional_usd"],
            "cheap_leg": whale.get("cheap_leg"),
            "expensive_leg": whale.get("expensive_leg"),
        },
        "microstructure_context": summarize_microstructure_context(window, sleeve_names),
        "sleeves": sleeve_metrics,
    }


def build_qa_summary(windows: list[dict[str, Any]], sleeve_names: list[str]) -> dict[str, Any]:
    overall_bucket_counts = Counter(window["overall_bucket"] for window in windows)
    bucket_windows = {
        bucket: [window for window in windows if window["overall_bucket"] == bucket]
        for bucket in ("whale_only", "us_only", "both", "neither")
    }
    whale_participated_windows = [window for window in windows if window["whale"]["participated"]]

    return {
        "schema_version": 1,
        "window_count": len(windows),
        "overall_bucket_counts": dict(sorted(overall_bucket_counts.items())),
        "whale": {
            "participated_window_count": len(whale_participated_windows),
            "trade_count": sum(int(window["whale"]["trade_count"]) for window in whale_participated_windows),
            "merge_count": sum(int(window["whale"]["merge_count"]) for window in whale_participated_windows),
            "redeem_count": sum(int(window["whale"]["redeem_count"]) for window in whale_participated_windows),
            "rough_notional_usd": round(
                sum(float(window["whale"].get("rough_notional_usd", 0.0)) for window in whale_participated_windows),
                6,
            ),
            "buy_levels": summarize_buy_levels(whale_participated_windows),
        },
        "buckets": {
            bucket: {
                "count": len(bucket_rows),
                "whale_buy_levels": summarize_buy_levels(bucket_rows),
                "windows": [compact_window_reference(window, sleeve_names) for window in bucket_rows],
            }
            for bucket, bucket_rows in bucket_windows.items()
        },
        "sleeves": {
            sleeve_name: summarize_local_sleeve_metrics(windows, sleeve_name)
            for sleeve_name in sleeve_names
        },
        "pnl_note": "Current local order stores expose order/fill/cancel state and signal snapshots, but not realized/unrealized PnL columns.",
    }


def flatten_for_csv(windows: list[dict[str, Any]], sleeve_names: list[str]) -> list[dict[str, Any]]:
    rows: list[dict[str, Any]] = []
    for window in windows:
        row: dict[str, Any] = {
            "window_start_ms": window["window_start_ms"],
            "window_start_iso": window["window_start_iso"],
            "market_slug": window["market_slug"] or "",
            "condition_id": window["condition_id"] or "",
            "overall_bucket": window["overall_bucket"],
            "whale_participated": int(bool(window["whale"]["participated"])),
            "whale_first_timestamp_ms": window["whale"]["first_timestamp_ms"] or "",
            "whale_trade_count": window["whale"]["trade_count"],
            "whale_merge_count": window["whale"]["merge_count"],
            "whale_redeem_count": window["whale"]["redeem_count"],
            "whale_rough_notional_usd": window["whale"]["rough_notional_usd"],
            "whale_cheap_outcome": (
                window["whale"]["cheap_leg"]["outcome"]
                if isinstance(window["whale"].get("cheap_leg"), dict)
                else ""
            ),
            "whale_cheap_avg_buy_price": (
                window["whale"]["cheap_leg"]["avg_buy_price"]
                if isinstance(window["whale"].get("cheap_leg"), dict)
                else ""
            ),
            "whale_expensive_outcome": (
                window["whale"]["expensive_leg"]["outcome"]
                if isinstance(window["whale"].get("expensive_leg"), dict)
                else ""
            ),
            "whale_expensive_avg_buy_price": (
                window["whale"]["expensive_leg"]["avg_buy_price"]
                if isinstance(window["whale"].get("expensive_leg"), dict)
                else ""
            ),
        }
        for sleeve_name in sleeve_names:
            sleeve = window["sleeves"][sleeve_name]
            prefix = f"{sleeve_name}_"
            row[f"{prefix}bucket"] = sleeve["bucket"]
            row[f"{prefix}orders"] = int(bool(sleeve["any_orders_submitted"]))
            row[f"{prefix}fills"] = int(bool(sleeve["any_fills"]))
            row[f"{prefix}order_count"] = sleeve["order_count"]
            row[f"{prefix}submit_count"] = sleeve["submit_count"]
            row[f"{prefix}submitted_notional_usd"] = sleeve["submitted_notional_usd"]
            row[f"{prefix}filled_notional_usd"] = sleeve["filled_notional_usd"]
            row[f"{prefix}submitted_qty"] = sleeve["submitted_qty"]
            row[f"{prefix}filled_qty"] = sleeve["filled_qty"]
            row[f"{prefix}filled_order_count"] = sleeve["filled_order_count"]
            row[f"{prefix}canceled_order_count"] = sleeve["canceled_order_count"]
            row[f"{prefix}status_counts"] = "|".join(
                f"{status}:{count}"
                for status, count in sleeve["status_counts"].items()
            )
            row[f"{prefix}market_ids"] = "|".join(sleeve["market_ids"])
            row[f"{prefix}signal_snapshot_count"] = sleeve["signal_snapshot_count"]
            row[f"{prefix}signal_mode"] = (
                sleeve["latest_signal"]["mode"]
                if isinstance(sleeve["latest_signal"], dict)
                else ""
            )
            row[f"{prefix}signal_aggression_tier"] = (
                sleeve["latest_signal"]["aggression_tier"]
                if isinstance(sleeve["latest_signal"], dict)
                else ""
            )
            row[f"{prefix}signal_session_bucket"] = (
                sleeve["latest_signal"]["session_bucket"]
                if isinstance(sleeve["latest_signal"], dict)
                else ""
            )
            row[f"{prefix}signal_clip_scale"] = (
                sleeve["latest_signal"]["clip_scale"]
                if isinstance(sleeve["latest_signal"], dict)
                else ""
            )
            row[f"{prefix}signal_price_gap"] = (
                sleeve["latest_signal"]["price_gap"]
                if isinstance(sleeve["latest_signal"], dict)
                else ""
            )
            for field in (
                "cheap_spread",
                "expensive_spread",
                "cheap_bid_depth_top3_qty",
                "cheap_ask_depth_top3_qty",
                "expensive_bid_depth_top3_qty",
                "expensive_ask_depth_top3_qty",
                "cheap_bid_notional_top3",
                "cheap_ask_notional_top3",
                "expensive_bid_notional_top3",
                "expensive_ask_notional_top3",
                "cheap_depth_imbalance_top3",
                "expensive_depth_imbalance_top3",
            ):
                row[f"{prefix}signal_{field}"] = (
                    sleeve["latest_signal"][field]
                    if isinstance(sleeve["latest_signal"], dict)
                    else ""
                )
            row[f"{prefix}signal_books_fresh"] = (
                int(bool(sleeve["latest_signal"]["books_fresh"]))
                if isinstance(sleeve["latest_signal"], dict)
                and sleeve["latest_signal"].get("books_fresh") is not None
                else ""
            )
            row[f"{prefix}signal_both_sides_present"] = (
                int(bool(sleeve["latest_signal"]["both_sides_present"]))
                if isinstance(sleeve["latest_signal"], dict)
                and sleeve["latest_signal"].get("both_sides_present") is not None
                else ""
            )
            row[f"{prefix}signal_btc_vol_5m_bps"] = (
                sleeve["latest_signal"]["btc_realized_vol_5m_bps"]
                if isinstance(sleeve["latest_signal"], dict)
                else ""
            )
            row[f"{prefix}signal_btc_trade_count_5m"] = (
                sleeve["latest_signal"]["btc_trade_count_5m"]
                if isinstance(sleeve["latest_signal"], dict)
                else ""
            )
            row[f"{prefix}signal_activity_30s"] = (
                sleeve["latest_signal"]["activity_30s"]
                if isinstance(sleeve["latest_signal"], dict)
                else ""
            )
            row[f"{prefix}suppression_reasons"] = "|".join(
                f"{item['reason']}:{item['count']}"
                for item in sleeve["suppression_reason_summary"]
            )
            row[f"{prefix}rejection_reasons"] = "|".join(
                f"{item['reason']}:{item['count']}"
                for item in sleeve["rejection_reason_summary"]
            )
            row[f"{prefix}signal_gate_reasons"] = "|".join(
                f"{item['reason']}:{item['count']}"
                for item in sleeve["signal_gate_reason_summary"]
            )
        rows.append(row)
    return rows


def write_csv(path: Path, rows: list[dict[str, Any]]) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    fieldnames: list[str] = []
    for row in rows:
        for key in row.keys():
            if key not in fieldnames:
                fieldnames.append(key)
    with path.open("w", newline="") as handle:
        writer = csv.DictWriter(handle, fieldnames=fieldnames)
        writer.writeheader()
        for row in rows:
            writer.writerow(row)


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser()
    parser.add_argument(
        "--wallet",
        default=DEFAULT_WHALE_WALLET,
        help="whale wallet to fetch from data-api.polymarket.com/activity",
    )
    parser.add_argument(
        "--activity-limit",
        type=int,
        default=500,
        help="page size for the Polymarket activity API",
    )
    parser.add_argument(
        "--activity-max-offset",
        type=int,
        default=3000,
        help="max offset to fetch from the activity API",
    )
    parser.add_argument(
        "--activity-json",
        type=Path,
        help="optional local activity JSON to use instead of fetching live API data",
    )
    parser.add_argument(
        "--start-ms",
        type=int,
        help="optional inclusive window-range start in epoch ms",
    )
    parser.add_argument(
        "--end-ms",
        type=int,
        help="optional inclusive window-range end in epoch ms",
    )
    parser.add_argument(
        "--out-json",
        type=Path,
        default=Path("reports/unlawful_calibration/unlawful_whale_vs_us.json"),
    )
    parser.add_argument(
        "--out-csv",
        type=Path,
        default=Path("reports/unlawful_calibration/unlawful_whale_vs_us.csv"),
    )
    parser.add_argument(
        "--journal-mode",
        choices=("auto", "full", "tail", "skip"),
        default="auto",
        help="how to read local journals; auto uses tail mode for large live logs",
    )
    parser.add_argument(
        "--journal-tail-lines",
        type=int,
        default=DEFAULT_JOURNAL_TAIL_LINES,
        help="line count for bounded journal tail reads",
    )
    return parser.parse_args()


def main() -> int:
    args = parse_args()
    repo_root = Path(__file__).resolve().parent.parent

    if args.activity_json:
        activity_rows = json.loads(args.activity_json.read_text())
        if not isinstance(activity_rows, list):
            raise SystemExit("activity-json must contain a top-level array")
        activity_source = f"file:{args.activity_json}"
    else:
        activity_rows = fetch_activity_rows(
            wallet=args.wallet,
            limit=args.activity_limit,
            max_offset=args.activity_max_offset,
        )
        activity_source = "data-api.polymarket.com/activity"

    whale_windows = build_whale_windows(activity_rows)
    journal_start_ms = args.start_ms
    journal_end_ms = args.end_ms
    if whale_windows:
        if journal_start_ms is None:
            journal_start_ms = min(whale_windows.keys())
        if journal_end_ms is None:
            journal_end_ms = max(whale_windows.keys())

    sleeve_windows: dict[str, dict[int, dict[str, Any]]] = {}
    sleeve_meta: dict[str, dict[str, Any]] = {}
    for sleeve in DEFAULT_SLEEVES:
        resolved = resolve_sleeve_paths(repo_root, sleeve)
        order_store_path = resolved["order_store_path"]
        journal_path = resolved["journal_path"]
        order_rows = (
            read_sqlite_order_store(order_store_path)
            if order_store_path is not None
            else []
        )
        signal_rows = (
            read_sqlite_signal_snapshots(order_store_path)
            if order_store_path is not None
            else []
        )
        order_windows = summarize_order_rows(order_rows)
        signal_windows = summarize_signal_snapshot_rows(signal_rows)
        if journal_path is not None:
            journal_windows, journal_source_mode = summarize_journal(
                journal_path,
                mode=args.journal_mode,
                tail_lines=args.journal_tail_lines,
                start_ms=journal_start_ms,
                end_ms=journal_end_ms,
            )
        else:
            journal_windows, journal_source_mode = {}, "missing"
        sleeve_windows[sleeve.name] = merge_local_window_summaries(
            order_windows,
            journal_windows,
            signal_windows,
        )
        sleeve_meta[sleeve.name] = {
            "env_path": str((repo_root / sleeve.env_path).resolve()),
            "order_store_path": str(order_store_path.resolve()) if order_store_path else None,
            "journal_path": str(journal_path.resolve()) if journal_path else None,
            "order_rows": len(order_rows),
            "signal_snapshot_rows": len(signal_rows),
            "journal_exists": bool(journal_path and journal_path.exists()),
            "order_store_exists": bool(order_store_path and order_store_path.exists()),
            "current_runtime_artifact_root": str((repo_root / "polymarket-exec/data").resolve()),
            "journal_source_mode": journal_source_mode,
            "journal_window_rows": len(journal_windows),
        }

    comparison_windows = build_comparison_windows(
        whale_windows=whale_windows,
        sleeve_windows=sleeve_windows,
        start_ms=args.start_ms,
        end_ms=args.end_ms,
    )
    sleeve_names = [sleeve.name for sleeve in DEFAULT_SLEEVES]
    csv_rows = flatten_for_csv(comparison_windows, sleeve_names)

    bucket_counts = Counter(window["overall_bucket"] for window in comparison_windows)
    generated_at_ms = now_ms()
    json_payload = {
        "generated_at_ms": generated_at_ms,
        "generated_at_iso": iso_utc(generated_at_ms),
        "whale_wallet": args.wallet,
        "activity_source": activity_source,
        "activity_row_count": len(activity_rows),
        "window_count": len(comparison_windows),
        "bucket_counts": dict(sorted(bucket_counts.items())),
        "qa_summary": build_qa_summary(comparison_windows, sleeve_names),
        "journal_mode": args.journal_mode,
        "journal_tail_lines": args.journal_tail_lines,
        "sleeves": sleeve_meta,
        "windows": comparison_windows,
    }

    args.out_json.parent.mkdir(parents=True, exist_ok=True)
    args.out_json.write_text(json.dumps(json_payload, indent=2) + "\n")
    write_csv(args.out_csv, csv_rows)

    summary = {
        "out_json": str(args.out_json),
        "out_csv": str(args.out_csv),
        "activity_row_count": len(activity_rows),
        "window_count": len(comparison_windows),
        "bucket_counts": dict(sorted(bucket_counts.items())),
    }
    print(json.dumps(summary, indent=2))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
