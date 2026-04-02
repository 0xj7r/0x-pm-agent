"""Validate current strategies for paper-trading readiness.

Runs validation on top of the materialized feature store using:
- expanding walk-forward folds
- untouched final holdout
- nearby-parameter robustness checks
- execution stress scenarios

Usage:
    python backtesting/validate_strategy_readiness.py
    python backtesting/validate_strategy_readiness.py --coin btc --folds 5
"""
from __future__ import annotations

import argparse
import json
import sys
from dataclasses import dataclass
from pathlib import Path
from types import SimpleNamespace

sys.path.insert(0, str(Path(__file__).parent.parent))

from backtesting.feature_store import MarketMeta, load_feature_store
from shared.fees import taker_fee
from strategies.registry import build_check_fn

BASE_DIR = Path(__file__).parent
STRATEGY_RESULTS = BASE_DIR / "strategy_results.json"
OUTPUT = BASE_DIR / "validation_report.json"


@dataclass(frozen=True)
class StressScenario:
    name: str
    fee_multiplier: float = 1.0
    entry_slippage: float = 0.0
    entry_delay: int = 0


def _slice_market(meta: MarketMeta, features: dict) -> SimpleNamespace:
    start = meta.offset
    end = start + meta.length
    return SimpleNamespace(
        market_id=meta.market_id,
        winner=meta.winner,
        num_snaps=meta.length,
        move_pct=features["move_pct"][start:end],
        abs_move=features["abs_move"][start:end],
        velocity=features["velocity"][start:end],
        consistency=features["consistency"][start:end],
        volatility=features["volatility"][start:end],
        token_skew=features["token_skew"][start:end],
        elapsed_pct=features["elapsed_pct"][start:end],
        acceleration=features["acceleration"][start:end],
        price_up=features["price_up"][start:end],
        price_down=features["price_down"][start:end],
    )


def simulate_manifest(
    manifest: list[MarketMeta],
    features: dict,
    strategy_name: str,
    params: dict,
    stress: StressScenario | None = None,
) -> dict:
    stress = stress or StressScenario("base")
    check_fn = build_check_fn(strategy_name, params)
    trades = wins = 0
    total_pnl = 0.0
    entry_sum = 0.0

    for meta in manifest:
        pm = _slice_market(meta, features)
        for i in range(10, pm.num_snaps):
            direction = check_fn(pm, i)
            if direction is None:
                continue
            if direction == "SKIP":
                break

            entry_idx = i + stress.entry_delay
            if entry_idx >= pm.num_snaps:
                break

            entry = pm.price_up[entry_idx] if direction == "Up" else pm.price_down[entry_idx]
            entry = min(float(entry) + stress.entry_slippage, 0.999)
            if entry <= 0 or entry >= 0.99:
                break

            won = direction == pm.winner
            fee = taker_fee(entry) * entry * stress.fee_multiplier
            pnl = (1.0 - entry - fee) if won else -(entry + fee)

            trades += 1
            wins += int(won)
            total_pnl += pnl
            entry_sum += entry
            break

    return {
        "trades": trades,
        "wins": wins,
        "win_rate": wins / trades if trades else 0.0,
        "pnl": total_pnl,
        "pnl_per_trade": total_pnl / trades if trades else 0.0,
        "avg_entry": entry_sum / trades if trades else 0.0,
    }


def build_walk_forward_splits(
    n_markets: int,
    folds: int,
    holdout_pct: float,
) -> tuple[list[tuple[int, int, int]], tuple[int, int]]:
    holdout_start = max(int(n_markets * (1 - holdout_pct)), 1)
    walk_n = holdout_start
    min_train = max(int(walk_n * 0.4), 50)
    remaining = walk_n - min_train
    if remaining <= 0:
        return [], (holdout_start, n_markets)

    fold_size = max(remaining // folds, 1)
    splits: list[tuple[int, int, int]] = []
    train_end = min_train
    while train_end < walk_n and len(splits) < folds:
        test_end = min(train_end + fold_size, walk_n)
        if test_end > train_end:
            splits.append((0, train_end, test_end))
        train_end = test_end
    return splits, (holdout_start, n_markets)


def walk_forward_validate(
    manifest: list[MarketMeta],
    features: dict,
    strategy_name: str,
    params: dict,
    folds: int,
    holdout_pct: float,
) -> dict:
    splits, holdout = build_walk_forward_splits(len(manifest), folds, holdout_pct)
    fold_results = []
    for train_start, train_end, test_end in splits:
        train = manifest[train_start:train_end]
        test = manifest[train_end:test_end]
        train_stats = simulate_manifest(train, features, strategy_name, params)
        test_stats = simulate_manifest(test, features, strategy_name, params)
        fold_results.append({
            "train_markets": len(train),
            "test_markets": len(test),
            "train": train_stats,
            "test": test_stats,
        })

    holdout_stats = simulate_manifest(
        manifest[holdout[0]:holdout[1]], features, strategy_name, params
    )

    test_trades = sum(f["test"]["trades"] for f in fold_results)
    test_wins = sum(f["test"]["wins"] for f in fold_results)
    test_pnl = sum(f["test"]["pnl"] for f in fold_results)
    profitable_folds = sum(1 for f in fold_results if f["test"]["pnl"] > 0)
    fold_pnls = [f["test"]["pnl_per_trade"] for f in fold_results if f["test"]["trades"]]

    return {
        "folds": fold_results,
        "aggregate_oos": {
            "trades": test_trades,
            "wins": test_wins,
            "win_rate": test_wins / test_trades if test_trades else 0.0,
            "pnl": test_pnl,
            "pnl_per_trade": test_pnl / test_trades if test_trades else 0.0,
            "profitable_folds": profitable_folds,
            "num_folds": len(fold_results),
            "min_fold_pnl_per_trade": min(fold_pnls) if fold_pnls else 0.0,
            "max_fold_pnl_per_trade": max(fold_pnls) if fold_pnls else 0.0,
        },
        "final_holdout": holdout_stats,
        "final_holdout_markets": holdout[1] - holdout[0],
    }


def neighbor_params(strategy_name: str, params: dict) -> list[dict]:
    variants = []
    seen = {json.dumps(params, sort_keys=True)}

    def add(candidate: dict) -> None:
        key = json.dumps(candidate, sort_keys=True)
        if key not in seen:
            seen.add(key)
            variants.append(candidate)

    move = params.get("move")
    if move is not None:
        for delta in (-0.01, 0.01, -0.02, 0.02):
            add({**params, "move": max(0.01, round(move + delta, 3))})

    max_entry = params.get("max_entry")
    if max_entry is not None:
        for delta in (-0.05, 0.05):
            add({**params, "max_entry": min(0.85, max(0.4, round(max_entry + delta, 3)))})

    param_key = {
        "skew": "skew",
        "volatility": "vol",
        "acceleration": "accel",
        "velocity": "vel",
        "consistency": "cons",
        "timing": "elapsed",
        "combo": "cons",
        "vel+cons": "vel",
        "accel+time": "accel",
    }.get(strategy_name)

    if param_key and param_key in params:
        base = params[param_key]
        deltas = {
            "skew": (-0.01, 0.01),
            "vol": (-0.002, 0.002),
            "accel": (-0.01, 0.01),
            "vel": (-0.005, 0.005),
            "cons": (-0.05, 0.05),
            "elapsed": (-0.05, 0.05),
        }[param_key]
        for delta in deltas:
            add({**params, param_key: round(max(0.0, base + delta), 4)})

    return variants[:8]


def robustness_check(
    manifest: list[MarketMeta],
    features: dict,
    strategy_name: str,
    params: dict,
    folds: int,
    holdout_pct: float,
) -> dict:
    candidates = [params] + neighbor_params(strategy_name, params)
    rows = []
    for candidate in candidates:
        wf = walk_forward_validate(manifest, features, strategy_name, candidate, folds, holdout_pct)
        agg = wf["aggregate_oos"]
        rows.append({
            "params": candidate,
            "oos_trades": agg["trades"],
            "oos_win_rate": agg["win_rate"],
            "oos_pnl_per_trade": agg["pnl_per_trade"],
            "profitable_folds": agg["profitable_folds"],
            "holdout_pnl_per_trade": wf["final_holdout"]["pnl_per_trade"],
        })

    baseline = rows[0]
    stable_neighbors = [
        row for row in rows[1:]
        if row["oos_trades"] >= max(10, baseline["oos_trades"] * 0.5)
        and row["oos_pnl_per_trade"] > 0
        and row["holdout_pnl_per_trade"] > -0.02
    ]
    return {
        "baseline": baseline,
        "neighbors": rows[1:],
        "stable_neighbor_count": len(stable_neighbors),
        "total_neighbors": len(rows) - 1,
    }


def stress_check(
    manifest: list[MarketMeta],
    features: dict,
    strategy_name: str,
    params: dict,
) -> dict:
    scenarios = [
        StressScenario("base"),
        StressScenario("higher_fees", fee_multiplier=1.5),
        StressScenario("entry_slippage_1c", entry_slippage=0.01),
        StressScenario("entry_delay_1", entry_delay=1),
        StressScenario("delay_and_slip", entry_delay=1, entry_slippage=0.01, fee_multiplier=1.25),
    ]
    return {
        scenario.name: simulate_manifest(manifest, features, strategy_name, params, stress=scenario)
        for scenario in scenarios
    }


def classify_readiness(validation: dict, robustness: dict, stress: dict, coin: str) -> str:
    agg = validation["aggregate_oos"]
    holdout = validation["final_holdout"]
    min_trades = {"btc": 30, "eth": 30, "sol": 12}.get(coin, 30)

    if agg["trades"] < min_trades or holdout["trades"] < max(5, min_trades // 4):
        return "reject_for_now"
    if agg["pnl_per_trade"] <= 0 or holdout["pnl_per_trade"] <= 0:
        return "reject_for_now"
    if agg["profitable_folds"] < max(2, agg["num_folds"] - 1):
        return "reject_for_now"
    if robustness["stable_neighbor_count"] < max(2, robustness["total_neighbors"] // 3):
        return "paper_only_with_caution"
    stressed = stress["delay_and_slip"]["pnl_per_trade"]
    if stressed <= 0:
        return "paper_only_with_caution"
    return "ready_for_paper"


def validate_coin(
    coin: str,
    strategy_name: str,
    params: dict,
    folds: int,
    holdout_pct: float,
) -> dict:
    manifest, features = load_feature_store(coin)
    validation = walk_forward_validate(manifest, features, strategy_name, params, folds, holdout_pct)
    robustness = robustness_check(manifest, features, strategy_name, params, folds, holdout_pct)
    holdout_start = len(manifest) - validation["final_holdout_markets"]
    stress = stress_check(manifest[holdout_start:], features, strategy_name, params)
    readiness = classify_readiness(validation, robustness, stress, coin)
    return {
        "strategy": {"name": strategy_name, "params": params},
        "markets": len(manifest),
        "validation": validation,
        "robustness": robustness,
        "stress": stress,
        "readiness": readiness,
    }


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--coin", action="append", default=[])
    parser.add_argument("--folds", type=int, default=4)
    parser.add_argument("--holdout-pct", type=float, default=0.15)
    args = parser.parse_args()

    selected = args.coin or ["btc", "eth", "sol"]
    saved = json.loads(STRATEGY_RESULTS.read_text())
    report = {}
    for coin in selected:
        strategy = saved[coin]["best_strategy"]
        report[coin] = validate_coin(
            coin,
            strategy["name"],
            strategy["params"],
            args.folds,
            args.holdout_pct,
        )

    OUTPUT.write_text(json.dumps(report, indent=2))
    print(json.dumps(report, indent=2))


if __name__ == "__main__":
    main()
