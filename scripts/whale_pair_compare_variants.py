#!/usr/bin/env python3
"""Compare explicit two-sided whale-pair variants on historical BTC data."""
from __future__ import annotations

import argparse
import json
from dataclasses import replace
from pathlib import Path
import sys

ROOT = Path(__file__).resolve().parent.parent
if str(ROOT) not in sys.path:
    sys.path.insert(0, str(ROOT))

from backtesting.whale_pair_backtest import ExecutionModel, run_backtest
from strategies.whale_pair import WhalePairConfig


BASE_CFG = WhalePairConfig(
    max_pair_cost=0.985,
    accumulate_price_max=0.50,
    aggressive_price_max=0.10,
    base_clip_usd=25.0,
    aggressive_clip_usd=50.0,
    max_gross_cost_usd=250.0,
    min_seconds_from_start=10,
    max_seconds_from_start=240,
    completion_min_pnl_per_share=0.002,
    max_imbalance_ratio=3.0,
)


def build_variants() -> dict[str, WhalePairConfig]:
    return {
        "pair_recycler": replace(BASE_CFG, variant="pair_recycler"),
        "skewed_pair_builder": replace(
            BASE_CFG,
            variant="skewed_pair_builder",
            accumulate_price_max=0.60,
            max_imbalance_ratio=4.0,
        ),
        "passive_ladder": replace(
            BASE_CFG,
            variant="passive_ladder",
            max_pair_cost=0.99,
            max_imbalance_ratio=4.0,
            base_clip_shares=25.0,
            aggressive_clip_shares=100.0,
        ),
    }


def summarize(report: dict) -> dict:
    cfg = report["config"]
    return {
        "variant": cfg["variant"],
        "markets_considered": report["markets_considered"],
        "markets_traded": report["markets_traded"],
        "win_rate": report["win_rate"],
        "gross_cost_usd": report["gross_cost_usd"],
        "merged_pnl_usd": report["merged_pnl_usd"],
        "residual_pnl_usd": report["residual_pnl_usd"],
        "total_pnl_usd": report["total_pnl_usd"],
        "attempted_orders": report["attempted_orders"],
        "missed_orders": report["missed_orders"],
        "partial_orders": report["partial_orders"],
        "execution_slippage_usd": report["execution_slippage_usd"],
    }


def _rank(summary: list[dict]) -> list[dict]:
    ranked = sorted(
        summary,
        key=lambda row: (
            row["total_pnl_usd"],
            row["win_rate"] if row["markets_traded"] else -1.0,
        ),
        reverse=True,
    )
    for idx, row in enumerate(ranked, 1):
        row["rank"] = idx
    return ranked


def main() -> None:
    ap = argparse.ArgumentParser()
    ap.add_argument("--db", default="backtesting/btc.db")
    ap.add_argument("--limit", type=int, default=0)
    ap.add_argument("--whale-activity", default="")
    ap.add_argument("--latency-snapshots", type=int, default=0)
    ap.add_argument("--fill-fraction", type=float, default=1.0)
    ap.add_argument("--output", default="")
    args = ap.parse_args()

    execution = ExecutionModel(
        latency_snapshots=max(0, int(args.latency_snapshots)),
        fill_fraction=max(0.0, min(1.0, float(args.fill_fraction))),
    )
    whale_path = Path(args.whale_activity) if args.whale_activity else None

    full: dict[str, dict] = {}
    summary: list[dict] = []
    for name, cfg in build_variants().items():
        report = run_backtest(
            Path(args.db),
            cfg,
            limit=(args.limit or None),
            whale_activity_path=whale_path,
            execution=execution,
        )
        full[name] = report
        summary.append(summarize(report))

    ranked_summary = _rank(summary)
    payload = {
        "db_path": str(Path(args.db)),
        "execution": {
            "latency_snapshots": execution.latency_snapshots,
            "fill_fraction": execution.fill_fraction,
        },
        "summary": ranked_summary,
        "reports": full,
    }
    text = json.dumps(payload, indent=2)
    if args.output:
        Path(args.output).write_text(text)
    print(text)


if __name__ == "__main__":
    main()
