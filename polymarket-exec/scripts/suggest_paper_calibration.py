#!/usr/bin/env python3
"""Suggest paper-env knob adjustments based on a `paper_report.json`.

Reads the report card produced by a shadow_live or replay session and
emits recommended changes to the four Phase 2 knobs (submit-latency,
queue-depth-fraction, post-only-reject-probability, cancel-race-window).
Pure heuristic — surfaces directional suggestions, not optimal values.

Usage:
    polymarket-exec/scripts/suggest_paper_calibration.py path/to/paper_report.json

Output: JSON to stdout with `current` (assumed defaults), `observed`
(metrics that drove the suggestion), and `suggested` (knob changes plus
rationale strings the operator can read before applying).

The point: replace the eyeball-the-report-and-guess workflow with a
documented heuristic so calibration sessions are reproducible.
"""

from __future__ import annotations

import argparse
import json
import sys
from pathlib import Path
from typing import Any


DEFAULT_KNOBS = {
    "paper_submit_latency_ms": 150,
    "paper_queue_depth_fraction": 0.75,
    "paper_post_only_reject_probability": 0.85,
    "paper_cancel_race_window_ms": 500,
    "paper_maker_rebate_coeff": 0.0,
    "paper_taker_fee_coeff_override": None,
}


def suggest(report: dict[str, Any]) -> dict[str, Any]:
    fills = report.get("fills", {})
    edge = report.get("edge", {})
    queue = report.get("queue", {})
    vs_whale = report.get("vs_whale", {})

    total_fills = fills.get("total_count", 0)
    maker_fraction = fills.get("maker_fraction", 0.0)
    realized_edge = edge.get("realized_edge_usd", 0.0)
    expected_edge = edge.get("expected_edge_usd", 0.0)
    edge_capture_ratio = edge.get("edge_capture_ratio")
    avg_slippage_bps = edge.get("avg_slippage_bps", 0.0)
    post_only_rejects = queue.get("post_only_reject_count", 0)
    notional_capture_ratio = vs_whale.get("notional_capture_ratio")

    suggestions: list[dict[str, Any]] = []

    if total_fills == 0:
        suggestions.append({
            "knob": "paper_queue_depth_fraction",
            "current": DEFAULT_KNOBS["paper_queue_depth_fraction"],
            "suggested": 0.50,
            "direction": "decrease",
            "rationale": (
                "Zero fills observed. Either the strategy never submitted, or "
                "every submitted maker order was queued behind too much depth. "
                "Lowering queue_depth_fraction to 0.50 makes paper give us 50% "
                "of top-of-book per attempt instead of 25%."
            ),
        })

    if total_fills > 0 and maker_fraction < 0.40:
        suggestions.append({
            "knob": "paper_post_only_reject_probability",
            "current": DEFAULT_KNOBS["paper_post_only_reject_probability"],
            "suggested": 0.60,
            "direction": "decrease",
            "rationale": (
                f"Maker fraction is {maker_fraction:.2f} (target >= 0.7 for a "
                "maker-first strategy). High taker fraction usually means "
                "post-only rejects fired too often and the strategy fell back "
                "to crossing. Lowering reject probability to 0.6 is the first "
                "step; if maker fraction stays low after that, look at the "
                "strategy's repricing logic."
            ),
        })

    if isinstance(edge_capture_ratio, (int, float)) and edge_capture_ratio < 0.5 and total_fills > 5:
        suggestions.append({
            "knob": "paper_submit_latency_ms",
            "current": DEFAULT_KNOBS["paper_submit_latency_ms"],
            "suggested": 80,
            "direction": "decrease",
            "rationale": (
                f"Edge capture ratio {edge_capture_ratio:.2f} (realized "
                f"{realized_edge:.2f} vs expected {expected_edge:.2f}). "
                "Most likely cause: by the time paper allows the fill, the "
                "book has moved away from the quote price. Cutting "
                "submit_latency_ms from 150 to 80 lets fills land closer to "
                "the quote moment. If realized edge stays poor, the gap is "
                "real and the strategy's edge model is over-optimistic."
            ),
        })

    if avg_slippage_bps > 25.0 and total_fills > 0:
        suggestions.append({
            "knob": "paper_post_only_reject_probability",
            "current": DEFAULT_KNOBS["paper_post_only_reject_probability"],
            "suggested": 0.95,
            "direction": "increase",
            "rationale": (
                f"Average slippage {avg_slippage_bps:.1f} bps is high — paper "
                "is letting orders cross too easily. Raising reject probability "
                "to 0.95 forces the strategy to wait for a non-crossing book "
                "or visibly reprice."
            ),
        })

    if isinstance(notional_capture_ratio, (int, float)):
        if notional_capture_ratio < 0.05:
            suggestions.append({
                "knob": "paper_queue_depth_fraction",
                "current": DEFAULT_KNOBS["paper_queue_depth_fraction"],
                "suggested": 0.50,
                "direction": "decrease",
                "rationale": (
                    f"Notional capture ratio {notional_capture_ratio:.4f} "
                    "(we filled tiny vs the whale on the same window). "
                    "Most likely the queue depth assumption is too pessimistic "
                    "OR the strategy sized too small. Lower queue_depth_fraction "
                    "to 0.50 first; if that doesn't move the ratio, examine "
                    "strategy sizing."
                ),
            })
        elif notional_capture_ratio > 3.0:
            suggestions.append({
                "knob": "paper_queue_depth_fraction",
                "current": DEFAULT_KNOBS["paper_queue_depth_fraction"],
                "suggested": 0.85,
                "direction": "increase",
                "rationale": (
                    f"Notional capture ratio {notional_capture_ratio:.2f} "
                    "(we filled 3x+ what the whale did). Either we're "
                    "submitting too aggressively or the paper fill model is "
                    "over-generous. Raise queue_depth_fraction to 0.85 to "
                    "be more conservative on top-of-book claim rate."
                ),
            })

    if not suggestions:
        suggestions.append({
            "knob": None,
            "current": None,
            "suggested": None,
            "direction": "hold",
            "rationale": (
                "No directional adjustment surfaced. Heuristics expect a "
                "richer session (more fills, vs_whale data populated). Run "
                "shadow_live for longer or against a higher-volume window."
            ),
        })

    return {
        "current": DEFAULT_KNOBS,
        "observed": {
            "total_fills": total_fills,
            "maker_fraction": maker_fraction,
            "realized_edge_usd": realized_edge,
            "expected_edge_usd": expected_edge,
            "edge_capture_ratio": edge_capture_ratio,
            "avg_slippage_bps": avg_slippage_bps,
            "post_only_reject_count": post_only_rejects,
            "notional_capture_ratio": notional_capture_ratio,
        },
        "suggested": suggestions,
    }


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("report", type=Path, help="Path to paper_report.json")
    parser.add_argument(
        "--output",
        type=Path,
        default=None,
        help="Optional path to write the suggestion JSON (defaults to stdout).",
    )
    args = parser.parse_args()

    with args.report.open("r", encoding="utf-8") as fh:
        report = json.load(fh)

    suggestions = suggest(report)
    rendered = json.dumps(suggestions, indent=2) + "\n"
    if args.output:
        args.output.write_text(rendered, encoding="utf-8")
        print(f"wrote {args.output}")
    else:
        sys.stdout.write(rendered)
    return 0


if __name__ == "__main__":
    sys.exit(main())
