"""Walk-forward evaluation for the whale-pair strategy.

Uses only our historical BTC market-state data to choose robust configs.
Whale activity is never consumed by this harness; any whale overlap is a
post-hoc shape check in the backtest module, not a tuning signal here.

Primary inputs
- SQLite DB (``--db``) with the standard schema:
  * ``markets(market_id, slug, market_type, start_time, end_time, winner, ...)``
  * ``snapshots(market_id, time, best_ask_up, ask_size_up, best_ask_down, ask_size_down, ...)``
  Use the worktree's ``backtesting/btc.db`` by default; pass ``--db-path`` to override.

Modes
- ``--fast``: reduced config grid (default: full grid).
- ``--sensitivity``: after consensus is chosen from CV, run a parameter sweep on
  the pooled out-of-sample test markets. Each swept parameter (max_pair_cost,
  min_seconds_from_start, base_clip_usd, aggressive_clip_usd, max_gross_cost_usd)
  is varied one at a time around the consensus; other params are frozen. This
  reports robustness without re-tuning.

Execution realism toggles
- ``--latency-snapshots``: N>0 executes the fill at snap[i+N] instead of snap[i].
  This exposes both slippage and missed fills (when i+N runs off the end of the
  day). Reported as ``missed_orders`` / ``partial_orders`` / ``execution_slippage_usd``.
- ``--fill-fraction``: fraction of top-of-book ask_size assumed fillable; partial
  fills are tracked.

Output
- One machine-readable JSON payload to stdout. Use ``--output`` to also persist.
"""
from __future__ import annotations

import argparse
import json
from dataclasses import dataclass, replace
from itertools import product
from pathlib import Path
from statistics import median
import sys
from typing import Iterable

ROOT = Path(__file__).resolve().parent.parent
if str(ROOT) not in sys.path:
    sys.path.insert(0, str(ROOT))

from backtesting.whale_pair_backtest import (
    ExecutionModel,
    load_markets,
    load_snapshots,
    simulate_market,
)
from shared.db import get_connection
from strategies.whale_pair import WhalePairConfig


@dataclass(frozen=True)
class Fold:
    train_markets: list[dict]
    test_markets: list[dict]


@dataclass(frozen=True)
class ConfigResult:
    cfg: WhalePairConfig
    markets: int
    traded: int
    wins: int
    total_pnl_usd: float
    gross_cost_usd: float
    attempted_orders: int = 0
    missed_orders: int = 0
    partial_orders: int = 0
    execution_slippage_usd: float = 0.0

    @property
    def pnl_bps_on_cost(self) -> float:
        if self.gross_cost_usd <= 0:
            return 0.0
        return (self.total_pnl_usd / self.gross_cost_usd) * 10000.0


def build_folds(markets: list[dict], *, min_train: int, fold_size: int) -> list[Fold]:
    folds: list[Fold] = []
    i = 0
    while min_train + (i + 1) * fold_size <= len(markets):
        train_end = min_train + i * fold_size
        test_end = train_end + fold_size
        folds.append(
            Fold(
                train_markets=markets[:train_end],
                test_markets=markets[train_end:test_end],
            )
        )
        i += 1
    return folds


def iter_configs() -> Iterable[WhalePairConfig]:
    grid = {
        "max_pair_cost": [0.985, 0.99],
        "base_clip_usd": [25.0, 50.0],
        "aggressive_clip_usd": [100.0, 250.0],
        "max_gross_cost_usd": [500.0, 1000.0, 2000.0],
        "min_seconds_from_start": [0, 10],
        "completion_min_pnl_per_share": [0.001, 0.002],
    }
    for values in product(*grid.values()):
        kwargs = dict(zip(grid.keys(), values))
        yield WhalePairConfig(
            max_pair_cost=float(kwargs["max_pair_cost"]),
            base_clip_usd=float(kwargs["base_clip_usd"]),
            aggressive_clip_usd=float(kwargs["aggressive_clip_usd"]),
            max_gross_cost_usd=float(kwargs["max_gross_cost_usd"]),
            min_seconds_from_start=int(kwargs["min_seconds_from_start"]),
            completion_min_pnl_per_share=float(kwargs["completion_min_pnl_per_share"]),
        )


def iter_configs_fast() -> Iterable[WhalePairConfig]:
    grid = {
        "max_pair_cost": [0.985, 0.99],
        "base_clip_usd": [25.0, 50.0],
        "aggressive_clip_usd": [100.0],
        "max_gross_cost_usd": [500.0, 1000.0],
        "min_seconds_from_start": [0, 10],
        "completion_min_pnl_per_share": [0.001, 0.002],
    }
    for values in product(*grid.values()):
        kwargs = dict(zip(grid.keys(), values))
        yield WhalePairConfig(
            max_pair_cost=float(kwargs["max_pair_cost"]),
            base_clip_usd=float(kwargs["base_clip_usd"]),
            aggressive_clip_usd=float(kwargs["aggressive_clip_usd"]),
            max_gross_cost_usd=float(kwargs["max_gross_cost_usd"]),
            min_seconds_from_start=int(kwargs["min_seconds_from_start"]),
            completion_min_pnl_per_share=float(kwargs["completion_min_pnl_per_share"]),
        )


def evaluate_markets(
    markets: list[dict],
    load_market_snapshots,
    cfg: WhalePairConfig,
    execution: ExecutionModel | None = None,
) -> ConfigResult:
    traded = 0
    wins = 0
    total_pnl = 0.0
    gross_cost = 0.0
    attempted_orders = 0
    missed_orders = 0
    partial_orders = 0
    execution_slippage_usd = 0.0
    for market in markets:
        result = simulate_market(
            market,
            load_market_snapshots(market["market_id"]),
            cfg,
            execution=execution,
        )
        total_pnl += result.total_pnl_usd
        gross_cost += result.gross_cost_usd
        attempted_orders += result.attempted_orders
        missed_orders += result.missed_orders
        partial_orders += result.partial_orders
        execution_slippage_usd += result.execution_slippage_usd
        if result.fills > 0:
            traded += 1
            if result.total_pnl_usd > 0:
                wins += 1
    return ConfigResult(
        cfg=cfg,
        markets=len(markets),
        traded=traded,
        wins=wins,
        total_pnl_usd=total_pnl,
        gross_cost_usd=gross_cost,
        attempted_orders=attempted_orders,
        missed_orders=missed_orders,
        partial_orders=partial_orders,
        execution_slippage_usd=execution_slippage_usd,
    )


SENSITIVITY_GRID: dict[str, list[float]] = {
    "max_pair_cost": [0.97, 0.98, 0.985, 0.99, 0.995],
    "min_seconds_from_start": [0, 10, 30, 60, 120],
    "base_clip_usd": [10.0, 25.0, 50.0, 100.0],
    "aggressive_clip_usd": [50.0, 100.0, 250.0, 500.0],
    "max_gross_cost_usd": [200.0, 500.0, 1000.0, 2000.0, 5000.0],
}


def _result_metrics(result: ConfigResult) -> dict:
    return {
        "traded": result.traded,
        "wins": result.wins,
        "win_rate": (result.wins / result.traded) if result.traded else 0.0,
        "total_pnl_usd": result.total_pnl_usd,
        "gross_cost_usd": result.gross_cost_usd,
        "pnl_bps_on_cost": result.pnl_bps_on_cost,
        "attempted_orders": result.attempted_orders,
        "missed_orders": result.missed_orders,
        "partial_orders": result.partial_orders,
        "execution_slippage_usd": result.execution_slippage_usd,
    }


def sensitivity_sweep(
    markets: list[dict],
    load_market_snapshots,
    baseline: WhalePairConfig,
    *,
    execution: ExecutionModel | None = None,
    grid: dict[str, list[float]] | None = None,
) -> dict[str, list[dict]]:
    """Vary one parameter at a time around ``baseline`` and report metrics.

    Returns ``{param_name: [{"value": v, **metrics, "delta_pnl_usd": ...}, ...]}``
    where deltas are relative to the baseline config evaluated on the same
    market set. This measures config robustness on held-out markets without
    re-running model selection.
    """
    grid = grid or SENSITIVITY_GRID
    baseline_result = evaluate_markets(markets, load_market_snapshots, baseline, execution=execution)
    base_pnl = baseline_result.total_pnl_usd
    base_bps = baseline_result.pnl_bps_on_cost
    sweep: dict[str, list[dict]] = {
        "_baseline": [
            {
                "cfg": _serialize_cfg(baseline),
                **_result_metrics(baseline_result),
            }
        ]
    }
    for param, values in grid.items():
        entries: list[dict] = []
        for value in values:
            if param in {"min_seconds_from_start"}:
                cfg = replace(baseline, **{param: int(value)})
            else:
                cfg = replace(baseline, **{param: float(value)})
            result = evaluate_markets(markets, load_market_snapshots, cfg, execution=execution)
            entries.append(
                {
                    "value": value,
                    **_result_metrics(result),
                    "delta_pnl_usd": result.total_pnl_usd - base_pnl,
                    "delta_pnl_bps_on_cost": result.pnl_bps_on_cost - base_bps,
                }
            )
        sweep[param] = entries
    return sweep


def _serialize_cfg(cfg: WhalePairConfig) -> dict:
    return {
        "max_pair_cost": cfg.max_pair_cost,
        "accumulate_price_max": cfg.accumulate_price_max,
        "aggressive_price_max": cfg.aggressive_price_max,
        "base_clip_usd": cfg.base_clip_usd,
        "aggressive_clip_usd": cfg.aggressive_clip_usd,
        "max_gross_cost_usd": cfg.max_gross_cost_usd,
        "min_seconds_from_start": cfg.min_seconds_from_start,
        "max_seconds_from_start": cfg.max_seconds_from_start,
        "completion_min_pnl_per_share": cfg.completion_min_pnl_per_share,
        "max_imbalance_ratio": cfg.max_imbalance_ratio,
    }


def score(result: ConfigResult) -> tuple[float, float, int]:
    return (
        result.pnl_bps_on_cost,
        result.total_pnl_usd,
        result.traded,
    )


def median_config(results: list[ConfigResult]) -> WhalePairConfig:
    return WhalePairConfig(
        max_pair_cost=float(median([r.cfg.max_pair_cost for r in results])),
        base_clip_usd=float(median([r.cfg.base_clip_usd for r in results])),
        aggressive_clip_usd=float(median([r.cfg.aggressive_clip_usd for r in results])),
        max_gross_cost_usd=float(median([r.cfg.max_gross_cost_usd for r in results])),
        min_seconds_from_start=int(median([r.cfg.min_seconds_from_start for r in results])),
        max_seconds_from_start=int(median([r.cfg.max_seconds_from_start for r in results])),
        completion_min_pnl_per_share=float(median([r.cfg.completion_min_pnl_per_share for r in results])),
        max_imbalance_ratio=float(median([r.cfg.max_imbalance_ratio for r in results])),
    )


def main(
    db_path: str,
    min_train: int,
    fold_size: int,
    market_limit: int,
    fast: bool,
    execution: ExecutionModel,
    *,
    sensitivity: bool = False,
    output_path: str | None = None,
) -> int:
    conn = get_connection(db_path)
    try:
        markets = load_markets(conn, market_type="5m", limit=market_limit if market_limit > 0 else None)
        cached_snapshots: dict[str, list[dict]] = {}

        def load_market_snapshots(market_id: str) -> list[dict]:
            if market_id not in cached_snapshots:
                cached_snapshots[market_id] = load_snapshots(conn, market_id)
            return cached_snapshots[market_id]

        markets = [m for m in markets if load_market_snapshots(m["market_id"])]
        folds = build_folds(markets, min_train=min_train, fold_size=fold_size)
        if not folds:
            print(json.dumps({"error": "not enough markets", "markets": len(markets)}, indent=2))
            return 1

        configs = list(iter_configs_fast() if fast else iter_configs())
        fold_best: list[ConfigResult] = []
        fold_rows: list[dict] = []
        print(f"Loaded {len(markets)} markets with snapshots from {db_path}")
        print(f"Evaluating {len(configs)} configs across {len(folds)} folds")

        for idx, fold in enumerate(folds):
            best_train: ConfigResult | None = None
            best_test: ConfigResult | None = None
            for cfg in configs:
                train_result = evaluate_markets(
                    fold.train_markets,
                    load_market_snapshots,
                    cfg,
                    execution=execution,
                )
                if best_train is None or score(train_result) > score(best_train):
                    best_train = train_result
                    best_test = evaluate_markets(
                        fold.test_markets,
                        load_market_snapshots,
                        cfg,
                        execution=execution,
                    )
            assert best_train is not None and best_test is not None
            fold_best.append(best_train)
            row = {
                "fold": idx,
                "train_markets": len(fold.train_markets),
                "test_markets": len(fold.test_markets),
                "cfg": {
                    "max_pair_cost": best_train.cfg.max_pair_cost,
                    "base_clip_usd": best_train.cfg.base_clip_usd,
                    "aggressive_clip_usd": best_train.cfg.aggressive_clip_usd,
                    "max_gross_cost_usd": best_train.cfg.max_gross_cost_usd,
                    "min_seconds_from_start": best_train.cfg.min_seconds_from_start,
                    "completion_min_pnl_per_share": best_train.cfg.completion_min_pnl_per_share,
                },
                "train": {
                    "traded": best_train.traded,
                    "wins": best_train.wins,
                    "total_pnl_usd": best_train.total_pnl_usd,
                    "gross_cost_usd": best_train.gross_cost_usd,
                    "pnl_bps_on_cost": best_train.pnl_bps_on_cost,
                    "attempted_orders": best_train.attempted_orders,
                    "missed_orders": best_train.missed_orders,
                    "partial_orders": best_train.partial_orders,
                    "execution_slippage_usd": best_train.execution_slippage_usd,
                },
                "test": {
                    "traded": best_test.traded,
                    "wins": best_test.wins,
                    "total_pnl_usd": best_test.total_pnl_usd,
                    "gross_cost_usd": best_test.gross_cost_usd,
                    "pnl_bps_on_cost": best_test.pnl_bps_on_cost,
                    "attempted_orders": best_test.attempted_orders,
                    "missed_orders": best_test.missed_orders,
                    "partial_orders": best_test.partial_orders,
                    "execution_slippage_usd": best_test.execution_slippage_usd,
                },
            }
            fold_rows.append(row)

        consensus = median_config(fold_best)
        consensus_results = [
            evaluate_markets(
                fold.test_markets,
                load_market_snapshots,
                consensus,
                execution=execution,
            )
            for fold in folds
        ]
        payload = {
            "db_path": db_path,
            "execution": {
                "latency_snapshots": execution.latency_snapshots,
                "fill_fraction": execution.fill_fraction,
            },
            "markets": len(markets),
            "fold_count": len(folds),
            "grid_size": len(configs),
            "folds": fold_rows,
            "consensus_cfg": {
                "max_pair_cost": consensus.max_pair_cost,
                "base_clip_usd": consensus.base_clip_usd,
                "aggressive_clip_usd": consensus.aggressive_clip_usd,
                "max_gross_cost_usd": consensus.max_gross_cost_usd,
                "min_seconds_from_start": consensus.min_seconds_from_start,
                "completion_min_pnl_per_share": consensus.completion_min_pnl_per_share,
            },
            "consensus_test_summary": {
                "traded": sum(r.traded for r in consensus_results),
                "wins": sum(r.wins for r in consensus_results),
                "total_pnl_usd": sum(r.total_pnl_usd for r in consensus_results),
                "gross_cost_usd": sum(r.gross_cost_usd for r in consensus_results),
                "attempted_orders": sum(r.attempted_orders for r in consensus_results),
                "missed_orders": sum(r.missed_orders for r in consensus_results),
                "partial_orders": sum(r.partial_orders for r in consensus_results),
                "execution_slippage_usd": sum(r.execution_slippage_usd for r in consensus_results),
                "pnl_bps_on_cost": (
                    (sum(r.total_pnl_usd for r in consensus_results) / sum(r.gross_cost_usd for r in consensus_results)) * 10000.0
                    if sum(r.gross_cost_usd for r in consensus_results) > 0
                    else 0.0
                ),
            },
        }

        if sensitivity:
            pooled_test: list[dict] = []
            seen_ids: set[str] = set()
            for fold in folds:
                for m in fold.test_markets:
                    mid = m["market_id"]
                    if mid in seen_ids:
                        continue
                    seen_ids.add(mid)
                    pooled_test.append(m)
            payload["sensitivity"] = {
                "pooled_test_markets": len(pooled_test),
                "baseline_cfg": _serialize_cfg(consensus),
                "sweep": sensitivity_sweep(
                    pooled_test,
                    load_market_snapshots,
                    consensus,
                    execution=execution,
                ),
            }

        text = json.dumps(payload, indent=2)
        if output_path:
            Path(output_path).write_text(text)
        print(text)
        return 0
    finally:
        conn.close()


if __name__ == "__main__":
    ap = argparse.ArgumentParser()
    ap.add_argument("--db", default="backtesting/btc.db")
    ap.add_argument(
        "--db-path",
        default=None,
        help="Alias for --db; takes precedence when both are set.",
    )
    ap.add_argument("--min-train", type=int, default=200)
    ap.add_argument("--fold-size", type=int, default=100)
    ap.add_argument("--market-limit", type=int, default=1000)
    ap.add_argument("--fast", action="store_true")
    ap.add_argument(
        "--sensitivity",
        action="store_true",
        help="Sweep max_pair_cost/min_seconds_from_start/clip/max_gross_cost_usd one at a time around the consensus config on pooled OOS test markets.",
    )
    ap.add_argument("--latency-snapshots", type=int, default=0)
    ap.add_argument("--fill-fraction", type=float, default=1.0)
    ap.add_argument("--output", default="")
    args = ap.parse_args()
    execution = ExecutionModel(
        latency_snapshots=max(0, int(args.latency_snapshots)),
        fill_fraction=max(0.0, min(1.0, float(args.fill_fraction))),
    )
    db_path = args.db_path or args.db
    raise SystemExit(
        main(
            db_path,
            args.min_train,
            args.fold_size,
            args.market_limit,
            args.fast,
            execution,
            sensitivity=args.sensitivity,
            output_path=(args.output or None),
        )
    )
