#!/usr/bin/env python3
"""Summarize recovered wallet history into phases and market patterns.

Usage:
  python3 scripts/summarize_wallet_history.py \
      --history data/research/wallet_research/8b5b82/history/historical/activity_history.json \
      --output data/research/wallet_research/8b5b82/history/historical/phase_summary.json
"""
from __future__ import annotations

import argparse
import json
import statistics
from collections import Counter, defaultdict
from datetime import UTC, datetime
from pathlib import Path
from typing import Any


def day(ts: int) -> str:
    return datetime.fromtimestamp(int(ts), tz=UTC).strftime("%Y-%m-%d")


def asset(slug: str) -> str:
    slug = (slug or "").lower()
    if slug.startswith("btc-") or slug.startswith("bitcoin-"):
        return "BTC"
    if slug.startswith("eth-") or slug.startswith("ethereum-"):
        return "ETH"
    if slug.startswith("sol-") or slug.startswith("solana-"):
        return "SOL"
    if slug.startswith("xrp-"):
        return "XRP"
    if slug.startswith("bnb-"):
        return "BNB"
    return "OTHER"


def family(slug: str) -> str:
    slug = (slug or "").lower()
    if "updown-5m" in slug:
        return "5m"
    if "updown-15m" in slug:
        return "15m"
    if "updown-1h" in slug:
        return "1h"
    return "other"


def side(row: dict[str, Any]) -> str:
    value = str(row.get("outcome") or row.get("side") or "").lower()
    if "up" in value or value == "yes":
        return "UP"
    if "down" in value or value == "no":
        return "DOWN"
    return "UNK"


def summarize_period(rows: list[dict[str, Any]], start_day: str, end_day: str, label: str) -> dict[str, Any]:
    sub = [r for r in rows if start_day <= day(int(r["timestamp"])) < end_day]
    trades = [r for r in sub if r.get("type") == "TRADE"]
    clips = [float(r.get("size") or 0.0) for r in trades]
    by_asset = Counter(asset(str(r.get("slug") or "")) for r in sub)
    by_family = Counter(family(str(r.get("slug") or "")) for r in sub)
    by_slug: dict[str, dict[str, Any]] = defaultdict(
        lambda: {"up_rows": 0, "down_rows": 0, "merge_rows": 0, "up_sh": 0.0, "down_sh": 0.0, "merge_sh": 0.0}
    )
    for row in sub:
        slug = str(row.get("slug") or "")
        rec = by_slug[slug]
        typ = row.get("type")
        if typ == "TRADE":
            sh = float(row.get("size") or 0.0)
            s = side(row)
            if s == "UP":
                rec["up_rows"] += 1
                rec["up_sh"] += sh
            elif s == "DOWN":
                rec["down_rows"] += 1
                rec["down_sh"] += sh
        elif typ == "MERGE":
            rec["merge_rows"] += 1
            rec["merge_sh"] += float(row.get("size") or 0.0)

    pair_markets = 0
    merge_coverage_values: list[float] = []
    for rec in by_slug.values():
        if rec["up_rows"] and rec["down_rows"]:
            pair_markets += 1
        pairable = min(rec["up_sh"], rec["down_sh"])
        if pairable > 0:
            merge_coverage_values.append(rec["merge_sh"] / pairable)

    return {
        "period": label,
        "start_day": start_day,
        "end_day": end_day,
        "rows": len(sub),
        "trade_rows": len(trades),
        "merge_rows": sum(1 for r in sub if r.get("type") == "MERGE"),
        "redeem_rows": sum(1 for r in sub if r.get("type") == "REDEEM"),
        "markets": len(by_slug),
        "pair_markets": pair_markets,
        "top_assets": by_asset.most_common(5),
        "top_families": by_family.most_common(5),
        "median_clip": round(statistics.median(clips), 2) if clips else None,
        "p90_clip": round(sorted(clips)[int(0.9 * (len(clips) - 1))], 2) if clips else None,
        "exact100_clips": sum(1 for c in clips if abs(c - 100.0) < 1e-9),
        "avg_merge_coverage": round(sum(merge_coverage_values) / len(merge_coverage_values), 3)
        if merge_coverage_values
        else None,
        "median_merge_coverage": round(statistics.median(merge_coverage_values), 3)
        if merge_coverage_values
        else None,
    }


def build_args() -> argparse.Namespace:
    ap = argparse.ArgumentParser()
    ap.add_argument("--history", required=True)
    ap.add_argument("--output", required=True)
    return ap.parse_args()


def main() -> None:
    args = build_args()
    rows = json.loads(Path(args.history).read_text())
    periods = [
        ("2026-03-20", "2026-03-27", "mar20_27"),
        ("2026-03-27", "2026-04-03", "mar27_apr03"),
        ("2026-04-03", "2026-04-10", "apr03_10"),
        ("2026-04-10", "2026-04-17", "apr10_17"),
        ("2026-04-17", "2026-04-23", "apr17_23"),
    ]
    payload = {
        "history_file": args.history,
        "periods": [summarize_period(rows, start, end, label) for start, end, label in periods],
    }
    Path(args.output).write_text(json.dumps(payload, indent=2))
    print(json.dumps(payload, indent=2))


if __name__ == "__main__":
    main()
