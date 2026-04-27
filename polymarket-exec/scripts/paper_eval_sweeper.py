#!/usr/bin/env python3
"""Shardable paper-eval runner for polymarket-exec.

This is the repo-local version of the ClawSweeper pattern: plan many
bounded paper-eval jobs, run each shard independently, then merge the
artifacts into one operator-readable report. It is intentionally
proposal/evidence only; it never promotes config values or touches live
execution state.
"""

from __future__ import annotations

import argparse
import json
import os
import subprocess
import sys
import time
from dataclasses import dataclass
from pathlib import Path
from typing import Any


ROOT = Path(__file__).resolve().parents[2]
DEFAULT_MATRIX = ROOT / "polymarket-exec" / "evals" / "paper_eval_matrix.json"
DEFAULT_PROMPT_PATTERNS = ROOT / "polymarket-exec" / "evals" / "prompt_anchoring_patterns.md"
COMPARE_SCRIPT = ROOT / "polymarket-exec" / "scripts" / "compare_replay.py"
SUGGEST_SCRIPT = ROOT / "polymarket-exec" / "scripts" / "suggest_paper_calibration.py"
STACK_TEST_SCRIPT = ROOT / "polymarket-exec" / "scripts" / "test_unlawful_stack.sh"


@dataclass(frozen=True)
class MetricRules:
    min_quality_fills: int
    min_maker_fraction: float
    min_edge_capture_ratio: float
    max_avg_slippage_bps: float
    max_maker_fraction_drop: float
    max_edge_capture_drop: float
    max_slippage_increase_bps: float


def load_json(path: Path) -> dict[str, Any]:
    with path.open("r", encoding="utf-8") as fh:
        return json.load(fh)


def write_json(path: Path, value: Any) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(json.dumps(value, indent=2, sort_keys=True) + "\n", encoding="utf-8")


def slug(value: str) -> str:
    safe = []
    for char in value.lower():
        if char.isalnum():
            safe.append(char)
        elif safe and safe[-1] != "-":
            safe.append("-")
    rendered = "".join(safe).strip("-")
    return rendered or "job"


def rel(path: Path) -> str:
    try:
        return str(path.resolve().relative_to(ROOT))
    except ValueError:
        return str(path)


def default_matrix() -> dict[str, Any]:
    return {
        "version": 1,
        "description": "Initial paper replay calibration matrix for BTC 5m unlawful_shear.",
        "rules": {
            "min_quality_fills": 3,
            "min_maker_fraction": 0.70,
            "min_edge_capture_ratio": 0.70,
            "max_avg_slippage_bps": 10.0,
            "max_maker_fraction_drop": 0.15,
            "max_edge_capture_drop": 0.25,
            "max_slippage_increase_bps": 10.0,
        },
        "replay_variants": [
            {
                "name": "baseline",
                "description": "Current env defaults.",
                "env": {},
            },
            {
                "name": "queue_depth_050",
                "description": "Less pessimistic queue position.",
                "env": {"WHALE_PAIR_PAPER_QUEUE_DEPTH_FRACTION": "0.50"},
            },
            {
                "name": "queue_depth_085",
                "description": "More conservative queue position.",
                "env": {"WHALE_PAIR_PAPER_QUEUE_DEPTH_FRACTION": "0.85"},
            },
            {
                "name": "submit_latency_080",
                "description": "Faster simulated submit ack.",
                "env": {"WHALE_PAIR_PAPER_SUBMIT_LATENCY_MS": "80"},
            },
            {
                "name": "submit_latency_250",
                "description": "Slower simulated submit ack.",
                "env": {"WHALE_PAIR_PAPER_SUBMIT_LATENCY_MS": "250"},
            },
            {
                "name": "post_only_reject_060",
                "description": "Lower post-only crossing rejection probability.",
                "env": {"WHALE_PAIR_PAPER_POST_ONLY_REJECT_PROBABILITY": "0.60"},
            },
            {
                "name": "post_only_reject_095",
                "description": "Higher post-only crossing rejection probability.",
                "env": {"WHALE_PAIR_PAPER_POST_ONLY_REJECT_PROBABILITY": "0.95"},
            },
            {
                "name": "cancel_race_1000",
                "description": "Longer late-fill-after-cancel window.",
                "env": {"WHALE_PAIR_PAPER_CANCEL_RACE_WINDOW_MS": "1000"},
            },
            {
                "name": "maker_rebate_v2_proxy",
                "description": "Non-zero maker rebate sensitivity check.",
                "env": {"WHALE_PAIR_PAPER_MAKER_REBATE_COEFF": "0.01"},
            },
        ],
        "test_jobs": [
            {
                "name": "unlawful_stack",
                "description": "Focused gate, config, structured logging, and scenario tests.",
                "command": ["bash", "polymarket-exec/scripts/test_unlawful_stack.sh"],
            }
        ],
    }


def rules_from_matrix(matrix: dict[str, Any]) -> MetricRules:
    raw = matrix.get("rules", {})
    return MetricRules(
        min_quality_fills=int(raw.get("min_quality_fills", 3)),
        min_maker_fraction=float(raw.get("min_maker_fraction", 0.70)),
        min_edge_capture_ratio=float(raw.get("min_edge_capture_ratio", 0.70)),
        max_avg_slippage_bps=float(raw.get("max_avg_slippage_bps", 10.0)),
        max_maker_fraction_drop=float(raw.get("max_maker_fraction_drop", 0.15)),
        max_edge_capture_drop=float(raw.get("max_edge_capture_drop", 0.25)),
        max_slippage_increase_bps=float(raw.get("max_slippage_increase_bps", 10.0)),
    )


def metric(report: dict[str, Any], path: str, default: Any = 0) -> Any:
    value: Any = report
    for part in path.split("."):
        if not isinstance(value, dict):
            return default
        value = value.get(part, default)
    return value


def evaluate_report(
    report: dict[str, Any],
    baseline: dict[str, Any] | None,
    rules: MetricRules,
) -> tuple[str, list[str]]:
    findings: list[str] = []
    total_fills = int(metric(report, "fills.total_count", 0) or 0)
    maker_fraction = float(metric(report, "fills.maker_fraction", 0.0) or 0.0)
    avg_slippage = float(metric(report, "edge.avg_slippage_bps", 0.0) or 0.0)
    edge_capture = metric(report, "edge.edge_capture_ratio", None)

    status = "pass"
    if total_fills >= rules.min_quality_fills:
        if maker_fraction < rules.min_maker_fraction:
            status = "fail"
            findings.append(
                f"maker_fraction {maker_fraction:.3f} below {rules.min_maker_fraction:.3f}"
            )
        if isinstance(edge_capture, (int, float)) and edge_capture < rules.min_edge_capture_ratio:
            status = "fail"
            findings.append(
                f"edge_capture_ratio {edge_capture:.3f} below {rules.min_edge_capture_ratio:.3f}"
            )
        if avg_slippage > rules.max_avg_slippage_bps:
            status = "fail"
            findings.append(
                f"avg_slippage_bps {avg_slippage:.2f} above {rules.max_avg_slippage_bps:.2f}"
            )
    else:
        status = "warn"
        findings.append(
            f"only {total_fills} fills; below quality threshold {rules.min_quality_fills}"
        )

    if baseline is not None and total_fills >= rules.min_quality_fills:
        base_maker = float(metric(baseline, "fills.maker_fraction", 0.0) or 0.0)
        base_slippage = float(metric(baseline, "edge.avg_slippage_bps", 0.0) or 0.0)
        base_capture = metric(baseline, "edge.edge_capture_ratio", None)
        if base_maker - maker_fraction > rules.max_maker_fraction_drop:
            status = "fail"
            findings.append(
                f"maker_fraction dropped {base_maker - maker_fraction:.3f} vs baseline"
            )
        if avg_slippage - base_slippage > rules.max_slippage_increase_bps:
            status = "fail"
            findings.append(
                f"avg_slippage_bps increased {avg_slippage - base_slippage:.2f} vs baseline"
            )
        if isinstance(base_capture, (int, float)) and isinstance(edge_capture, (int, float)):
            if base_capture - edge_capture > rules.max_edge_capture_drop:
                status = "fail"
                findings.append(
                    f"edge_capture_ratio dropped {base_capture - edge_capture:.3f} vs baseline"
                )

    if not findings:
        findings.append("metrics inside configured gates")
    return status, findings


def report_summary(report: dict[str, Any]) -> dict[str, Any]:
    return {
        "total_fills": metric(report, "fills.total_count", 0),
        "maker_fraction": metric(report, "fills.maker_fraction", 0.0),
        "total_notional_usd": metric(report, "fills.total_notional_usd", 0.0),
        "edge_capture_ratio": metric(report, "edge.edge_capture_ratio", None),
        "avg_slippage_bps": metric(report, "edge.avg_slippage_bps", 0.0),
        "post_only_reject_count": metric(report, "queue.post_only_reject_count", 0),
        "late_fill_after_cancel_count": metric(report, "queue.late_fill_after_cancel_count", 0),
        "notional_capture_ratio": metric(report, "vs_whale.notional_capture_ratio", None),
    }


def infer_assets_from_snapshot(path: Path, limit: int = 1000) -> list[str]:
    assets: list[str] = []
    seen: set[str] = set()
    if not path.exists():
        return assets
    with path.open("r", encoding="utf-8") as fh:
        for idx, line in enumerate(fh):
            if idx >= limit:
                break
            stripped = line.strip()
            if not stripped:
                continue
            try:
                asset = json.loads(stripped).get("asset")
            except json.JSONDecodeError:
                continue
            if isinstance(asset, str) and asset and asset not in seen:
                seen.add(asset)
                assets.append(asset)
    return assets


def replay_config_env(snapshot: Path) -> dict[str, str]:
    env: dict[str, str] = {}
    if not os.environ.get("WHALE_PAIR_EXEC_STARTING_CASH_USD"):
        env["WHALE_PAIR_EXEC_STARTING_CASH_USD"] = "1000"
    assets = infer_assets_from_snapshot(snapshot)
    if assets and not os.environ.get("WHALE_PAIR_ASSET_IDS"):
        env["WHALE_PAIR_ASSET_IDS"] = ",".join(assets)
    if assets and not os.environ.get("WHALE_PAIR_INSTRUMENT_MARKETS"):
        env["WHALE_PAIR_INSTRUMENT_MARKETS"] = ",".join(
            f"{asset}:replay-market" for asset in assets
        )
    return env


def run_command(
    command: list[str],
    cwd: Path,
    env: dict[str, str],
    timeout_seconds: int,
    stdout_path: Path,
    stderr_path: Path,
    dry_run: bool,
) -> tuple[int, float]:
    stdout_path.parent.mkdir(parents=True, exist_ok=True)
    if dry_run:
        stdout_path.write_text(
            "DRY RUN: " + " ".join(command) + "\n", encoding="utf-8"
        )
        stderr_path.write_text("", encoding="utf-8")
        return 0, 0.0
    started = time.monotonic()
    with stdout_path.open("w", encoding="utf-8") as out, stderr_path.open(
        "w", encoding="utf-8"
    ) as err:
        proc = subprocess.run(
            command,
            cwd=cwd,
            env=env,
            stdout=out,
            stderr=err,
            text=True,
            timeout=timeout_seconds,
            check=False,
        )
    return proc.returncode, time.monotonic() - started


def command_to_text(command: list[str], env: dict[str, str] | None = None) -> str:
    prefix = ""
    if env:
        prefix = " ".join(f"{k}={v}" for k, v in sorted(env.items())) + " "
    return prefix + " ".join(command)


def render_agent_prompt(job: dict[str, Any], plan: dict[str, Any]) -> str:
    patterns_path = Path(plan.get("prompt_patterns_path") or DEFAULT_PROMPT_PATTERNS)
    patterns = (
        patterns_path.read_text(encoding="utf-8")
        if patterns_path.exists()
        else "No prompt anchoring pattern file found."
    )
    command = ""
    if job["type"] == "replay":
        config_env = replay_config_env(Path(job["snapshot"]))
        env = {
            "WHALE_PAIR_EXEC_MODE": "replay",
            "WHALE_PAIR_REPLAY_INPUT_PATH": job["snapshot"],
            "WHALE_PAIR_PAPER_REPORT_PATH": job["report_path"],
            **config_env,
            **{str(k): str(v) for k, v in job.get("env", {}).items()},
        }
        command = command_to_text(["cargo", "run", "-p", "polymarket-exec"], env)
    elif job["type"] == "test":
        command = command_to_text([str(part) for part in job["command"]])
    return f"""# Paper Eval Shard Prompt

## Job

- id: `{job["id"]}`
- type: `{job["type"]}`
- name: `{job["name"]}`
- description: {job.get("description", "")}

## Command

```bash
{command}
```

## Required Output Discipline

- Anchor every conclusion to the generated artifacts: `paper_report.json`, `diff.md`, `suggestions.json`, `stdout.log`, `stderr.log`, or the failing test output.
- Separate capture-quality problems from execution-engine problems.
- Do not propose a code fix unless there is a reproducible failing command or a report metric regression tied to a specific code path.
- If a code fix is proposed, state the exact failing behavior, the smallest likely code surface, and the test/replay command that should prove it.
- Do not promote env defaults or strategy config from a single shard.

## Prompting Patterns Reference

{patterns}
"""


def make_plan(args: argparse.Namespace) -> int:
    matrix_path = Path(args.matrix)
    if not matrix_path.exists():
        raise SystemExit(f"matrix not found: {matrix_path}")
    matrix = load_json(matrix_path)
    out_dir = Path(args.out_dir)
    snapshot = Path(args.snapshot).resolve() if args.snapshot else None
    baseline_report = Path(args.baseline_report).resolve() if args.baseline_report else None

    jobs: list[dict[str, Any]] = []
    if snapshot is not None:
        for idx, variant in enumerate(matrix.get("replay_variants", [])):
            name = str(variant["name"])
            job_id = f"{idx:03d}-{slug(name)}"
            report_path = out_dir / "jobs" / job_id / "paper_report.json"
            jobs.append(
                {
                    "id": job_id,
                    "type": "replay",
                    "name": name,
                    "description": variant.get("description", ""),
                    "snapshot": str(snapshot),
                    "env": variant.get("env", {}),
                    "report_path": str(report_path),
                    "baseline": bool(name == "baseline"),
                }
            )

    if args.include_tests:
        offset = len(jobs)
        for idx, test in enumerate(matrix.get("test_jobs", []), start=offset):
            name = str(test["name"])
            jobs.append(
                {
                    "id": f"{idx:03d}-{slug(name)}",
                    "type": "test",
                    "name": name,
                    "description": test.get("description", ""),
                    "command": test["command"],
                }
            )

    if not jobs:
        raise SystemExit("plan has no jobs; pass --snapshot and/or --include-tests")

    shard_count = max(1, int(args.shard_count))
    shards = [{"index": i, "job_ids": []} for i in range(shard_count)]
    for idx, job in enumerate(jobs):
        shards[idx % shard_count]["job_ids"].append(job["id"])

    plan = {
        "created_at_ms": int(time.time() * 1000),
        "root": str(ROOT),
        "matrix_path": str(matrix_path.resolve()),
        "out_dir": str(out_dir.resolve()),
        "snapshot": str(snapshot) if snapshot else None,
        "external_baseline_report": str(baseline_report) if baseline_report else None,
        "prompt_patterns_path": str(Path(args.prompt_patterns).resolve()),
        "baseline_job_id": next(
            (job["id"] for job in jobs if job.get("baseline")), None
        ),
        "timeout_seconds": int(args.timeout_seconds),
        "rules": matrix.get("rules", {}),
        "jobs": jobs,
        "shards": shards,
    }
    write_json(out_dir / "plan.json", plan)
    for shard in shards:
        write_json(out_dir / "shards" / f"shard-{shard['index']}.json", shard)
    print(f"planned {len(jobs)} jobs across {shard_count} shards -> {out_dir / 'plan.json'}")
    return 0


def run_job(
    job: dict[str, Any],
    plan: dict[str, Any],
    baseline_report_path: Path | None,
    rules: MetricRules,
    dry_run: bool,
) -> dict[str, Any]:
    out_dir = Path(plan["out_dir"])
    job_dir = out_dir / "jobs" / job["id"]
    job_dir.mkdir(parents=True, exist_ok=True)
    timeout_seconds = int(plan.get("timeout_seconds", 900))
    result: dict[str, Any] = {
        "job_id": job["id"],
        "name": job["name"],
        "type": job["type"],
        "description": job.get("description", ""),
        "started_at_ms": int(time.time() * 1000),
    }
    (job_dir / "agent_prompt.md").write_text(
        render_agent_prompt(job, plan),
        encoding="utf-8",
    )

    if job["type"] == "replay":
        report_path = Path(job["report_path"])
        config_env = replay_config_env(Path(job["snapshot"]))
        env_overrides = {
            "WHALE_PAIR_EXEC_MODE": "replay",
            "WHALE_PAIR_REPLAY_INPUT_PATH": job["snapshot"],
            "WHALE_PAIR_PAPER_REPORT_PATH": str(report_path),
            **config_env,
            **{str(k): str(v) for k, v in job.get("env", {}).items()},
        }
        env = os.environ.copy()
        env.update(env_overrides)
        command = ["cargo", "run", "-p", "polymarket-exec"]
        result["command"] = command_to_text(command, env_overrides)
        code, elapsed = run_command(
            command,
            ROOT,
            env,
            timeout_seconds,
            job_dir / "stdout.log",
            job_dir / "stderr.log",
            dry_run,
        )
        result["returncode"] = code
        result["elapsed_seconds"] = elapsed
        result["report_path"] = str(report_path)
        if dry_run:
            result["status"] = "dry_run"
            result["findings"] = ["command rendered but not executed"]
        elif code != 0:
            result["status"] = "error"
            result["findings"] = [f"command exited {code}"]
        elif not report_path.exists():
            result["status"] = "error"
            result["findings"] = [f"missing report {report_path}"]
        else:
            report = load_json(report_path)
            baseline = (
                load_json(baseline_report_path)
                if baseline_report_path and baseline_report_path.exists()
                else None
            )
            status, findings = evaluate_report(report, baseline, rules)
            if baseline_report_path and not baseline_report_path.exists():
                if status == "pass":
                    status = "warn"
                findings.append(f"baseline report not available yet: {baseline_report_path}")
            result["status"] = status
            result["findings"] = findings
            result["metrics"] = report_summary(report)
            if baseline_report_path and baseline_report_path.exists() and baseline_report_path != report_path:
                run_command(
                    [
                        sys.executable,
                        str(COMPARE_SCRIPT),
                        str(baseline_report_path),
                        str(report_path),
                        "--output",
                        str(job_dir / "diff.md"),
                    ],
                    ROOT,
                    os.environ.copy(),
                    120,
                    job_dir / "compare.stdout.log",
                    job_dir / "compare.stderr.log",
                    False,
                )
            run_command(
                [
                    sys.executable,
                    str(SUGGEST_SCRIPT),
                    str(report_path),
                    "--output",
                    str(job_dir / "suggestions.json"),
                ],
                ROOT,
                os.environ.copy(),
                120,
                job_dir / "suggest.stdout.log",
                job_dir / "suggest.stderr.log",
                False,
            )
    elif job["type"] == "test":
        command = [str(part) for part in job["command"]]
        result["command"] = command_to_text(command)
        code, elapsed = run_command(
            command,
            ROOT,
            os.environ.copy(),
            timeout_seconds,
            job_dir / "stdout.log",
            job_dir / "stderr.log",
            dry_run,
        )
        result["returncode"] = code
        result["elapsed_seconds"] = elapsed
        result["status"] = "dry_run" if dry_run else ("pass" if code == 0 else "error")
        result["findings"] = (
            ["command rendered but not executed"]
            if dry_run
            else (["test command passed"] if code == 0 else [f"test command exited {code}"])
        )
    else:
        result["status"] = "error"
        result["findings"] = [f"unknown job type {job['type']}"]

    result["ended_at_ms"] = int(time.time() * 1000)
    write_json(job_dir / "result.json", result)
    return result


def run_shard(args: argparse.Namespace) -> int:
    plan = load_json(Path(args.plan))
    rules = rules_from_matrix({"rules": plan.get("rules", {})})
    jobs_by_id = {job["id"]: job for job in plan["jobs"]}
    shard_index = int(args.shard_index)
    shard = next((s for s in plan["shards"] if int(s["index"]) == shard_index), None)
    if shard is None:
        raise SystemExit(f"shard {shard_index} not found in plan")

    external_baseline = plan.get("external_baseline_report")
    baseline_path: Path | None = Path(external_baseline) if external_baseline else None
    if baseline_path is None and plan.get("baseline_job_id"):
        baseline_job = jobs_by_id[plan["baseline_job_id"]]
        baseline_path = Path(baseline_job["report_path"])

    results = []
    max_jobs = int(args.max_jobs) if args.max_jobs else None
    for job_id in shard["job_ids"][:max_jobs]:
        job = jobs_by_id[job_id]
        print(f"[paper-eval] shard={shard_index} job={job_id} {job['name']}")
        results.append(run_job(job, plan, baseline_path, rules, args.dry_run))
    out_dir = Path(plan["out_dir"])
    write_json(out_dir / "shards" / f"shard-{shard_index}-result.json", results)
    failures = [r for r in results if r.get("status") in {"error", "fail"}]
    return 1 if failures else 0


def score_result(result: dict[str, Any]) -> tuple[int, float, float]:
    status_rank = {"pass": 0, "warn": 1, "dry_run": 2, "fail": 3, "error": 4}
    metrics = result.get("metrics", {})
    edge_capture = metrics.get("edge_capture_ratio")
    maker_fraction = metrics.get("maker_fraction") or 0.0
    edge_score = float(edge_capture) if isinstance(edge_capture, (int, float)) else -1.0
    return (status_rank.get(result.get("status"), 9), -edge_score, -float(maker_fraction))


def merge(args: argparse.Namespace) -> int:
    out_dir = Path(args.out_dir)
    plan = load_json(out_dir / "plan.json")
    results = []
    for job in plan["jobs"]:
        result_path = out_dir / "jobs" / job["id"] / "result.json"
        if result_path.exists():
            results.append(load_json(result_path))
        else:
            results.append(
                {
                    "job_id": job["id"],
                    "name": job["name"],
                    "type": job["type"],
                    "status": "missing",
                    "findings": ["job result not found"],
                }
            )
    results.sort(key=score_result)
    write_json(out_dir / "summary.json", {"plan": rel(out_dir / "plan.json"), "results": results})

    lines = [
        "# Paper Eval Sweep Summary",
        "",
        f"- Plan: `{rel(out_dir / 'plan.json')}`",
        f"- Jobs: {len(results)}",
        "",
        "| status | job | type | fills | maker | edge_capture | slippage_bps | findings |",
        "|---|---|---:|---:|---:|---:|---:|---|",
    ]
    for result in results:
        metrics = result.get("metrics", {})
        findings = "; ".join(result.get("findings", []))
        lines.append(
            "| {status} | `{job}` | {typ} | {fills} | {maker} | {edge} | {slip} | {findings} |".format(
                status=result.get("status", "?"),
                job=result.get("name", result.get("job_id", "?")),
                typ=result.get("type", "?"),
                fills=metrics.get("total_fills", ""),
                maker=fmt_metric(metrics.get("maker_fraction")),
                edge=fmt_metric(metrics.get("edge_capture_ratio")),
                slip=fmt_metric(metrics.get("avg_slippage_bps")),
                findings=findings.replace("|", "/"),
            )
        )
    lines.append("")
    lines.append("## Commands")
    lines.append("")
    for result in results:
        command = result.get("command")
        if command:
            lines.append(f"- `{result.get('name')}`: `{command}`")
    (out_dir / "summary.md").write_text("\n".join(lines) + "\n", encoding="utf-8")
    print(f"wrote {out_dir / 'summary.md'}")
    return 0


def fmt_metric(value: Any) -> str:
    if value is None:
        return ""
    if isinstance(value, float):
        return f"{value:.4f}"
    return str(value)


def init_matrix(args: argparse.Namespace) -> int:
    output = Path(args.output)
    if output.exists() and not args.force:
        raise SystemExit(f"{output} already exists; pass --force to overwrite")
    write_json(output, default_matrix())
    print(f"wrote {output}")
    return 0


def capture_command(args: argparse.Namespace) -> int:
    out_dir = Path(args.out_dir)
    snapshot = out_dir / "capture" / "session.snap.jsonl"
    report = out_dir / "capture" / "baseline.report.json"
    print("set -a")
    print("source polymarket-exec/env/btc_5m_mm_shadowlive.env")
    print("export WHALE_PAIR_EXEC_STARTING_CASH_USD=1000")
    print("export WHALE_PAIR_EXEC_MAX_SESSION_LOSS_BPS=500")
    print(f"export WHALE_PAIR_BOOK_SNAPSHOT_LOG_PATH={snapshot}")
    print(f"export WHALE_PAIR_PAPER_REPORT_PATH={report}")
    print("set +a")
    print("cargo run -p polymarket-exec")
    return 0


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    sub = parser.add_subparsers(dest="cmd", required=True)

    p = sub.add_parser("init-matrix", help="Write the default paper eval matrix JSON.")
    p.add_argument("--output", type=Path, default=DEFAULT_MATRIX)
    p.add_argument("--force", action="store_true")
    p.set_defaults(func=init_matrix)

    p = sub.add_parser("capture-command", help="Print a shadow_live capture command block.")
    p.add_argument("--out-dir", required=True, type=Path)
    p.set_defaults(func=capture_command)

    p = sub.add_parser("plan", help="Create a shardable paper eval plan.")
    p.add_argument("--snapshot", type=Path, default=None, help="Book snapshot JSONL to replay.")
    p.add_argument("--baseline-report", type=Path, default=None)
    p.add_argument("--matrix", type=Path, default=DEFAULT_MATRIX)
    p.add_argument("--prompt-patterns", type=Path, default=DEFAULT_PROMPT_PATTERNS)
    p.add_argument("--out-dir", required=True, type=Path)
    p.add_argument("--shard-count", type=int, default=10)
    p.add_argument("--timeout-seconds", type=int, default=900)
    p.add_argument("--include-tests", action="store_true")
    p.set_defaults(func=make_plan)

    p = sub.add_parser("run-shard", help="Run one shard from a plan.")
    p.add_argument("--plan", required=True, type=Path)
    p.add_argument("--shard-index", required=True, type=int)
    p.add_argument("--max-jobs", type=int, default=None)
    p.add_argument("--dry-run", action="store_true")
    p.set_defaults(func=run_shard)

    p = sub.add_parser("merge", help="Merge job results into summary artifacts.")
    p.add_argument("--out-dir", required=True, type=Path)
    p.set_defaults(func=merge)

    args = parser.parse_args()
    return args.func(args)


if __name__ == "__main__":
    sys.exit(main())
