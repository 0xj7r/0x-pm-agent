#!/usr/bin/env python3
"""Compare shadow-live and tiny-live decision outputs by market/time window.

Inputs may be either:
  - PaperReportSummary JSON files from PM_BTC_5M_PAPER_REPORT_PATH
  - Runtime journal JSONL files from PM_BTC_5M_EXEC_JOURNAL_PATH

The script intentionally compares decision surface, not fills economics. It
answers whether shadow and live were trying to submit/cancel/suppress in the
same markets over the same windows.
"""

from __future__ import annotations

import argparse
import json
from collections import Counter, defaultdict
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any


@dataclass(frozen=True, order=True)
class WindowKey:
    market_id: str
    window_start_ms: int


@dataclass
class WindowStats:
    submits: int = 0
    cancels: int = 0
    fills: int = 0
    rejects: int = 0
    suppressions: int = 0
    events: Counter[str] = field(default_factory=Counter)
    intent_types: Counter[str] = field(default_factory=Counter)
    reasons: Counter[str] = field(default_factory=Counter)


def bucket(ts_ms: int, window_ms: int) -> int:
    return (max(ts_ms, 0) // window_ms) * window_ms


def market_id(value: Any) -> str | None:
    if value is None:
        return None
    if isinstance(value, str):
        return value
    return str(value)


def as_int(value: Any, default: int = 0) -> int:
    try:
        return int(value)
    except (TypeError, ValueError):
        return default


def classify_event(message: str) -> str:
    text = message.lower()
    if "suppressed" in text or "gate" in text:
        return "suppression"
    if "submit" in text:
        return "submit_event"
    if "cancel" in text:
        return "cancel_event"
    if "fill" in text:
        return "fill_event"
    if "risk-off" in text or "riskoff" in text:
        return "risk_off"
    return "other"


def add_report_summary(path: Path, data: dict[str, Any], window_ms: int) -> dict[WindowKey, WindowStats]:
    session = data.get("session", {})
    started_at_ms = as_int(session.get("started_at_ms"))
    rows: dict[WindowKey, WindowStats] = defaultdict(WindowStats)

    for market in data.get("markets", []):
        mid = market_id(market.get("market_id"))
        if not mid:
            continue
        stats = rows[WindowKey(mid, bucket(started_at_ms, window_ms))]
        stats.submits += as_int(market.get("submit_count"))
        stats.cancels += as_int(market.get("cancel_count"))
        stats.fills += as_int(market.get("fill_count"))
        stats.rejects += as_int(market.get("reject_count"))
        stats.events.update(market.get("strategy_events_by_type") or {})
        stats.intent_types.update(market.get("intents_by_type") or {})

    for sample in data.get("suppression_samples", []):
        mid = market_id(sample.get("market_id"))
        if not mid:
            continue
        stats = rows[WindowKey(mid, bucket(as_int(sample.get("observed_at_ms")), window_ms))]
        stats.suppressions += 1
        reason_type = str(sample.get("reason_type") or "suppression")
        stats.events[reason_type] += 1
        reason = str(sample.get("reason") or reason_type)
        stats.reasons[reason] += 1

    if not rows:
        raise ValueError(f"{path} looked like a report summary but contained no comparable rows")
    return dict(rows)


def command_payload(command: Any) -> tuple[str, dict[str, Any]] | None:
    if not isinstance(command, dict) or not command:
        return None
    if "Submit" in command and isinstance(command["Submit"], dict):
        return "Submit", command["Submit"]
    if "Cancel" in command and isinstance(command["Cancel"], dict):
        return "Cancel", command["Cancel"]
    if "market_id" in command:
        return str(command.get("kind") or "Command"), command
    return None


def add_journal(path: Path, window_ms: int) -> dict[WindowKey, WindowStats]:
    rows: dict[WindowKey, WindowStats] = defaultdict(WindowStats)
    with path.open() as fh:
        for line_no, line in enumerate(fh, 1):
            line = line.strip()
            if not line:
                continue
            try:
                item = json.loads(line)
            except json.JSONDecodeError as exc:
                raise ValueError(f"{path}:{line_no}: invalid JSONL: {exc}") from exc

            kind = item.get("kind")
            if kind == "runtime_event":
                record = item.get("record") or {}
                mid = market_id(record.get("market_id"))
                if not mid:
                    continue
                message = str(record.get("message") or "")
                stats = rows[WindowKey(mid, bucket(as_int(record.get("observed_at_ms")), window_ms))]
                event = classify_event(message)
                stats.events[event] += 1
                if event == "suppression":
                    stats.suppressions += 1
                    stats.reasons[message] += 1
                elif event == "fill_event":
                    stats.fills += 1
            elif kind == "runtime_command":
                payload = command_payload(item.get("command"))
                if payload is None:
                    continue
                command_kind, command = payload
                mid = market_id(command.get("market_id"))
                if not mid:
                    continue
                stats = rows[WindowKey(mid, bucket(as_int(command.get("created_at_ms")), window_ms))]
                if command_kind == "Submit":
                    stats.submits += 1
                    stats.intent_types[str(command.get("kind") or command.get("quote_level_tag") or "submit")] += 1
                elif command_kind == "Cancel":
                    stats.cancels += 1
    if not rows:
        raise ValueError(f"{path} contained no comparable journal rows")
    return dict(rows)


def load_rows(path: Path, window_ms: int) -> dict[WindowKey, WindowStats]:
    text = path.read_text().lstrip()
    if not text:
        raise ValueError(f"{path} is empty")
    if text.startswith("{"):
        data = json.loads(text)
        return add_report_summary(path, data, window_ms)
    return add_journal(path, window_ms)


def top(counter: Counter[str], limit: int = 3) -> str:
    return ", ".join(f"{name}:{count}" for name, count in counter.most_common(limit))


def compare(shadow: dict[WindowKey, WindowStats], live: dict[WindowKey, WindowStats]) -> list[dict[str, Any]]:
    rows = []
    for key in sorted(set(shadow) | set(live)):
        s = shadow.get(key, WindowStats())
        l = live.get(key, WindowStats())
        rows.append(
            {
                "market_id": key.market_id,
                "window_start_ms": key.window_start_ms,
                "shadow_submits": s.submits,
                "live_submits": l.submits,
                "submit_delta": l.submits - s.submits,
                "shadow_cancels": s.cancels,
                "live_cancels": l.cancels,
                "shadow_suppressions": s.suppressions,
                "live_suppressions": l.suppressions,
                "suppression_delta": l.suppressions - s.suppressions,
                "shadow_fills": s.fills,
                "live_fills": l.fills,
                "shadow_top_events": top(s.events),
                "live_top_events": top(l.events),
                "shadow_top_reasons": top(s.reasons),
                "live_top_reasons": top(l.reasons),
            }
        )
    return rows


def print_table(rows: list[dict[str, Any]]) -> None:
    headers = [
        "market_id",
        "window_start_ms",
        "shadow_submits",
        "live_submits",
        "submit_delta",
        "shadow_suppressions",
        "live_suppressions",
        "suppression_delta",
        "shadow_fills",
        "live_fills",
    ]
    print("\t".join(headers))
    for row in rows:
        print("\t".join(str(row[h]) for h in headers))


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--shadow", required=True, type=Path, help="shadow-live report JSON or journal JSONL")
    parser.add_argument("--live", required=True, type=Path, help="tiny-live report JSON or journal JSONL")
    parser.add_argument("--window-ms", type=int, default=60_000)
    parser.add_argument("--json", action="store_true", help="emit full JSON comparison rows")
    args = parser.parse_args()
    if args.window_ms <= 0:
        parser.error("--window-ms must be positive")

    shadow = load_rows(args.shadow, args.window_ms)
    live = load_rows(args.live, args.window_ms)
    rows = compare(shadow, live)
    if args.json:
        print(json.dumps(rows, indent=2, sort_keys=True))
    else:
        print_table(rows)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
