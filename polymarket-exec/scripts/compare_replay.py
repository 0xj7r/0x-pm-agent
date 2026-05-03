#!/usr/bin/env python3
"""Compare two paper_report.json files produced by polymarket-exec
(typically: a baseline replay vs. a replay with one paper-fill knob
changed) and emit a side-by-side delta table.

Usage:
    polymarket-exec/scripts/compare_replay.py \\
        path/to/baseline.json \\
        path/to/variant.json [--output diff.md]

Phase 5 of the paper env design. The Rust replay binary
(PM_BTC_5M_EXEC_MODE=replay) writes the report; this script does the
comparison so iteration on knobs doesn't require recompilation.
"""

from __future__ import annotations

import argparse
import json
import sys
from pathlib import Path
from typing import Any


def load(path: Path) -> dict[str, Any]:
    with path.open("r", encoding="utf-8") as fh:
        return json.load(fh)


def fmt_pct_delta(baseline: float, variant: float) -> str:
    if baseline == 0:
        if variant == 0:
            return "0%"
        return "+inf%"
    delta = (variant - baseline) / abs(baseline) * 100.0
    sign = "+" if delta >= 0 else ""
    return f"{sign}{delta:.1f}%"


def fmt_abs(value: float | int) -> str:
    if isinstance(value, int):
        return str(value)
    if abs(value) >= 100:
        return f"{value:.2f}"
    if abs(value) >= 1:
        return f"{value:.4f}"
    return f"{value:.6f}"


def diff_row(label: str, base: Any, var: Any) -> tuple[str, str, str, str]:
    if isinstance(base, (int, float)) and isinstance(var, (int, float)):
        return (label, fmt_abs(base), fmt_abs(var), fmt_pct_delta(base, var))
    return (label, str(base), str(var), "n/a")


def emit(rows: list[tuple[str, str, str, str]], output: Path | None) -> None:
    header = ("metric", "baseline", "variant", "delta")
    widths = [
        max(len(r[i]) for r in [header] + rows) for i in range(4)
    ]
    line = " | ".join(header[i].ljust(widths[i]) for i in range(4))
    sep = "-+-".join("-" * widths[i] for i in range(4))
    body = [line, sep]
    for r in rows:
        body.append(" | ".join(r[i].ljust(widths[i]) for i in range(4)))
    rendered = "\n".join(body) + "\n"
    if output:
        output.write_text(rendered, encoding="utf-8")
        print(f"wrote {output}")
    else:
        sys.stdout.write(rendered)


def main() -> int:
    parser = argparse.ArgumentParser(description="Compare two paper_report.json files.")
    parser.add_argument("baseline", type=Path, help="Path to baseline paper_report.json")
    parser.add_argument("variant", type=Path, help="Path to variant paper_report.json")
    parser.add_argument(
        "--output",
        type=Path,
        default=None,
        help="Optional path to write the diff table (defaults to stdout).",
    )
    args = parser.parse_args()

    base = load(args.baseline)
    var = load(args.variant)

    rows: list[tuple[str, str, str, str]] = []

    # Session metadata (informational; no delta).
    rows.append(diff_row(
        "session.run_id",
        base.get("session", {}).get("run_id", "?"),
        var.get("session", {}).get("run_id", "?"),
    ))
    rows.append(diff_row(
        "session.mode",
        base.get("session", {}).get("mode", "?"),
        var.get("session", {}).get("mode", "?"),
    ))

    for k in ("total_count", "maker_count", "taker_count", "maker_fraction",
              "total_notional_usd", "total_fees_usd"):
        b = base.get("fills", {}).get(k, 0)
        v = var.get("fills", {}).get(k, 0)
        rows.append(diff_row(f"fills.{k}", b, v))

    for k in ("expected_edge_usd", "realized_edge_usd", "avg_slippage_bps"):
        b = base.get("edge", {}).get(k, 0)
        v = var.get("edge", {}).get(k, 0)
        rows.append(diff_row(f"edge.{k}", b, v))

    base_cap = base.get("edge", {}).get("edge_capture_ratio")
    var_cap = var.get("edge", {}).get("edge_capture_ratio")
    rows.append(diff_row(
        "edge.edge_capture_ratio",
        "n/a" if base_cap is None else base_cap,
        "n/a" if var_cap is None else var_cap,
    ))

    for k in ("post_only_reject_count", "late_fill_after_cancel_count"):
        b = base.get("queue", {}).get(k, 0)
        v = var.get("queue", {}).get(k, 0)
        rows.append(diff_row(f"queue.{k}", b, v))

    emit(rows, args.output)
    return 0


if __name__ == "__main__":
    sys.exit(main())
