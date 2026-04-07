"""Autoresearch orchestration with fixed evaluator and structured artifacts.

The agent may refine search directions and Python search code, but large-scale
candidate evaluation always runs through deterministic Python and the canonical
validator.
"""
from __future__ import annotations

import argparse
import logging
import time
from pathlib import Path

from autoresearch.metrics import (
    compute_max_drawdown,
    compute_sharpe,
    is_result_significant,
    score_trades,
    sort_results,
)
from autoresearch.models import CandidateArtifact, ExperimentRecord
from autoresearch.search import shortlist_candidates
from autoresearch.store import (
    append_experiment,
    fingerprint_dataset,
    load_current_best,
    save_candidate,
    utc_now,
)
from backtesting.eval.validate_strategy_readiness import validate_coin
from shared.constants import COINS

logger = logging.getLogger(__name__)


def _candidate_is_better(candidate: dict, current_best: dict) -> bool:
    previous = current_best.get("best_strategy", {})
    prev_score = previous.get("test_sharpe", float("-inf"))
    return candidate.get("test_sharpe", float("-inf")) > prev_score


def _profile_patch(coin: str, candidate: dict) -> dict:
    return {
        "coins": {
            coin: {
                "strategy": candidate["name"],
                "params": candidate["params"],
            }
        }
    }


def _acceptance_reason(best: dict, validation: dict, current_best: dict) -> tuple[bool, str]:
    readiness = validation["readiness"]
    if readiness == "reject_for_now":
        return False, "validator_rejected"
    if not _candidate_is_better(best, current_best):
        return False, "no_improvement_over_current_best"
    return True, "accepted_for_review"


def run_once(
    coins: list[str],
    db_dir: Path | None = None,
    shortlist_size: int = 5,
    strategy_names: list[str] | None = None,
) -> list[Path]:
    current = load_current_best()
    saved: list[Path] = []

    for coin in coins:
        logger.info("[%s] Starting autoresearch", coin.upper())
        started = time.time()
        try:
            shortlist, total_profitable, manifest, _features, db_path = shortlist_candidates(
                coin,
                db_dir=db_dir,
                limit=shortlist_size,
                strategy_names=strategy_names,
            )
        except FileNotFoundError as exc:
            logger.warning("[%s] %s", coin.upper(), exc)
            continue

        if not shortlist:
            logger.info("[%s] No significant candidates", coin.upper())
            continue

        dataset = fingerprint_dataset(coin, db_path, len(manifest))
        best = shortlist[0]
        validation = validate_coin(
            coin,
            best["name"],
            best["params"],
            folds=5,
            holdout_pct=0.2,
        )
        current_best = current.get(coin, {})
        accepted, reason = _acceptance_reason(best, validation, current_best)
        elapsed = time.time() - started

        record = ExperimentRecord(
            coin=coin,
            created_at=utc_now(),
            dataset=dataset,
            current_best=current_best,
            candidate=best,
            validation=validation,
            accepted=accepted,
            reason=reason,
            search_time_s=round(elapsed, 1),
        )
        append_experiment(record)

        if not accepted:
            logger.info("[%s] Candidate rejected: %s", coin.upper(), reason)
            continue

        artifact = CandidateArtifact(
            coin=coin,
            created_at=record.created_at,
            current_best=current_best.get("best_strategy", {}),
            proposed=best,
            top_5=shortlist[:5],
            validation=validation,
            total_profitable=total_profitable,
            search_time_s=round(elapsed, 1),
            profile_patch=_profile_patch(coin, best),
        )
        path = save_candidate(artifact)
        saved.append(path)
        logger.info("[%s] Candidate saved: %s", coin.upper(), path.name)

    return saved


def main() -> None:
    logging.basicConfig(level=logging.INFO, format="%(asctime)s %(levelname)s %(message)s")
    parser = argparse.ArgumentParser()
    parser.add_argument("--coin", action="append", choices=COINS)
    parser.add_argument("--db-dir", type=str, default=None)
    parser.add_argument("--shortlist-size", type=int, default=5)
    parser.add_argument("--strategy", action="append", default=None, help="Optional strategy names to constrain search")
    parser.add_argument("--loop", action="store_true")
    parser.add_argument("--interval", type=int, default=86400)
    args = parser.parse_args()

    coins = args.coin or COINS
    db_dir = Path(args.db_dir) if args.db_dir else None
    if args.loop:
        logger.info("Starting autoresearch loop interval=%ss coins=%s", args.interval, coins)
        while True:
            saved = run_once(
                coins,
                db_dir=db_dir,
                shortlist_size=args.shortlist_size,
                strategy_names=args.strategy,
            )
            for path in saved:
                print(f"Candidate: {path}")
            if not saved:
                print("No new candidates found")
            logger.info("Sleeping %ss", args.interval)
            time.sleep(args.interval)
    else:
        saved = run_once(
            coins,
            db_dir=db_dir,
            shortlist_size=args.shortlist_size,
            strategy_names=args.strategy,
        )
        for path in saved:
            print(f"Candidate: {path}")
        if not saved:
            print("No new candidates found")


__all__ = [
    "compute_max_drawdown",
    "compute_sharpe",
    "is_result_significant",
    "run_once",
    "score_trades",
    "sort_results",
]


if __name__ == "__main__":
    main()
