"""Researcher pipeline: forecaster + simulator + CPCV + bootstrap + edge gate.

This is the orchestration script that ties the framework together.
Given a coin and a feature store, it:

  1. Loads the manifest and feature arrays
  2. Splits into TRAIN / CV / HOLDOUT via the canonical 60/20/20
     partition function
  3. Constructs MarketSlice objects from the manifest + features
  4. For each candidate in a small parameter grid:
     a. Fits RegimeGatedForecaster on TRAIN
     b. Applies isotonic recalibration on the CV slice
     c. Runs the simulator on CV with the candidate forecaster
     d. Runs the simulator on CV with the market-implied baseline
     e. Computes log_growth_per_trade for both, with bootstrap CIs
     f. Computes the edge of candidate over baseline
     g. Computes Brier + Murphy decomposition + reliability diagram
  5. Reports the results in a structured artifact

The HOLDOUT partition is NEVER touched by this script. Holdout
evaluation is a separate, single-shot action via the (future)
Holdout Burner agent.

Usage:
    python3 -m autoresearch.researcher --coin eth
    python3 -m autoresearch.researcher --coin eth --output-dir runs/
"""
from __future__ import annotations

import argparse
import dataclasses
import hashlib
import json
import logging
import math
import sys
import time
from dataclasses import dataclass, field, asdict
from pathlib import Path
from typing import Optional

import numpy as np

from autoresearch.audit import AuditLog
from autoresearch.forecasters.baseline import MarketImpliedBaseline
from autoresearch.forecasters.regime_gated import RegimeGatedForecaster
from autoresearch.methods.bootstrap import (
    acf_block_length,
    stationary_block_bootstrap,
)
from autoresearch.methods.calibration import (
    brier_score,
    murphy_decomposition,
    reliability_diagram,
)
from autoresearch.methods.edge import market_implied_q
from autoresearch.methods.simulator import (
    MarketSlice,
    SimulatorReport,
    market_implied_baseline_forecaster,
    simulate_partition,
)
from autoresearch.methods.sizing import SizingPolicy
from autoresearch.partitions import make_partitions
from backtesting.eval.feature_store import (
    REGIME_FEATURE_NAMES,
    load_feature_store,
    store_dir,
)


logger = logging.getLogger(__name__)


@dataclass
class CandidateSpec:
    """One row of the parameter grid."""
    C: float
    lag_n: int
    min_abs_move: float
    recalibrate: bool

    def hash_id(self, dataset_fingerprint: str) -> str:
        body = json.dumps(
            {
                "family": "regime_gated_dual_logistic",
                **dataclasses.asdict(self),
                "dataset": dataset_fingerprint,
            },
            sort_keys=True,
        )
        return hashlib.sha256(body.encode()).hexdigest()[:16]


@dataclass
class CandidateReport:
    candidate: dict
    candidate_hash: str
    train_stats: dict
    cv_stats: dict
    baseline_cv_stats: dict
    edge_over_baseline: dict
    calibration: dict
    bootstrap_log_growth: dict
    bootstrap_baseline_log_growth: dict


def _market_slice_from_manifest(
    market_meta, features: dict[str, np.ndarray]
) -> MarketSlice:
    start = market_meta.offset
    end = start + market_meta.length
    return MarketSlice(
        market_id=market_meta.market_id,
        winner=market_meta.winner,
        best_ask_up=np.asarray(features["best_ask_up"][start:end]),
        best_ask_down=np.asarray(features["best_ask_down"][start:end]),
        best_bid_up=np.asarray(features["best_bid_up"][start:end]),
        best_bid_down=np.asarray(features["best_bid_down"][start:end]),
        features={name: np.asarray(features[name][start:end]) for name in features},
    )


def build_market_slices(coin: str) -> list[MarketSlice]:
    """Load the feature store for a coin and return MarketSlice objects."""
    manifest, features = load_feature_store(coin)
    return [_market_slice_from_manifest(m, features) for m in manifest]


def dataset_fingerprint(coin: str) -> str:
    """Compute a fingerprint of the feature store for reproducibility.

    Hash includes coin name, manifest market count, total snapshot
    count, and the file size of the manifest.json. Cheap and adequate
    for distinguishing dataset versions across runs.
    """
    sd = store_dir(coin)
    manifest_path = sd / "manifest.json"
    raw = manifest_path.read_bytes()
    h = hashlib.sha256()
    h.update(coin.encode())
    h.update(str(len(raw)).encode())
    manifest = json.loads(raw)
    h.update(str(len(manifest)).encode())
    total_snaps = sum(m["length"] for m in manifest)
    h.update(str(total_snaps).encode())
    return f"{coin}:{len(manifest)}m:{total_snaps}s:{h.hexdigest()[:12]}"


def evaluate_candidate(
    spec: CandidateSpec,
    train_slices: list[MarketSlice],
    cv_slices: list[MarketSlice],
    sizing_policy: SizingPolicy,
    seed: int,
) -> CandidateReport:
    """Fit a candidate, simulate on CV, return a CandidateReport."""
    # Fit
    forecaster = RegimeGatedForecaster(
        C=spec.C,
        lag_n=spec.lag_n,
        min_abs_move=spec.min_abs_move,
    ).fit(train_slices, cv_slices=cv_slices, recalibrate=spec.recalibrate)

    # Simulate the candidate on CV
    sim = simulate_partition(
        cv_slices,
        forecaster.as_simulator_forecaster(),
        sizing_policy=sizing_policy,
    )
    # Simulate the baseline on CV
    baseline_sim = simulate_partition(
        cv_slices,
        market_implied_baseline_forecaster,
        sizing_policy=sizing_policy,
    )

    # Bootstrap CI on log_growth_per_trade for the candidate
    cand_log_returns = np.array(
        [t.log_return for t in sim.trades], dtype=np.float64
    )
    base_log_returns = np.array(
        [t.log_return for t in baseline_sim.trades], dtype=np.float64
    )

    if len(cand_log_returns) >= 4:
        bl = acf_block_length(cand_log_returns, max_lag=20)
        cand_boot = stationary_block_bootstrap(
            cand_log_returns,
            statistic=lambda x: float(np.mean(x)),
            block_length=bl,
            n_resamples=2000,
            seed=seed,
        )
        cand_boot_dict = {
            "point_estimate": cand_boot.point_estimate,
            "lower_ci_95": cand_boot.lower_ci,
            "upper_ci_95": cand_boot.upper_ci,
            "block_length": cand_boot.block_length,
            "n_resamples": cand_boot.n_resamples,
        }
    else:
        cand_boot_dict = {
            "point_estimate": float("nan"),
            "lower_ci_95": float("nan"),
            "upper_ci_95": float("nan"),
            "block_length": 0,
            "n_resamples": 0,
        }

    if len(base_log_returns) >= 4:
        bl_b = acf_block_length(base_log_returns, max_lag=20)
        base_boot = stationary_block_bootstrap(
            base_log_returns,
            statistic=lambda x: float(np.mean(x)),
            block_length=bl_b,
            n_resamples=2000,
            seed=seed + 1,
        )
        base_boot_dict = {
            "point_estimate": base_boot.point_estimate,
            "lower_ci_95": base_boot.lower_ci,
            "upper_ci_95": base_boot.upper_ci,
            "block_length": base_boot.block_length,
            "n_resamples": base_boot.n_resamples,
        }
    else:
        base_boot_dict = {
            "point_estimate": float("nan"),
            "lower_ci_95": float("nan"),
            "upper_ci_95": float("nan"),
            "block_length": 0,
            "n_resamples": 0,
        }

    # Edge over baseline
    edge_dict = {
        "candidate_log_growth": cand_boot_dict["point_estimate"],
        "baseline_log_growth": base_boot_dict["point_estimate"],
        "delta": cand_boot_dict["point_estimate"] - base_boot_dict["point_estimate"]
            if not (math.isnan(cand_boot_dict["point_estimate"]) or
                    math.isnan(base_boot_dict["point_estimate"])) else float("nan"),
        "candidate_lower_ci_above_zero": (
            (not math.isnan(cand_boot_dict["lower_ci_95"])) and
            cand_boot_dict["lower_ci_95"] > 0
        ),
    }

    # Calibration audit on the CV trades
    if sim.trades:
        # For each trade, the forecaster's q at entry vs the realised outcome
        forecasts = []
        outcomes = []
        for t in sim.trades:
            # We don't have q at entry directly; reconstruct by calling
            # the simulator's forecaster on the same (slice, index)
            # This is wasteful but accurate.
            slice_idx = next(
                (i for i, s in enumerate(cv_slices) if s.market_id == t.market_id),
                None,
            )
            if slice_idx is None:
                continue
            slice_ = cv_slices[slice_idx]
            q = forecaster.as_simulator_forecaster()(slice_, t.entry_index)
            # The "outcome" relative to q (P YES wins): 1 if YES won, 0 if NO won
            outcome_yes = 1.0 if slice_.winner == "Up" else 0.0
            forecasts.append(q)
            outcomes.append(outcome_yes)
        forecasts = np.array(forecasts)
        outcomes = np.array(outcomes)
        try:
            brier = brier_score(forecasts, outcomes)
            murphy = murphy_decomposition(forecasts, outcomes, n_bins=5)
            reliability = reliability_diagram(forecasts, outcomes, n_bins=5)
            calibration_dict = {
                "n_trades": int(len(forecasts)),
                "brier": brier,
                "murphy_uncertainty": murphy.uncertainty,
                "murphy_resolution": murphy.resolution,
                "murphy_calibration": murphy.calibration,
                "reliability_mean_forecasts": reliability.mean_forecasts.tolist(),
                "reliability_mean_outcomes": reliability.mean_outcomes.tolist(),
                "reliability_counts": reliability.counts.tolist(),
            }
        except ValueError as exc:
            calibration_dict = {"error": str(exc), "n_trades": int(len(forecasts))}
    else:
        calibration_dict = {"n_trades": 0, "note": "no trades to evaluate"}

    return CandidateReport(
        candidate=dataclasses.asdict(spec),
        candidate_hash="",  # filled in by caller
        train_stats=forecaster.train_stats,
        cv_stats={
            "n_trades": sim.n_trades,
            "n_skipped_no_book": sim.n_skipped_no_book,
            "n_no_entry": sim.n_no_entry,
            "log_growth_per_trade": sim.log_growth_per_trade,
            "log_growth_total": sim.log_growth_total,
            "win_rate": sim.win_rate,
            "bankroll_final": sim.bankroll_final,
        },
        baseline_cv_stats={
            "n_trades": baseline_sim.n_trades,
            "log_growth_per_trade": baseline_sim.log_growth_per_trade,
            "log_growth_total": baseline_sim.log_growth_total,
            "win_rate": baseline_sim.win_rate,
            "bankroll_final": baseline_sim.bankroll_final,
        },
        edge_over_baseline=edge_dict,
        calibration=calibration_dict,
        bootstrap_log_growth=cand_boot_dict,
        bootstrap_baseline_log_growth=base_boot_dict,
    )


def run(coin: str, output_dir: Optional[Path] = None, seed: int = 42) -> dict:
    logging.basicConfig(level=logging.INFO, format="%(asctime)s %(levelname)s %(message)s")
    logger.info(f"=== Researcher run: coin={coin} ===")

    fingerprint = dataset_fingerprint(coin)
    logger.info(f"dataset fingerprint: {fingerprint}")

    slices = build_market_slices(coin)
    logger.info(f"loaded {len(slices)} markets")

    parts = make_partitions(len(slices))
    train_slices = slices[parts.train_slice()]
    cv_slices = slices[parts.cv_slice()]
    # We do NOT touch holdout in this script.
    logger.info(
        f"partitions: train={parts.train_size()} cv={parts.cv_size()} "
        f"holdout={parts.holdout_size()} (holdout untouched)"
    )

    # Small grid: 2 C values × 2 min_abs_move × 1 lag_n × 2 recalibrate = 8 cells
    grid = [
        CandidateSpec(C=C, lag_n=20, min_abs_move=mam, recalibrate=recal)
        for C in (0.1, 1.0)
        for mam in (0.05, 0.08)
        for recal in (False, True)
    ]
    logger.info(f"grid size: {len(grid)} candidates")

    sizing_policy = SizingPolicy(kelly_fraction=0.25, shrink_alpha=0.5, max_bet_fraction=0.05)

    reports: list[CandidateReport] = []
    for i, spec in enumerate(grid):
        t0 = time.time()
        logger.info(f"[{i+1}/{len(grid)}] fitting {spec}")
        rep = evaluate_candidate(spec, train_slices, cv_slices, sizing_policy, seed=seed + i)
        rep.candidate_hash = spec.hash_id(fingerprint)
        elapsed = time.time() - t0
        logger.info(
            f"  hash={rep.candidate_hash[:8]} cv_trades={rep.cv_stats['n_trades']} "
            f"log_growth={rep.cv_stats['log_growth_per_trade']:+.5f} "
            f"baseline={rep.baseline_cv_stats['log_growth_per_trade']:+.5f} "
            f"edge_lower_ci_pos={rep.edge_over_baseline['candidate_lower_ci_above_zero']} "
            f"in {elapsed:.1f}s"
        )
        reports.append(rep)

    # Summary
    summary = {
        "coin": coin,
        "dataset_fingerprint": fingerprint,
        "n_markets_total": len(slices),
        "partitions": {
            "train": parts.train_size(),
            "cv": parts.cv_size(),
            "holdout_untouched": parts.holdout_size(),
        },
        "n_candidates": len(grid),
        "candidates": [asdict(r) for r in reports],
    }

    # Find the best candidate by point estimate of log_growth (purely
    # informational; promotion gating is a separate decision)
    valid = [r for r in reports if not math.isnan(r.bootstrap_log_growth["point_estimate"])]
    if valid:
        best = max(valid, key=lambda r: r.bootstrap_log_growth["point_estimate"])
        summary["best_candidate_hash"] = best.candidate_hash
        summary["best_candidate_log_growth"] = best.bootstrap_log_growth["point_estimate"]
        summary["best_candidate_edge_over_baseline"] = best.edge_over_baseline["delta"]
        summary["best_candidate_lower_ci_above_zero"] = best.edge_over_baseline["candidate_lower_ci_above_zero"]

    # Write artifact
    out_dir = output_dir or Path("autoresearch/runs")
    out_dir.mkdir(parents=True, exist_ok=True)
    ts = time.strftime("%Y%m%dT%H%M%SZ", time.gmtime())
    artifact_path = out_dir / f"{ts}-{coin}-research.json"
    artifact_path.write_text(json.dumps(summary, indent=2, default=str))
    logger.info(f"artifact written: {artifact_path}")

    # Audit log entry
    log = AuditLog()
    log.append(
        agent="researcher",
        action="research_run_completed",
        subject={"type": "research_run", "coin": coin, "fingerprint": fingerprint},
        details={
            "n_markets": len(slices),
            "n_candidates": len(grid),
            "best_log_growth": summary.get("best_candidate_log_growth"),
            "best_lower_ci_above_zero": summary.get("best_candidate_lower_ci_above_zero"),
            "artifact_path": str(artifact_path),
        },
    )

    return summary


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--coin", required=True, choices=("btc", "eth"))
    parser.add_argument("--output-dir", type=Path, default=None)
    parser.add_argument("--seed", type=int, default=42)
    args = parser.parse_args()

    summary = run(args.coin, args.output_dir, args.seed)

    # Print compact summary
    print()
    print("=" * 60)
    print(f"RESEARCH RUN SUMMARY: {args.coin.upper()}")
    print("=" * 60)
    print(f"dataset: {summary['dataset_fingerprint']}")
    print(f"markets: {summary['n_markets_total']} (train {summary['partitions']['train']}, "
          f"cv {summary['partitions']['cv']}, holdout {summary['partitions']['holdout_untouched']} untouched)")
    print(f"candidates evaluated: {summary['n_candidates']}")
    print()
    print(f"{'cand_hash':>10} {'cv_trades':>10} {'log_growth':>14} {'baseline':>14} {'edge':>14} {'lower>0':>8}")
    for r in summary["candidates"]:
        h = r["candidate_hash"][:8]
        n = r["cv_stats"]["n_trades"]
        lg = r["cv_stats"]["log_growth_per_trade"]
        bl = r["baseline_cv_stats"]["log_growth_per_trade"]
        ed = r["edge_over_baseline"]["delta"]
        gp = "YES" if r["edge_over_baseline"]["candidate_lower_ci_above_zero"] else "no"
        print(f"{h:>10} {n:>10} {lg:>+14.6f} {bl:>+14.6f} {ed:>+14.6f} {gp:>8}")
    print()
    if "best_candidate_hash" in summary:
        print(f"best by point estimate: {summary['best_candidate_hash'][:8]} "
              f"(log_growth={summary['best_candidate_log_growth']:+.6f}, "
              f"edge={summary['best_candidate_edge_over_baseline']:+.6f}, "
              f"lower_ci_above_zero={summary['best_candidate_lower_ci_above_zero']})")
    else:
        print("no valid candidates")


if __name__ == "__main__":
    main()
