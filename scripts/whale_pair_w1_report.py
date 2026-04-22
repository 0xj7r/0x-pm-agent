"""Whale-pair w1 shape comparison report.

Run the whale-pair backtest against the windows in w1's activity file and
emit a compact comparison artifact (per-market + aggregate) suitable for
shape review.

w1 is a SHAPE BENCHMARK ONLY. Do not use the diffs here to pick configs.
Config selection belongs in the walk-forward script on intrinsic metrics.

Expected activity JSON schema (per row):
    proxyWallet:str, timestamp:int (unix sec), conditionId:str,
    type:str ("TRADE"|"MERGE"|"REDEEM"|"SPLIT"), size:float, usdcSize:float,
    price:float, asset:str, side:str ("BUY"|"SELL"|""),
    outcome:str ("Up"|"Down"|""), slug:str ("btc-updown-5m-<unix>").

Expected pnl JSON schema:
    list of {"t": unix_seconds, "p": cumulative_pnl_usdc}.

Usage:
    python scripts/whale_pair_w1_report.py \\
        --activity-json data/whale_analysis/activity_8b5b82.json \\
        --pnl-json data/whale_analysis/pnl_timeseries_8b5b82.json \\
        --db backtesting/btc.db \\
        --output data/whale_analysis/w1_comparison.json
"""
from __future__ import annotations

import argparse
import json
from pathlib import Path
import sys

ROOT = Path(__file__).resolve().parent.parent
if str(ROOT) not in sys.path:
    sys.path.insert(0, str(ROOT))

from backtesting.whale_pair_backtest import (
    ExecutionModel,
    build_w1_comparison,
    run_backtest,
)
from strategies.whale_pair import WhalePairConfig


DISCLAIMER = (
    "w1 is a shape benchmark only. Do not use these diffs to select params."
)


def _summarize_pnl_bracket(pnl_path: Path, activity_path: Path) -> dict | None:
    """Return the w1 pnl delta that brackets the activity window.

    Informational only: we do not use this for P&L reconciliation.
    """
    try:
        rows = json.loads(activity_path.read_text())
        pnl = json.loads(pnl_path.read_text())
    except Exception:
        return None
    btc5 = [r for r in rows if (r.get("slug") or "").startswith("btc-updown-5m-")]
    if not btc5:
        return None
    ts_min = min(int(r["timestamp"]) for r in btc5)
    ts_max = max(int(r["timestamp"]) for r in btc5)
    points = sorted(pnl, key=lambda p: p["t"])
    before = [p for p in points if p["t"] <= ts_min]
    after = [p for p in points if p["t"] >= ts_max]
    if not before or not points:
        return None
    start = before[-1]
    end = after[0] if after else points[-1]
    return {
        "activity_ts_min": ts_min,
        "activity_ts_max": ts_max,
        "pnl_start_t": start["t"],
        "pnl_start_p": start["p"],
        "pnl_end_t": end["t"],
        "pnl_end_p": end["p"],
        "pnl_delta": end["p"] - start["p"],
        "note": "Informational. Not used for strategy calibration.",
    }


def build_report(
    db_path: Path,
    activity_path: Path,
    cfg: WhalePairConfig,
    *,
    pnl_path: Path | None = None,
    execution: ExecutionModel | None = None,
) -> dict:
    report = run_backtest(
        db_path,
        cfg,
        whale_activity_path=activity_path,
        execution=execution,
    )
    # Build a compact comparison-only artifact.
    comparison = report.get("w1_comparison") or build_w1_comparison([])
    artifact: dict = {
        "disclaimer": DISCLAIMER,
        "activity_file": str(activity_path),
        "db_path": str(db_path),
        "config": {
            "accumulate_price_max": cfg.accumulate_price_max,
            "aggressive_price_max": cfg.aggressive_price_max,
            "max_pair_cost": cfg.max_pair_cost,
            "base_clip_usd": cfg.base_clip_usd,
            "aggressive_clip_usd": cfg.aggressive_clip_usd,
            "max_gross_cost_usd": cfg.max_gross_cost_usd,
            "min_seconds_from_start": cfg.min_seconds_from_start,
            "max_seconds_from_start": cfg.max_seconds_from_start,
            "completion_min_pnl_per_share": cfg.completion_min_pnl_per_share,
            "max_imbalance_ratio": cfg.max_imbalance_ratio,
        },
        "execution": report.get("execution"),
        "markets_considered": report.get("markets_considered", 0),
        "markets_traded": report.get("markets_traded", 0),
        "w1_comparison": comparison,
    }
    if pnl_path is not None:
        bracket = _summarize_pnl_bracket(pnl_path, activity_path)
        if bracket:
            artifact["pnl_bracket_info"] = bracket
    return artifact


def main() -> None:
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("--db", default="backtesting/btc.db")
    ap.add_argument(
        "--activity-json",
        default="data/whale_analysis/activity_8b5b82.json",
    )
    ap.add_argument(
        "--pnl-json",
        default="data/whale_analysis/pnl_timeseries_8b5b82.json",
    )
    ap.add_argument("--output", default="")
    ap.add_argument("--accumulate-price-max", type=float, default=0.50)
    ap.add_argument("--aggressive-price-max", type=float, default=0.10)
    ap.add_argument("--max-pair-cost", type=float, default=0.99)
    ap.add_argument("--base-clip-usd", type=float, default=10.0)
    ap.add_argument("--aggressive-clip-usd", type=float, default=25.0)
    ap.add_argument("--max-gross-cost-usd", type=float, default=200.0)
    ap.add_argument("--min-seconds-from-start", type=int, default=10)
    ap.add_argument("--max-seconds-from-start", type=int, default=298)
    ap.add_argument("--completion-min-pnl-per-share", type=float, default=0.002)
    ap.add_argument("--max-imbalance-ratio", type=float, default=3.0)
    ap.add_argument("--latency-snapshots", type=int, default=0)
    ap.add_argument("--fill-fraction", type=float, default=1.0)
    args = ap.parse_args()

    db_path = Path(args.db)
    activity_path = Path(args.activity_json)
    if not activity_path.exists():
        raise SystemExit(f"activity json not found: {activity_path}")
    pnl_path: Path | None = Path(args.pnl_json) if args.pnl_json else None
    if pnl_path is not None and not pnl_path.exists():
        pnl_path = None

    cfg = WhalePairConfig(
        accumulate_price_max=args.accumulate_price_max,
        aggressive_price_max=args.aggressive_price_max,
        max_pair_cost=args.max_pair_cost,
        base_clip_usd=args.base_clip_usd,
        aggressive_clip_usd=args.aggressive_clip_usd,
        max_gross_cost_usd=args.max_gross_cost_usd,
        min_seconds_from_start=args.min_seconds_from_start,
        max_seconds_from_start=args.max_seconds_from_start,
        completion_min_pnl_per_share=args.completion_min_pnl_per_share,
        max_imbalance_ratio=args.max_imbalance_ratio,
    )
    execution = ExecutionModel(
        latency_snapshots=max(0, int(args.latency_snapshots)),
        fill_fraction=max(0.0, min(1.0, float(args.fill_fraction))),
    )
    artifact = build_report(
        db_path,
        activity_path,
        cfg,
        pnl_path=pnl_path,
        execution=execution,
    )
    text = json.dumps(artifact, indent=2)
    if args.output:
        Path(args.output).write_text(text)
    print(text)


if __name__ == "__main__":
    main()
