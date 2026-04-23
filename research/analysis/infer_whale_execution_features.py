"""Infer execution-style features from whale activity joined to local market state.

Usage:
  python3 scripts/infer_whale_execution_features.py --wallet 0xb27bc9...
  python3 scripts/infer_whale_execution_features.py --joined data/research/wallet_research/unlawful-shear/market_join.json
"""
from __future__ import annotations

import argparse
import json
import sys
from collections import Counter, defaultdict
from pathlib import Path
from statistics import mean
from typing import Any

ROOT = Path(__file__).resolve().parent.parent
sys.path.insert(0, str(ROOT))

from research.wallet_aliases import wallet_dir_name
from scripts.join_wallet_to_market_state import main as join_main

OUT = ROOT / "data" / "research" / "wallet_research"


def _distance(fill: float | None, touch: float | None) -> float | None:
    if fill is None or touch is None:
        return None
    return float(fill) - float(touch)


def classify_execution(row: dict[str, Any], tolerance: float = 0.002) -> str:
    if row.get("type") != "TRADE":
        return "non_trade"
    side = str(row.get("side") or "").upper()
    fill_price = row.get("fill_price")
    best_bid = row.get("chosen_best_bid")
    best_ask = row.get("chosen_best_ask")
    if fill_price is None:
        return "unknown"
    if side == "BUY":
        if best_ask is None:
            return "unknown"
        delta = float(fill_price) - float(best_ask)
        if abs(delta) <= tolerance or delta > 0:
            return "likely_taker"
        return "likely_maker_or_passive"
    if side == "SELL":
        if best_bid is None:
            return "unknown"
        delta = float(fill_price) - float(best_bid)
        if abs(delta) <= tolerance or delta < 0:
            return "likely_taker"
        return "likely_maker_or_passive"
    return "unknown"


def classify_fill_quality(row: dict[str, Any], tolerance: float = 0.002) -> str:
    if row.get("type") != "TRADE":
        return "non_trade"
    fill_price = row.get("fill_price")
    bid = row.get("chosen_best_bid")
    ask = row.get("chosen_best_ask")
    side = str(row.get("side") or "").upper()
    if fill_price is None:
        return "unknown"
    if side == "BUY":
        if ask is None:
            return "unknown"
        delta = float(fill_price) - float(ask)
        if abs(delta) <= tolerance:
            return "at_ask"
        if delta < -tolerance:
            return "inside_or_better_than_ask"
        return "through_ask"
    if side == "SELL":
        if bid is None:
            return "unknown"
        delta = float(fill_price) - float(bid)
        if abs(delta) <= tolerance:
            return "at_bid"
        if delta > tolerance:
            return "inside_or_better_than_bid"
        return "through_bid"
    return "unknown"


def window_bucket(seconds_from_start: int | float | None) -> str:
    if seconds_from_start is None:
        return "unknown"
    value = float(seconds_from_start)
    if value < 0:
        return "pre_window"
    if value < 60:
        return "0_60"
    if value < 120:
        return "60_120"
    if value < 180:
        return "120_180"
    if value < 240:
        return "180_240"
    if value < 300:
        return "240_300"
    return "late"


def enrich_row(row: dict[str, Any]) -> dict[str, Any]:
    enriched = dict(row)
    execution_class = classify_execution(row)
    fill_quality = classify_fill_quality(row)
    shares = row.get("size")
    touch_size = None
    side = str(row.get("side") or "").upper()
    if side == "BUY":
        touch_size = row.get("ask_size_up") if row.get("outcome") == "Up" else row.get("ask_size_down")
    elif side == "SELL":
        touch_size = row.get("bid_size_up") if row.get("outcome") == "Up" else row.get("bid_size_down")
    depth_ratio = None
    if shares is not None and touch_size not in (None, 0):
        depth_ratio = float(shares) / float(touch_size)
    enriched.update(
        {
            "execution_class": execution_class,
            "fill_quality": fill_quality,
            "window_bucket": window_bucket(row.get("seconds_from_start")),
            "touch_depth_ratio": depth_ratio,
            "fill_vs_ask": _distance(row.get("fill_price"), row.get("chosen_best_ask")),
            "fill_vs_bid": _distance(row.get("fill_price"), row.get("chosen_best_bid")),
            "likely_negative_risk_window": row.get("ask_sum") is not None and float(row["ask_sum"]) < 1.0,
        }
    )
    return enriched


def summarize(rows: list[dict[str, Any]]) -> dict[str, Any]:
    trade_rows = [row for row in rows if row.get("type") == "TRADE"]
    by_exec = Counter(row["execution_class"] for row in trade_rows)
    by_quality = Counter(row["fill_quality"] for row in trade_rows)
    by_bucket = Counter(row["window_bucket"] for row in trade_rows)
    by_slug = defaultdict(list)
    for row in trade_rows:
        by_slug[str(row.get("slug") or "")].append(row)
    pair_windows = 0
    for slug_rows in by_slug.values():
        outcomes = {str(r.get("outcome") or "") for r in slug_rows if r.get("outcome")}
        if {"Up", "Down"}.issubset(outcomes):
            pair_windows += 1
    negative_risk_rows = [row for row in trade_rows if row.get("likely_negative_risk_window")]
    touch_ratios = [float(row["touch_depth_ratio"]) for row in trade_rows if row.get("touch_depth_ratio") is not None]
    return {
        "rows": len(rows),
        "trade_rows": len(trade_rows),
        "execution_class_counts": dict(by_exec),
        "fill_quality_counts": dict(by_quality),
        "window_bucket_counts": dict(by_bucket),
        "pair_windows": pair_windows,
        "negative_risk_trade_rows": len(negative_risk_rows),
        "avg_touch_depth_ratio": mean(touch_ratios) if touch_ratios else None,
        "median_like_touch_share": sorted(touch_ratios)[len(touch_ratios) // 2] if touch_ratios else None,
    }


def resolve_joined_path(wallet: str | None, joined_path: str | None) -> Path:
    if joined_path:
        return Path(joined_path)
    if not wallet:
        raise SystemExit("Either --wallet or --joined is required")
    wallet_dir = wallet_dir_name(wallet)
    candidates = [
        OUT / wallet_dir / "market_join.json",
        OUT / wallet_dir / "live_market_join.json",
    ]
    for candidate in candidates:
        if candidate.exists():
            return candidate
    candidate = OUT / wallet_dir / f"{wallet_dir}_market_join.json"
    if candidate.exists():
        return candidate
    raise SystemExit(f"Could not find joined market-state file for wallet {wallet}")


def ensure_joined(wallet: str | None, joined_path: Path) -> None:
    if joined_path.exists():
        return
    if not wallet:
        raise SystemExit(f"Joined file {joined_path} is missing and no wallet was provided")
    import sys

    argv_prev = sys.argv[:]
    try:
        sys.argv = ["join_wallet_to_market_state.py", "--wallet", wallet]
        join_main()
    finally:
        sys.argv = argv_prev


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--wallet")
    parser.add_argument("--joined", help="Joined wallet/market-state JSON from join_wallet_to_market_state.py")
    parser.add_argument("--output", help="Optional output path for the enriched feature payload")
    args = parser.parse_args()

    joined_path = resolve_joined_path(args.wallet, args.joined)
    ensure_joined(args.wallet, joined_path)
    payload = json.loads(joined_path.read_text())
    joined_rows = payload.get("joined_rows") or []
    enriched = [enrich_row(row) for row in joined_rows]
    summary = summarize(enriched)

    wallet_suffix = args.wallet.lower()[-6:] if args.wallet else joined_path.stem.split("_")[0]
    wallet_dir = OUT / wallet_suffix
    output_path = Path(args.output) if args.output else wallet_dir / "execution_features.json"
    output = {
        "source_joined_file": str(joined_path),
        "summary": summary,
        "rows": enriched,
    }
    output_path.write_text(json.dumps(output, indent=2))
    print(json.dumps({"summary": summary, "output": str(output_path)}, indent=2))


if __name__ == "__main__":
    main()
