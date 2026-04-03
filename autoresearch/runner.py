"""Continuous autoresearch runner.

Pulls latest data, builds feature store, grid-searches strategies,
saves candidates for human review. Does NOT auto-deploy.

Usage:
    python autoresearch/runner.py                    # one-shot, all coins
    python autoresearch/runner.py --coin btc          # one coin
    python autoresearch/runner.py --loop --interval 3600  # continuous, hourly
"""
from __future__ import annotations

import argparse
import asyncio
import json
import logging
import time
from datetime import datetime, timezone
from pathlib import Path

import sys
sys.path.insert(0, str(Path(__file__).parent.parent))

from backtesting.feature_store import build_feature_store, load_feature_store, FEATURE_NAMES
from shared.fees import taker_fee
from shared.constants import COINS, db_path as default_db_path

logger = logging.getLogger(__name__)

BASE_DIR = Path(__file__).resolve().parent.parent
RESULTS_PATH = BASE_DIR / "backtesting" / "strategy_results.json"
CANDIDATES_DIR = BASE_DIR / "autoresearch" / "candidates"


def _current_best() -> dict:
    if not RESULTS_PATH.exists():
        return {}
    return json.loads(RESULTS_PATH.read_text())


def _save_candidate(coin: str, candidate: dict) -> Path:
    CANDIDATES_DIR.mkdir(parents=True, exist_ok=True)
    stamp = datetime.now(timezone.utc).strftime("%Y%m%dT%H%M%SZ")
    path = CANDIDATES_DIR / f"{stamp}-{coin}.json"
    path.write_text(json.dumps(candidate, indent=2))
    return path


def run_grid_search(coin: str, db_dir: Path | None = None) -> list[dict]:
    """Fast grid search using the feature store."""
    try:
        manifest, f = load_feature_store(coin)
    except FileNotFoundError:
        db = Path(db_dir) / f"{coin}.db" if db_dir else default_db_path(coin)
        if not db.exists():
            logger.warning(f"[{coin.upper()}] No DB found")
            return []
        logger.info(f"[{coin.upper()}] Building feature store...")
        build_feature_store(db, coin)
        manifest, f = load_feature_store(coin)

    if len(manifest) < 30:
        logger.warning(f"[{coin.upper()}] Only {len(manifest)} markets, skipping")
        return []

    split = int(len(manifest) * 0.7)
    train, test = manifest[:split], manifest[split:]

    import numpy as np

    results = []
    configs_tested = 0

    for thresh in [0.03, 0.05, 0.08, 0.10, 0.12, 0.15, 0.18, 0.20, 0.25]:
        for max_e in [0.50, 0.55, 0.60, 0.65, 0.75]:
            for min_skew in [None, 0.02, 0.05, 0.10]:
                for min_vol in [0, 0.002, 0.005]:
                    configs_tested += 1

                    scores = {}
                    for label, subset in [("train", train), ("test", test)]:
                        trades = wins = 0
                        pnl = 0.0

                        for m in subset:
                            o, n = m.offset, m.length
                            am = f["abs_move"][o:o+n]

                            above = np.where(am[10:] >= thresh)[0]
                            if len(above) == 0:
                                continue
                            idx = above[0] + 10

                            if min_vol > 0 and f["volatility"][o+idx] < min_vol:
                                continue
                            if min_skew is not None and f["token_skew"][o+idx] > min_skew:
                                continue

                            mp = f["move_pct"][o+idx]
                            d_up = mp > 0
                            entry = float(f["price_up"][o+idx] if d_up else f["price_down"][o+idx])

                            if entry <= 0 or entry > max_e:
                                continue

                            trades += 1
                            won = (d_up and m.winner == "Up") or (not d_up and m.winner == "Down")
                            fee = entry * taker_fee(entry)
                            pnl += (1.0 - entry - fee) if won else -(entry + fee)
                            if won:
                                wins += 1

                        scores[label] = (trades, wins, pnl)

                    tr_t, tr_w, tr_p = scores["train"]
                    te_t, te_w, te_p = scores["test"]

                    if tr_t < 5 or te_t < 3 or tr_p <= 0 or te_p <= 0:
                        continue

                    results.append({
                        "name": "skew" if min_skew is not None else ("volatility" if min_vol > 0 else "threshold"),
                        "params": {"move": thresh, "max_entry": max_e, "skew": min_skew, "vol": min_vol},
                        "train_trades": tr_t,
                        "train_wins": tr_w,
                        "train_wr": round(tr_w / tr_t, 4),
                        "test_trades": te_t,
                        "test_wins": te_w,
                        "test_wr": round(te_w / te_t, 4),
                        "test_pnl": round(te_p, 4),
                        "test_pnl_per_trade": round(te_p / te_t, 4),
                    })

    results.sort(key=lambda r: r["test_pnl_per_trade"], reverse=True)
    logger.info(f"[{coin.upper()}] Tested {configs_tested} configs, "
                f"{len(results)} profitable on both sets")
    return results


def run_once(coins: list[str], db_dir: Path | None = None) -> list[Path]:
    current = _current_best()
    saved: list[Path] = []

    for coin in coins:
        logger.info(f"[{coin.upper()}] Running autoresearch...")
        t0 = time.time()
        results = run_grid_search(coin, db_dir=db_dir)
        elapsed = time.time() - t0

        if not results:
            logger.info(f"[{coin.upper()}] No profitable strategies found ({elapsed:.1f}s)")
            continue

        best = results[0]
        logger.info(
            f"[{coin.upper()}] Best: {best['name']} {best['params']} "
            f"test={best['test_wins']}/{best['test_trades']} ({best['test_wr']:.0%}) "
            f"${best['test_pnl_per_trade']:+.4f}/trade ({elapsed:.1f}s)"
        )

        previous = current.get(coin, {}).get("best_strategy", {})
        prev_score = previous.get("test_pnl_per_trade", float("-inf"))

        if best["test_pnl_per_trade"] > prev_score:
            candidate = {
                "coin": coin,
                "created_at": datetime.now(timezone.utc).isoformat(),
                "current_best": previous,
                "proposed": best,
                "top_5": results[:5],
                "total_profitable": len(results),
                "search_time_s": round(elapsed, 1),
            }
            path = _save_candidate(coin, candidate)
            saved.append(path)
            logger.info(f"[{coin.upper()}] Candidate saved: {path.name}")
        else:
            logger.info(f"[{coin.upper()}] No improvement over current best")

    return saved


def main():
    logging.basicConfig(level=logging.INFO, format="%(asctime)s %(levelname)s %(message)s")
    parser = argparse.ArgumentParser()
    parser.add_argument("--coin", action="append", choices=COINS)
    parser.add_argument("--loop", action="store_true")
    parser.add_argument("--interval", type=int, default=86400, help="Seconds between runs in loop mode")
    parser.add_argument("--db-dir", type=str, default=None, help="Directory containing coin DBs")
    args = parser.parse_args()

    coins = args.coin or COINS
    db_dir = Path(args.db_dir) if args.db_dir else None

    if args.loop:
        logger.info(f"Starting autoresearch loop (interval={args.interval}s, coins={coins})")
        while True:
            try:
                saved = run_once(coins, db_dir=db_dir)
                for p in saved:
                    logger.info(f"Candidate: {p}")
            except Exception as e:
                logger.error(f"Autoresearch failed: {e}", exc_info=True)
            logger.info(f"Sleeping {args.interval}s...")
            time.sleep(args.interval)
    else:
        saved = run_once(coins, db_dir=db_dir)
        for p in saved:
            print(f"Candidate: {p}")
        if not saved:
            print("No new candidates found")


if __name__ == "__main__":
    main()
