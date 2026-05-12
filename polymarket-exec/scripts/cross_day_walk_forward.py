#!/usr/bin/env python3
"""Build cross-day walk-forward diagnostics from completed backtest artifacts.

This reducer intentionally does not read or transform market data. It consumes
the immutable outputs produced by `backtest_runner`:

- `runs/run_id=*/manifest.json`
- `runs/run_id=*/metrics_summary.json`

The output is a validation report, not a profitability claim. If any fold uses
`fill_quality=optimistic` or has rejected data-quality windows, the fold is
marked non-claimable.
"""

from __future__ import annotations

import argparse
import csv
import json
import re
from dataclasses import asdict, dataclass
from pathlib import Path
from typing import Any


RUN_DAY_RE = re.compile(r"dt=(\d{4}-\d{2}-\d{2})")


@dataclass(frozen=True)
class DayRun:
    day: str
    run_id: str
    run_dir: str
    fill_config: str
    total_pnl_usd: float
    portfolio_total_pnl_usd: float
    accepted_fills: int
    sharpe_per_window: float | None
    data_quality_rejects: int
    data_quality_warnings: int
    exploratory_fill_assumption: bool

    @property
    def claimable(self) -> bool:
        return not self.exploratory_fill_assumption and self.data_quality_rejects == 0


@dataclass(frozen=True)
class FoldReport:
    fold_index: int
    train_days: list[str]
    test_days: list[str]
    train_pnl_usd: float
    test_pnl_usd: float
    train_fills: int
    test_fills: int
    data_quality_rejects: int
    data_quality_warnings: int
    exploratory_fill_assumption: bool
    claimable: bool


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(
        description="Reduce completed day backtests into cross-day walk-forward diagnostics"
    )
    parser.add_argument("--output-root", required=True, type=Path)
    parser.add_argument("--report-json", type=Path)
    parser.add_argument("--report-csv", type=Path)
    parser.add_argument("--mode", choices=["expanding", "rolling"], default="expanding")
    parser.add_argument("--min-train-days", type=int, required=True)
    parser.add_argument("--test-days", type=int, required=True)
    parser.add_argument("--step-days", type=int, default=1)
    parser.add_argument("--holdout-days", type=int, required=True)
    return parser.parse_args()


def load_json(path: Path) -> dict[str, Any]:
    with path.open("r", encoding="utf-8") as f:
        return json.load(f)


def infer_day(run_id: str, manifest: dict[str, Any]) -> str:
    match = RUN_DAY_RE.search(run_id)
    if match:
        return match.group(1)
    windows = manifest.get("windows") or []
    starts = [w.get("start_ns") for w in windows if isinstance(w.get("start_ns"), int)]
    if starts:
        # Avoid importing pandas/pyarrow; derive UTC date from ns.
        import datetime as dt

        return dt.datetime.fromtimestamp(min(starts) / 1_000_000_000, tz=dt.UTC).date().isoformat()
    raise ValueError(f"cannot infer day for run_id={run_id}")


def quality_counts(manifest: dict[str, Any]) -> tuple[int, int]:
    rejects = 0
    warnings = 0
    for row in manifest.get("data_quality") or []:
        status = row.get("status")
        if status == "reject":
            rejects += 1
        elif status == "warn":
            warnings += 1
    return rejects, warnings


def scan_day_runs(output_root: Path) -> list[DayRun]:
    run_dirs = sorted((output_root / "runs").glob("run_id=*"))
    runs: list[DayRun] = []
    for run_dir in run_dirs:
        manifest_path = run_dir / "manifest.json"
        metrics_path = run_dir / "metrics_summary.json"
        if not manifest_path.exists() or not metrics_path.exists():
            continue
        manifest = load_json(manifest_path)
        metrics = load_json(metrics_path)
        run_id = str(manifest.get("run_id") or run_dir.name.removeprefix("run_id="))
        fill_config = str(metrics.get("fill_config") or manifest.get("fill_config") or "")
        rejects, warnings = quality_counts(manifest)
        runs.append(
            DayRun(
                day=infer_day(run_id, manifest),
                run_id=run_id,
                run_dir=str(run_dir),
                fill_config=fill_config,
                total_pnl_usd=float(metrics.get("total_pnl_usd") or 0.0),
                portfolio_total_pnl_usd=float(metrics.get("portfolio_total_pnl_usd") or 0.0),
                accepted_fills=int(metrics.get("accepted_fills") or 0),
                sharpe_per_window=metrics.get("sharpe_per_window"),
                data_quality_rejects=rejects,
                data_quality_warnings=warnings,
                exploratory_fill_assumption="optimistic" in fill_config,
            )
        )
    runs.sort(key=lambda r: (r.day, r.run_id))
    return runs


def validate_args(args: argparse.Namespace, runs: list[DayRun]) -> None:
    if args.min_train_days < 1:
        raise SystemExit("--min-train-days must be >= 1")
    if args.test_days < 1:
        raise SystemExit("--test-days must be >= 1")
    if args.step_days < 1:
        raise SystemExit("--step-days must be >= 1")
    if args.holdout_days < 1:
        raise SystemExit("--holdout-days must be >= 1")
    if len(runs) <= args.holdout_days:
        raise SystemExit("not enough completed runs for requested holdout")
    calibration = len(runs) - args.holdout_days
    if calibration < args.min_train_days + args.test_days:
        raise SystemExit("not enough non-holdout runs for one fold")


def aggregate(days: list[DayRun]) -> tuple[float, int, int, int, bool, bool]:
    pnl = sum(d.portfolio_total_pnl_usd for d in days)
    fills = sum(d.accepted_fills for d in days)
    rejects = sum(d.data_quality_rejects for d in days)
    warnings = sum(d.data_quality_warnings for d in days)
    exploratory = any(d.exploratory_fill_assumption for d in days)
    claimable = (not exploratory) and rejects == 0
    return pnl, fills, rejects, warnings, exploratory, claimable


def build_folds(runs: list[DayRun], args: argparse.Namespace) -> tuple[list[FoldReport], list[DayRun]]:
    calibration_len = len(runs) - args.holdout_days
    folds: list[FoldReport] = []
    train_end = args.min_train_days
    while train_end + args.test_days <= calibration_len:
        if args.mode == "expanding":
            train_start = 0
        else:
            train_start = max(0, train_end - args.min_train_days)
        train = runs[train_start:train_end]
        test = runs[train_end : train_end + args.test_days]
        train_pnl, train_fills, train_rejects, train_warnings, train_exploratory, _ = aggregate(train)
        test_pnl, test_fills, test_rejects, test_warnings, test_exploratory, test_claimable = aggregate(test)
        folds.append(
            FoldReport(
                fold_index=len(folds),
                train_days=[d.day for d in train],
                test_days=[d.day for d in test],
                train_pnl_usd=train_pnl,
                test_pnl_usd=test_pnl,
                train_fills=train_fills,
                test_fills=test_fills,
                data_quality_rejects=train_rejects + test_rejects,
                data_quality_warnings=train_warnings + test_warnings,
                exploratory_fill_assumption=train_exploratory or test_exploratory,
                claimable=test_claimable and train_rejects == 0 and not train_exploratory,
            )
        )
        train_end += args.step_days
    return folds, runs[calibration_len:]


def write_csv(path: Path, folds: list[FoldReport]) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    with path.open("w", encoding="utf-8", newline="") as f:
        writer = csv.DictWriter(
            f,
            fieldnames=[
                "fold_index",
                "train_days",
                "test_days",
                "train_pnl_usd",
                "test_pnl_usd",
                "train_fills",
                "test_fills",
                "data_quality_rejects",
                "data_quality_warnings",
                "exploratory_fill_assumption",
                "claimable",
            ],
        )
        writer.writeheader()
        for fold in folds:
            row = asdict(fold)
            row["train_days"] = ";".join(fold.train_days)
            row["test_days"] = ";".join(fold.test_days)
            writer.writerow(row)


def main() -> int:
    args = parse_args()
    runs = scan_day_runs(args.output_root)
    validate_args(args, runs)
    folds, holdout = build_folds(runs, args)
    holdout_pnl, holdout_fills, holdout_rejects, holdout_warnings, holdout_exploratory, holdout_claimable = aggregate(holdout)
    report = {
        "mode": args.mode,
        "input_output_root": str(args.output_root),
        "completed_run_count": len(runs),
        "folds": [asdict(fold) for fold in folds],
        "holdout": {
            "days": [d.day for d in holdout],
            "pnl_usd": holdout_pnl,
            "accepted_fills": holdout_fills,
            "data_quality_rejects": holdout_rejects,
            "data_quality_warnings": holdout_warnings,
            "exploratory_fill_assumption": holdout_exploratory,
            "claimable": holdout_claimable,
        },
        "claim_policy": {
            "claimable_requires": [
                "no optimistic fill-quality in train/test/holdout segment",
                "zero data-quality rejected windows",
            ],
            "note": "This report validates historical backtest artifacts only; it is not live-performance evidence.",
        },
    }
    report_json = args.report_json or (args.output_root / "walk_forward_report.json")
    report_csv = args.report_csv or (args.output_root / "walk_forward_folds.csv")
    report_json.parent.mkdir(parents=True, exist_ok=True)
    with report_json.open("w", encoding="utf-8") as f:
        json.dump(report, f, indent=2, sort_keys=True)
        f.write("\n")
    write_csv(report_csv, folds)
    print(f"wrote {report_json}")
    print(f"wrote {report_csv}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
