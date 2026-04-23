"""Fetch and profile a Polymarket wallet for strategy research.

This is the operational wrapper around the current whale workflow:
  1. fetch raw activity / pnl / current value
  2. save artifacts under data/research/wallet_research/<wallet>/
  3. join activity rows to local historical market snapshots
  4. emit a compact research summary

Usage:
  python3 scripts/profile_wallet_research.py --wallet 0xb27b...
  python3 scripts/profile_wallet_research.py --wallet 0x04b6... --label peaceful-quadrant
"""
from __future__ import annotations

import argparse
import json
from collections import Counter
from datetime import UTC, datetime
from pathlib import Path
from statistics import mean

import httpx

from research.wallet_aliases import wallet_dir_name
from scripts.join_wallet_to_market_state import main as join_main

ROOT = Path(__file__).resolve().parent.parent
OUT = ROOT / "data" / "research" / "wallet_research"


def fetch_json(url: str, *, params: dict[str, str]) -> object:
    resp = httpx.get(url, params=params, timeout=60)
    resp.raise_for_status()
    return resp.json()


def save_json(path: Path, payload: object) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(json.dumps(payload, indent=2))


def eod_series(series: list[dict]) -> dict[str, float]:
    out: dict[str, float] = {}
    for row in series:
        day = datetime.fromtimestamp(int(row["t"]), tz=UTC).strftime("%Y-%m-%d")
        out[day] = float(row["p"])
    return dict(sorted(out.items()))


def summarize_pnl(series: list[dict]) -> dict[str, float | int | None]:
    if not series:
        return {"points": 0, "current_pnl": None}
    running_max = float(series[0]["p"])
    max_dd = 0.0
    for row in series:
        p = float(row["p"])
        running_max = max(running_max, p)
        max_dd = max(max_dd, running_max - p)
    return {
        "points": len(series),
        "current_pnl": float(series[-1]["p"]),
        "peak_pnl": max(float(r["p"]) for r in series),
        "trough_pnl": min(float(r["p"]) for r in series),
        "max_drawdown_abs": max_dd,
    }


def summarize_activity(rows: list[dict]) -> dict[str, object]:
    by_type = Counter(str(r.get("type") or "") for r in rows)
    by_slug: dict[str, set[str]] = {}
    for row in rows:
        slug = str(row.get("slug") or "")
        outcome = str(row.get("outcome") or "")
        if not slug or not outcome:
            continue
        by_slug.setdefault(slug, set()).add(outcome)
    hedged = sum(1 for sides in by_slug.values() if {"Up", "Down"}.issubset(sides))
    buy_prices = [float(r["price"]) for r in rows if r.get("type") == "TRADE" and r.get("price") is not None]
    usdc_sizes = [float(r["usdcSize"]) for r in rows if r.get("usdcSize") is not None]
    return {
        "rows": len(rows),
        "type_counts": dict(by_type),
        "market_count": len(by_slug),
        "hedged_market_count": hedged,
        "avg_trade_price": mean(buy_prices) if buy_prices else None,
        "avg_usdc_size": mean(usdc_sizes) if usdc_sizes else None,
        "latest_timestamp": max((int(r["timestamp"]) for r in rows), default=None),
        "earliest_timestamp": min((int(r["timestamp"]) for r in rows), default=None),
    }


def build_args() -> argparse.Namespace:
    ap = argparse.ArgumentParser()
    ap.add_argument("--wallet", required=True)
    ap.add_argument("--label", default="")
    ap.add_argument("--activity-limit", type=int, default=3500)
    return ap.parse_args()


def main() -> None:
    args = build_args()
    wallet = args.wallet.lower()
    dir_name = wallet_dir_name(wallet)
    label = args.label or dir_name
    wallet_dir = OUT / dir_name

    activity = fetch_json(
        "https://data-api.polymarket.com/activity",
        params={"user": wallet, "limit": str(args.activity_limit), "offset": "0"},
    )
    pnl = fetch_json(
        "https://user-pnl-api.polymarket.com/user-pnl",
        params={"user_address": wallet, "interval": "all"},
    )
    value = fetch_json(
        "https://data-api.polymarket.com/value",
        params={"user": wallet},
    )

    activity_path = wallet_dir / "activity.json"
    pnl_path = wallet_dir / "pnl_timeseries.json"
    daily_path = wallet_dir / "daily_pnl.json"
    value_path = wallet_dir / "value.json"

    save_json(activity_path, activity)
    save_json(pnl_path, pnl)
    save_json(daily_path, eod_series(pnl if isinstance(pnl, list) else []))
    save_json(value_path, value)

    # Run the existing joiner on the saved activity file by reusing its CLI.
    import sys

    argv_prev = sys.argv[:]
    try:
        sys.argv = [
            "join_wallet_to_market_state.py",
            "--activity",
            str(activity_path),
        ]
        join_main()
    finally:
        sys.argv = argv_prev

    join_path = wallet_dir / "market_join.json"
    join_payload = json.loads(join_path.read_text()) if join_path.exists() else {"summary": {}}

    summary = {
        "wallet": wallet,
        "label": label,
        "fetched_at": datetime.now(tz=UTC).isoformat(),
        "pnl": summarize_pnl(pnl if isinstance(pnl, list) else []),
        "activity": summarize_activity(activity if isinstance(activity, list) else []),
        "current_value": value[0]["value"] if isinstance(value, list) and value else None,
        "market_join": join_payload.get("summary", {}),
    }
    summary_path = wallet_dir / f"summary.json"
    save_json(summary_path, summary)
    print(json.dumps(summary, indent=2))
    print(f"Saved: {summary_path}")


if __name__ == "__main__":
    main()
