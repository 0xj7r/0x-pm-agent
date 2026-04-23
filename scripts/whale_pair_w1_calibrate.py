"""Whale-pair w1 shape diagnostic across a small config grid.

Sweeps a tiny grid of WhalePairConfig values and, for each config, reports
the shape diff (sim minus w1) aggregates produced by the backtest.

IMPORTANT: This is a DIAGNOSTIC. w1 is a shape benchmark, not a tuning
target. We do not select a winning config from this script. Use the
walk-forward evaluation for parameter selection. The output here exists
to show how closely the strategy's shape tracks w1 across configs, and
to flag structural divergences (e.g. always-too-late last buy offsets).

Expected activity JSON schema (per row):
    proxyWallet:str, timestamp:int (unix sec), conditionId:str,
    type:str ("TRADE"|"MERGE"|"REDEEM"|"SPLIT"), size:float, usdcSize:float,
    price:float, asset:str, side:str ("BUY"|"SELL"|""),
    outcome:str ("Up"|"Down"|""), slug:str ("btc-updown-5m-<unix>").

Expected pnl JSON schema:
    list of {"t": unix_seconds, "p": cumulative_pnl_usdc}.

Usage:
    python scripts/whale_pair_w1_calibrate.py \\
        --activity-json data/whale_analysis/activity_8b5b82.json \\
        --db backtesting/btc.db \\
        --output data/whale_analysis/w1_calibrate.json
"""
from __future__ import annotations

import argparse
import json
from itertools import product
from pathlib import Path
import sys

ROOT = Path(__file__).resolve().parent.parent
if str(ROOT) not in sys.path:
    sys.path.insert(0, str(ROOT))

from backtesting.whale_pair_backtest import (
    ExecutionModel,
    run_backtest,
)
from scripts.whale_pair.strategy_presets import W1_COMPARE_VARIANTS
from strategies.whale_pair import WhalePairConfig


DISCLAIMER = (
    "Shape benchmark only. Not used to pick configs. "
    "Parameter selection must come from intrinsic walk-forward metrics."
)


def _parse_list(csv: str, cast) -> list:
    if not csv:
        return []
    return [cast(x.strip()) for x in csv.split(",") if x.strip()]


def sweep(
    db_path: Path,
    activity_path: Path,
    *,
    base: WhalePairConfig,
    base_clip_usds: list[float],
    aggressive_clip_usds: list[float],
    max_seconds_from_start_values: list[int],
    max_imbalance_ratios: list[float],
    execution: ExecutionModel,
) -> list[dict]:
    rows: list[dict] = []
    if not base_clip_usds:
        base_clip_usds = [base.base_clip_usd]
    if not aggressive_clip_usds:
        aggressive_clip_usds = [base.aggressive_clip_usd]
    if not max_seconds_from_start_values:
        max_seconds_from_start_values = [base.max_seconds_from_start]
    if not max_imbalance_ratios:
        max_imbalance_ratios = [base.max_imbalance_ratio]

    for base_clip, agg_clip, max_sec, max_imb in product(
        base_clip_usds,
        aggressive_clip_usds,
        max_seconds_from_start_values,
        max_imbalance_ratios,
    ):
        cfg = WhalePairConfig(
            accumulate_price_max=base.accumulate_price_max,
            aggressive_price_max=base.aggressive_price_max,
            max_pair_cost=base.max_pair_cost,
            base_clip_usd=float(base_clip),
            aggressive_clip_usd=float(agg_clip),
            max_gross_cost_usd=base.max_gross_cost_usd,
            min_seconds_from_start=base.min_seconds_from_start,
            max_seconds_from_start=int(max_sec),
            completion_min_pnl_per_share=base.completion_min_pnl_per_share,
            max_imbalance_ratio=float(max_imb),
        )
        report = run_backtest(
            db_path,
            cfg,
            whale_activity_path=activity_path,
            execution=execution,
        )
        comparison = report.get("w1_comparison") or {
            "overlap_markets": 0,
            "overlap_markets_both_active": 0,
            "aggregates": {},
        }
        rows.append(
            {
                "config": {
                    "base_clip_usd": cfg.base_clip_usd,
                    "aggressive_clip_usd": cfg.aggressive_clip_usd,
                    "max_seconds_from_start": cfg.max_seconds_from_start,
                    "max_imbalance_ratio": cfg.max_imbalance_ratio,
                },
                "markets_considered": report.get("markets_considered", 0),
                "markets_traded": report.get("markets_traded", 0),
                "overlap_markets": comparison.get("overlap_markets", 0),
                "overlap_markets_both_active": comparison.get(
                    "overlap_markets_both_active", 0
                ),
                "aggregates": comparison.get("aggregates", {}),
            }
        )
    return rows


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
    ap.add_argument("--base-clip-usds", default="")
    ap.add_argument("--aggressive-clip-usds", default="")
    ap.add_argument("--max-seconds-from-start-values", default="")
    ap.add_argument("--max-imbalance-ratios", default="")
    ap.add_argument("--latency-snapshots", type=int, default=0)
    ap.add_argument("--fill-fraction", type=float, default=1.0)
    args = ap.parse_args()

    db_path = Path(args.db)
    activity_path = Path(args.activity_json)
    if not activity_path.exists():
        raise SystemExit(f"activity json not found: {activity_path}")

    base = W1_COMPARE_VARIANTS.get("pair_recycler", WhalePairConfig())
    execution = ExecutionModel(
        latency_snapshots=max(0, int(args.latency_snapshots)),
        fill_fraction=max(0.0, min(1.0, float(args.fill_fraction))),
    )
    rows = sweep(
        db_path,
        activity_path,
        base=base,
        base_clip_usds=_parse_list(args.base_clip_usds, float),
        aggressive_clip_usds=_parse_list(args.aggressive_clip_usds, float),
        max_seconds_from_start_values=_parse_list(
            args.max_seconds_from_start_values, int
        ),
        max_imbalance_ratios=_parse_list(args.max_imbalance_ratios, float),
        execution=execution,
    )

    artifact = {
        "disclaimer": DISCLAIMER,
        "activity_file": str(activity_path),
        "db_path": str(db_path),
        "execution": {
            "latency_snapshots": execution.latency_snapshots,
            "fill_fraction": execution.fill_fraction,
        },
        "results": rows,
    }
    text = json.dumps(artifact, indent=2)
    if args.output:
        Path(args.output).write_text(text)
    print(text)


if __name__ == "__main__":
    main()
