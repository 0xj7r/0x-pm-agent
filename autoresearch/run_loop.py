"""Autoresearch optimization loop: 50 iterations of mutation + backtest."""
from __future__ import annotations

import copy
import json
import random
import subprocess
import sys
from datetime import datetime, timezone
from pathlib import Path

CONFIG_PATH = Path("strategy_config.json")
BACKUP_PATH = Path("strategy_config.backup.json")
RESULTS_PATH = Path("autoresearch/results.tsv")
BACKTEST_CMD = [".venv/bin/python", "autoresearch/run_backtest.py", "--config", "strategy_config.json", "--json"]

APPROACHES = [
    {
        "name": "early_entry_high_weights",
        "desc": "Early entry with high weights to detect BTC move before Polymarket reprices",
        "config": {
            "signal.w1_order_flow": (5.0, 8.0),
            "signal.w3_price_delta": (10.0, 15.0),
            "signal.w4_acceleration": (2.0, 5.0),
            "signal.confidence_threshold": (0.53, 0.60),
            "execution.enable_early_snipe": True,
            "execution.enable_late_snipe": False,
            "execution.enable_midrange": False,
            "execution.entry_window_early": [0, 60],
            "execution.max_entry_price": (0.10, 0.55),
        },
    },
    {
        "name": "late_snipe_cheap_tokens",
        "desc": "Late snipe with cheap tokens only, high signal accuracy",
        "config": {
            "signal.w1_order_flow": (4.0, 6.0),
            "signal.w3_price_delta": (8.0, 12.0),
            "signal.w4_acceleration": (1.0, 4.0),
            "signal.confidence_threshold": (0.55, 0.65),
            "execution.enable_early_snipe": False,
            "execution.enable_late_snipe": True,
            "execution.enable_midrange": False,
            "execution.entry_window_late": [200, 295],
            "execution.max_entry_price": (0.05, 0.10),
        },
    },
    {
        "name": "full_window_midrange",
        "desc": "Full window with midrange entry at moderate confidence",
        "config": {
            "signal.w1_order_flow": (3.0, 8.0),
            "signal.w3_price_delta": (6.0, 12.0),
            "signal.w4_acceleration": (1.0, 4.0),
            "signal.confidence_threshold": (0.55, 0.70),
            "execution.enable_early_snipe": True,
            "execution.enable_late_snipe": True,
            "execution.enable_midrange": True,
            "execution.entry_window_early": [0, 295],
            "execution.midrange_min_confidence": (0.55, 0.70),
            "execution.midrange_max_price": (0.30, 0.50),
            "execution.max_entry_price": (0.15, 0.45),
        },
    },
    {
        "name": "ultra_high_weights_very_early",
        "desc": "Ultra-high weights with very early window (0-30s)",
        "config": {
            "signal.w1_order_flow": (8.0, 12.0),
            "signal.w3_price_delta": (15.0, 20.0),
            "signal.w4_acceleration": (3.0, 8.0),
            "signal.confidence_threshold": (0.51, 0.55),
            "execution.enable_early_snipe": True,
            "execution.enable_late_snipe": False,
            "execution.enable_midrange": False,
            "execution.entry_window_early": [0, 30],
            "execution.max_entry_price": (0.20, 0.55),
        },
    },
    {
        "name": "dual_window_snipe",
        "desc": "Both early and late windows enabled, no midrange",
        "config": {
            "signal.w1_order_flow": (5.0, 10.0),
            "signal.w3_price_delta": (8.0, 15.0),
            "signal.w4_acceleration": (2.0, 6.0),
            "signal.confidence_threshold": (0.53, 0.60),
            "execution.enable_early_snipe": True,
            "execution.enable_late_snipe": True,
            "execution.enable_midrange": False,
            "execution.entry_window_early": [0, 60],
            "execution.entry_window_late": [200, 295],
            "execution.max_entry_price": (0.05, 0.15),
        },
    },
]


def load_config() -> dict:
    return json.loads(CONFIG_PATH.read_text())


def save_config(cfg: dict) -> None:
    CONFIG_PATH.write_text(json.dumps(cfg, indent=4) + "\n")


def set_nested(cfg: dict, key: str, value: object) -> None:
    parts = key.split(".")
    d = cfg
    for p in parts[:-1]:
        d = d[p]
    d[parts[-1]] = value


def get_nested(cfg: dict, key: str) -> object:
    parts = key.split(".")
    d = cfg
    for p in parts:
        d = d[p]
    return d


def run_backtest() -> dict:
    result = subprocess.run(BACKTEST_CMD, capture_output=True, text=True)
    if result.returncode != 0:
        print(f"  BACKTEST ERROR: {result.stderr}", file=sys.stderr)
        return {"ev_per_trade": 0, "num_trades": 0, "max_drawdown": 0, "win_rate": 0, "sharpe": 0}
    for line in result.stdout.strip().split("\n"):
        line = line.strip()
        if line.startswith("{"):
            return json.loads(line)
    return {"ev_per_trade": 0, "num_trades": 0, "max_drawdown": 0, "win_rate": 0, "sharpe": 0}


def composite_score(r: dict) -> float:
    ev = r.get("ev_per_trade", 0)
    n = r.get("num_trades", 0)
    dd = r.get("max_drawdown", 0)
    return ev * min(n, 20) / 20 - 0.5 * dd


def enforce_invariants(cfg: dict) -> None:
    km = cfg["risk"]["kelly_multiplier"]
    ct = cfg["risk"]["cheap_token_multiplier"]
    if km * ct > 2.0:
        cfg["risk"]["cheap_token_multiplier"] = min(ct, 2.0 / km)


def apply_approach(cfg: dict, approach: dict) -> str:
    mutations = []
    for key, spec in approach["config"].items():
        old_val = get_nested(cfg, key)
        if isinstance(spec, tuple):
            lo, hi = spec
            new_val = round(random.uniform(lo, hi), 4)
            if isinstance(old_val, int):
                new_val = int(new_val)
        elif isinstance(spec, list):
            new_val = spec
        elif isinstance(spec, bool):
            new_val = spec
        else:
            new_val = spec
        set_nested(cfg, key, new_val)
        mutations.append(f"{key}={old_val}->{new_val}")
    return "; ".join(mutations)


def mutate_current(cfg: dict, best_cfg: dict) -> tuple[str, str, str]:
    """Single-field mutation of the current best config."""
    cfg_copy = copy.deepcopy(best_cfg)
    for k, v in best_cfg.items():
        if isinstance(v, dict):
            for k2, v2 in v.items():
                cfg[k][k2] = copy.deepcopy(v2) if isinstance(v2, (list, dict)) else v2

    fields = [
        ("signal.w1_order_flow", 0.0, 20.0),
        ("signal.w3_price_delta", 0.0, 20.0),
        ("signal.w4_acceleration", 0.0, 10.0),
        ("signal.confidence_threshold", 0.51, 0.95),
        ("signal.prior", 0.45, 0.55),
        ("execution.max_entry_price", 0.01, 0.60),
        ("risk.kelly_multiplier", 0.05, 1.0),
        ("risk.cheap_token_multiplier", 1.0, 5.0),
        ("execution.midrange_min_confidence", 0.51, 0.95),
        ("execution.midrange_max_price", 0.10, 0.60),
    ]
    field, lo, hi = random.choice(fields)
    old_val = get_nested(cfg, field)
    delta = (hi - lo) * random.uniform(-0.15, 0.15)
    new_val = round(max(lo, min(hi, old_val + delta)), 4)
    set_nested(cfg, field, new_val)
    return "tweak_best", f"Tweaking {field} on current best", f"{field}={old_val}->{new_val}"


def append_result(
    iteration: int, approach: str, mutation: str, score: float, r: dict, kept: bool
) -> None:
    ts = datetime.now(timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ")
    line = (
        f"{ts}\t{iteration}\t{approach}\t{mutation}\t"
        f"{score:.6f}\t{r.get('ev_per_trade', 0):.4f}\t"
        f"{r.get('num_trades', 0)}\t{r.get('win_rate', 0):.4f}\t"
        f"{r.get('sharpe', 0):.4f}\t{r.get('max_drawdown', 0):.4f}\t"
        f"{'yes' if kept else 'no'}\n"
    )
    with open(RESULTS_PATH, "a") as f:
        f.write(line)


def main() -> None:
    random.seed(42)
    iterations = 50

    # Save original as backup
    original_cfg = load_config()
    BACKUP_PATH.write_text(json.dumps(original_cfg, indent=4) + "\n")

    # Baseline
    print("BASELINE: Running initial backtest...")
    baseline_result = run_backtest()
    baseline_score = composite_score(baseline_result)
    print(f"  Baseline score: {baseline_score:.6f} ({baseline_result})")

    best_score = baseline_score
    best_cfg = copy.deepcopy(original_cfg)
    no_improve_streak = 0

    for i in range(1, iterations + 1):
        cfg = load_config()

        # Every 5th iteration or after 5 failures: try a new approach
        if i % 5 == 1 or no_improve_streak >= 5:
            approach = random.choice(APPROACHES)
            # Reset to best config first
            cfg = copy.deepcopy(best_cfg)
            mutation_desc = apply_approach(cfg, approach)
            approach_name = approach["name"]
            reasoning = approach["desc"]
        else:
            # Tweak current best
            cfg = copy.deepcopy(best_cfg)
            approach_name, reasoning, mutation_desc = mutate_current(cfg, best_cfg)

        enforce_invariants(cfg)
        save_config(cfg)

        result = run_backtest()
        score = composite_score(result)

        kept = score > best_score
        if kept:
            best_score = score
            best_cfg = copy.deepcopy(cfg)
            no_improve_streak = 0
        else:
            # Revert
            save_config(best_cfg)
            no_improve_streak += 1

        append_result(i, approach_name, mutation_desc, score, result, kept)

        status = "KEPT" if kept else "REVERTED"
        print(
            f"ITERATION {i}: [{approach_name}] score={score:.6f} "
            f"(best={best_score:.6f}) trades={result.get('num_trades', 0)} "
            f"wr={result.get('win_rate', 0):.2%} ev={result.get('ev_per_trade', 0):.4f} "
            f"-> {status}"
        )

    # Ensure best config is saved at the end
    save_config(best_cfg)
    print(f"\nDONE. Best score: {best_score:.6f}")
    print(f"Results logged to {RESULTS_PATH}")


if __name__ == "__main__":
    main()
