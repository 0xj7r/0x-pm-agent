#!/usr/bin/env python3
"""Validate whale-pair strategies directly against observed W1 activity.

This is the practical "does it match W1 execution" layer:
1. Run candidate configs per variant through the historical backtest.
2. Compare to W1 activity shape metrics from `run_backtest` (`w1_comparison`).
3. Score and rank candidates by shape-mismatch and report execution realism.

Defaults are intentionally conservative and close to the observed baseline.
Use this after any strategy logic changes to confirm parity before Rust work.
"""

from __future__ import annotations

import argparse
import json
from itertools import product
from pathlib import Path
from statistics import median
from typing import Any
import sys

from scripts.whale_pair._paths import repo_root

ROOT = repo_root(Path(__file__))
if str(ROOT) not in sys.path:
    sys.path.insert(0, str(ROOT))

from backtesting.whale_pair_backtest import ExecutionModel, run_backtest
from scripts.whale_pair.strategy_presets import W1_VALIDATION_SPECS, ValidationSpec
from strategies.whale_pair import WhalePairConfig


BASE_SPECS: dict[str, ValidationSpec] = {name: spec for name, spec in W1_VALIDATION_SPECS.items()}


SHAPE_WEIGHT = {
    "first_buy_offset_sec_mean_diff": 1.0 / 60.0,
    "last_buy_offset_sec_mean_diff": 1.0 / 60.0,
    "fill_count_mean_diff": 1.0 / 10.0,
    "fill_count_median_diff": 1.0 / 10.0,
    "buy_usdc_total_mean_diff": 1.0 / 100.0,
    "buy_usdc_total_median_diff": 1.0 / 100.0,
    "buy_usdc_up_mean_diff": 1.0 / 100.0,
    "buy_usdc_down_mean_diff": 1.0 / 100.0,
    "imbalance_ratio_mean_diff": 1.0,
    "imbalance_ratio_median_diff": 1.0,
}

def _ordered_unique(values: list[str]) -> list[str]:
    out: list[str] = []
    seen: set[str] = set()
    for value in values:
        if not value or value in seen:
            continue
        seen.add(value)
        out.append(value)
    return out


def _parse_floats(raw: str, default: list[float]) -> list[float]:
    if not raw:
        return default
    out: list[float] = []
    for item in raw.split(","):
        item = item.strip()
        if not item:
            continue
        out.append(float(item))
    return out or default


def _parse_ints(raw: str, default: list[int]) -> list[int]:
    if not raw:
        return default
    out: list[int] = []
    for item in raw.split(","):
        item = item.strip()
        if not item:
            continue
        out.append(int(item))
    return out or default


def _score_shape(aggregates: dict[str, Any]) -> tuple[float, int]:
    """Return (weighted_error, missing_fields)."""
    missing = 0
    score = 0.0
    for key, weight in SHAPE_WEIGHT.items():
        value = aggregates.get(key)
        if value is None:
            missing += 1
            continue
        score += abs(float(value)) * weight
    return score, missing


def _run_candidate(
    db_path: Path,
    cfg: WhalePairConfig,
    *,
    whale_activity_path: Path,
    market_type: str,
    limit: int | None,
    execution: ExecutionModel,
) -> tuple[dict[str, Any], float, int]:
    report = run_backtest(
        db_path,
        cfg,
        market_type=market_type,
        limit=limit,
        whale_activity_path=whale_activity_path,
        execution=execution,
    )
    comparison = report.get("w1_comparison") or {"overlaps": {}, "aggregates": {}}
    aggregates = comparison.get("aggregates") or {}
    shape_error, missing_fields = _score_shape(aggregates)
    missing_penalty = missing_fields * 2.5
    return report, shape_error + missing_penalty, missing_fields


def _serialize_cfg(cfg: WhalePairConfig) -> dict[str, Any]:
    return {
        "max_pair_cost": cfg.max_pair_cost,
        "accumulate_price_max": cfg.accumulate_price_max,
        "aggressive_price_max": cfg.aggressive_price_max,
        "base_clip_usd": cfg.base_clip_usd,
        "aggressive_clip_usd": cfg.aggressive_clip_usd,
        "base_clip_shares": cfg.base_clip_shares,
        "aggressive_clip_shares": cfg.aggressive_clip_shares,
        "max_gross_cost_usd": cfg.max_gross_cost_usd,
        "min_seconds_from_start": cfg.min_seconds_from_start,
        "max_seconds_from_start": cfg.max_seconds_from_start,
        "completion_min_pnl_per_share": cfg.completion_min_pnl_per_share,
        "max_imbalance_ratio": cfg.max_imbalance_ratio,
    }


def _to_payload_entry(
    spec: ValidationSpec,
    cfg: WhalePairConfig,
    report: dict[str, Any],
    shape_error: float,
    missing_fields: int,
) -> dict[str, Any]:
    comp = report.get("w1_comparison") or {}
    return {
        "variant": cfg.variant,
        "description": spec.description,
        "markets_considered": report.get("markets_considered", 0),
        "markets_traded": report.get("markets_traded", 0),
        "attempted_orders": report.get("attempted_orders", 0),
        "missed_orders": report.get("missed_orders", 0),
        "partial_orders": report.get("partial_orders", 0),
        "execution_slippage_usd": report.get("execution_slippage_usd", 0.0),
        "gross_cost_usd": report.get("gross_cost_usd", 0.0),
        "merged_pnl_usd": report.get("merged_pnl_usd", 0.0),
        "residual_pnl_usd": report.get("residual_pnl_usd", 0.0),
        "total_pnl_usd": report.get("total_pnl_usd", 0.0),
        "win_rate": report.get("win_rate", 0.0),
        "shape_error": shape_error,
        "shape_missing_fields": missing_fields,
        "w1_comparison": {
            "overlap_markets": comp.get("overlap_markets", 0),
            "overlap_markets_both_active": comp.get(
                "overlap_markets_both_active", 0
            ),
            "both_active_rate": comp.get("both_active_rate"),
            "aggregates": comp.get("aggregates", {}),
        },
        "cfg": _serialize_cfg(cfg),
    }


def _best(cfgs: list[ValidationSpec]) -> list[str]:
    return [cfg.variant for cfg in cfgs]


def main() -> None:
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("--db", default="backtesting/btc.db")
    ap.add_argument(
        "--activity-json",
        default="data/research/whale_analysis/unlawful-shear/activity.json",
        help="W1 activity slice JSON",
    )
    ap.add_argument(
        "--output",
        default="",
        help="Optional path to persist full JSON artifact",
    )
    ap.add_argument("--limit", type=int, default=0)
    ap.add_argument("--market-type", default="5m")

    ap.add_argument(
        "--variants",
        default="pair_recycler,skewed_pair_builder,passive_ladder,w1_mimic",
        help="Comma-separated variants to test",
    )

    ap.add_argument("--max-pair-cost", default="0.985,0.99")
    ap.add_argument("--accumulate-price-max", default="0.50")
    ap.add_argument("--aggressive-price-max", default="0.10")
    ap.add_argument("--base-clip-usd", default="20,25")
    ap.add_argument("--aggressive-clip-usd", default="50")
    ap.add_argument("--base-clip-shares", default="25")
    ap.add_argument("--aggressive-clip-shares", default="100")
    ap.add_argument("--max-gross-cost-usd", default="200,250")
    ap.add_argument("--min-seconds-from-start", default="10")
    ap.add_argument("--max-seconds-from-start", default="240")
    ap.add_argument("--completion-min-pnl-per-share", default="0.002")
    ap.add_argument("--max-imbalance-ratio", default="3,4")

    ap.add_argument("--latency-snapshots", type=int, default=0)
    ap.add_argument("--fill-fraction", type=float, default=1.0)

    ap.add_argument(
        "--top-k",
        type=int,
        default=10,
        help="How many best candidates to print in ranked output",
    )
    args = ap.parse_args()

    selected_variants = _ordered_unique([v.strip() for v in args.variants.split(",") if v.strip()])
    candidate_specs = [
        spec for name, spec in BASE_SPECS.items() if name in selected_variants
    ]

    if not candidate_specs:
        raise SystemExit("No valid variants selected")

    max_pair_costs = _parse_floats(args.max_pair_cost, [0.985])
    accumulate_price_maxs = _parse_floats(args.accumulate_price_max, [0.50])
    aggressive_price_maxs = _parse_floats(args.aggressive_price_max, [0.10])
    base_clip_usds = _parse_floats(args.base_clip_usd, [25.0])
    aggressive_clip_usds = _parse_floats(args.aggressive_clip_usd, [50.0])
    base_clip_shares = _parse_floats(args.base_clip_shares, [25.0])
    aggressive_clip_shares = _parse_floats(args.aggressive_clip_shares, [100.0])
    max_gross_costs = _parse_floats(args.max_gross_cost_usd, [250.0])
    min_seconds_values = _parse_ints(args.min_seconds_from_start, [10])
    max_seconds_values = _parse_ints(args.max_seconds_from_start, [240])
    completion_vals = _parse_floats(args.completion_min_pnl_per_share, [0.002])
    imbalance_vals = _parse_floats(args.max_imbalance_ratio, [3.0])

    whale_path = Path(args.activity_json)
    if not whale_path.exists():
        raise SystemExit(f"activity json not found: {whale_path}")

    execution = ExecutionModel(
        latency_snapshots=max(0, int(args.latency_snapshots)),
        fill_fraction=max(0.0, min(1.0, float(args.fill_fraction))),
    )

    runs: list[dict[str, Any]] = []
    tested_candidates = 0
    filtered_candidates = 0
    for spec in candidate_specs:
        share_sweeps = (base_clip_shares, aggressive_clip_shares)
        if spec.variant not in ("passive_ladder", "w1_mimic"):
            share_sweeps = ([None], [None])
        for candidate in product(
            max_pair_costs,
            accumulate_price_maxs,
            aggressive_price_maxs,
            base_clip_usds,
            aggressive_clip_usds,
            share_sweeps[0],
            share_sweeps[1],
            max_gross_costs,
            min_seconds_values,
            max_seconds_values,
            completion_vals,
            imbalance_vals,
        ):
            (
                max_pair_cost,
                accumulate_price_max,
                aggressive_price_max,
                base_clip_usd,
                aggressive_clip_usd,
                base_clip_share,
                aggressive_clip_share,
                max_gross_cost_usd,
                min_seconds,
                max_seconds,
                completion_min_pnl,
                max_imbalance_ratio,
            ) = candidate

            cfg = WhalePairConfig(
                variant=spec.variant,
                max_pair_cost=float(max_pair_cost),
                accumulate_price_max=float(accumulate_price_max),
                aggressive_price_max=float(aggressive_price_max),
                base_clip_usd=float(base_clip_usd),
                aggressive_clip_usd=float(aggressive_clip_usd),
                base_clip_shares=(None if base_clip_share is None else float(base_clip_share)),
                aggressive_clip_shares=(None if aggressive_clip_share is None else float(aggressive_clip_share)),
                max_gross_cost_usd=float(max_gross_cost_usd),
                min_seconds_from_start=int(min_seconds),
                max_seconds_from_start=int(max_seconds),
                completion_min_pnl_per_share=float(completion_min_pnl),
                max_imbalance_ratio=float(max_imbalance_ratio),
            )

            if cfg.max_seconds_from_start < cfg.min_seconds_from_start:
                filtered_candidates += 1
                continue
            tested_candidates += 1

            report, shape_error, missing_fields = _run_candidate(
                Path(args.db),
                cfg,
                market_type=args.market_type,
                limit=(args.limit or None),
                whale_activity_path=whale_path,
                execution=execution,
            )

            # Skip zero-overlap candidates unless we're intentionally probing.
            overlap = (report.get("w1_comparison") or {}).get("overlap_markets", 0)
            if overlap <= 0:
                filtered_candidates += 1
                continue

            runs.append(
                _to_payload_entry(spec, cfg, report, shape_error, missing_fields)
            )

    if not runs:
        raise SystemExit("No candidates had W1 overlap; check activity file and variant filters.")

    # Ranking: prioritize shape parity, then economic outcomes.
    runs.sort(
        key=lambda row: (
            row["shape_error"],
            -row["total_pnl_usd"],
            -row["markets_traded"],
            row["attempted_orders"],
        )
    )

    top_k = max(1, args.top_k)
    shape_summary = [
        {
            "rank": idx + 1,
            "variant": row["variant"],
            "shape_error": row["shape_error"],
            "shape_missing_fields": row["shape_missing_fields"],
            "overlap_markets": row["w1_comparison"].get("overlap_markets"),
            "both_active_rate": row["w1_comparison"].get("both_active_rate"),
            "total_pnl_usd": row["total_pnl_usd"],
            "win_rate": row["win_rate"],
            "markets_traded": row["markets_traded"],
            "markets_considered": row["markets_considered"],
        }
        for idx, row in enumerate(runs[:top_k])
    ]

    by_variant: dict[str, Any] = {}
    for variant in _best(candidate_specs):
        variant_runs = [row for row in runs if row["variant"] == variant]
        by_variant[variant] = {
            "count": len(variant_runs),
            "best_shape_error": (
                min(r["shape_error"] for r in variant_runs)
                if variant_runs
                else None
            ),
            "best_total_pnl_usd": (
                max(r["total_pnl_usd"] for r in variant_runs)
                if variant_runs
                else None
            ),
        }

    aggregate_shape_errors = [row["shape_error"] for row in runs]
    pnl_values = [row["total_pnl_usd"] for row in runs]

    payload: dict[str, Any] = {
        "db_path": args.db,
        "activity_json": str(whale_path),
        "execution": {
            "latency_snapshots": execution.latency_snapshots,
            "fill_fraction": execution.fill_fraction,
        },
        "market_type": args.market_type,
        "limit": args.limit,
        "variant_descriptions": {
            spec.variant: spec.description for spec in candidate_specs
        },
        "candidates": {
            "tested": len(runs),
            "filtered_out": filtered_candidates,
            "tested_configurations": tested_candidates,
            "top_k": top_k,
            "shape_error_median": median(aggregate_shape_errors),
            "shape_error_min": min(aggregate_shape_errors),
            "shape_error_max": max(aggregate_shape_errors),
            "total_pnl_median": median(pnl_values),
            "total_pnl_min": min(pnl_values),
            "total_pnl_max": max(pnl_values),
            "by_variant": by_variant,
        },
        "ranked": runs,
        "top": shape_summary,
    }

    print(json.dumps(payload, indent=2))
    if args.output:
        Path(args.output).write_text(json.dumps(payload, indent=2))


if __name__ == "__main__":
    main()
