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

MIN_TRAIN_TRADES = 100
MIN_TEST_TRADES = 30
MIN_SHARPE = 0.5


def compute_sharpe(pnls: list[float]) -> float:
    """Sharpe ratio: mean / std of per-trade PnL. Returns 0 if undefined."""
    if len(pnls) < 2:
        return 0.0
    mean = sum(pnls) / len(pnls)
    variance = sum((p - mean) ** 2 for p in pnls) / (len(pnls) - 1)
    if variance <= 0:
        return 0.0
    std = variance ** 0.5
    return mean / std


def compute_max_drawdown(pnls: list[float]) -> float:
    """Max drawdown from cumulative PnL series. Returns positive number."""
    if not pnls:
        return 0.0
    cumulative = 0.0
    peak = 0.0
    max_dd = 0.0
    for p in pnls:
        cumulative += p
        if cumulative > peak:
            peak = cumulative
        dd = peak - cumulative
        if dd > max_dd:
            max_dd = dd
    return max_dd


def score_trades(pnls: list[float]) -> dict:
    """Compute full summary stats for a list of trade PnLs."""
    trades = len(pnls)
    if trades == 0:
        return {
            "trades": 0, "wins": 0, "losses": 0, "pnl": 0.0,
            "win_rate": 0.0, "pnl_per_trade": 0.0,
            "sharpe": 0.0, "max_drawdown": 0.0,
        }
    wins = sum(1 for p in pnls if p > 0)
    losses = trades - wins
    pnl = sum(pnls)
    return {
        "trades": trades,
        "wins": wins,
        "losses": losses,
        "pnl": round(pnl, 4),
        "win_rate": round(wins / trades, 4),
        "pnl_per_trade": round(pnl / trades, 4),
        "sharpe": round(compute_sharpe(pnls), 4),
        "max_drawdown": round(compute_max_drawdown(pnls), 4),
    }


def is_result_significant(result: dict) -> bool:
    """True if the result has enough data and strong enough stats to trust."""
    if result.get("train_trades", 0) < MIN_TRAIN_TRADES:
        return False
    if result.get("test_trades", 0) < MIN_TEST_TRADES:
        return False
    if result.get("train_pnl", 0) <= 0:
        return False
    if result.get("test_pnl", 0) <= 0:
        return False
    if result.get("test_sharpe", 0) < MIN_SHARPE:
        return False
    return True


def sort_results(results: list[dict]) -> list[dict]:
    """Sort results by test Sharpe ratio (descending)."""
    return sorted(results, key=lambda r: r.get("test_sharpe", 0), reverse=True)


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
            for min_e in [0.0, 0.40, 0.48]:
                for min_skew in [None, 0.02, 0.05, 0.10]:
                    for min_vol in [0, 0.002, 0.005]:
                        configs_tested += 1

                        scores = {}
                        for label, subset in [("train", train), ("test", test)]:
                            pnls: list[float] = []

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
                                if entry < min_e:
                                    continue

                                won = (d_up and m.winner == "Up") or (not d_up and m.winner == "Down")
                                fee = entry * taker_fee(entry)
                                pnl_trade = (1.0 - entry - fee) if won else -(entry + fee)
                                pnls.append(pnl_trade)

                            scores[label] = score_trades(pnls)

                        train_s = scores["train"]
                        test_s = scores["test"]

                        result = {
                            "name": "skew" if min_skew is not None else ("volatility" if min_vol > 0 else "threshold"),
                            "params": {"move": thresh, "max_entry": max_e, "min_entry": min_e, "skew": min_skew, "vol": min_vol},
                            "train_trades": train_s["trades"],
                            "train_wins": train_s["wins"],
                            "train_wr": train_s["win_rate"],
                            "train_pnl": train_s["pnl"],
                            "train_sharpe": train_s["sharpe"],
                            "train_max_drawdown": train_s["max_drawdown"],
                            "test_trades": test_s["trades"],
                            "test_wins": test_s["wins"],
                            "test_wr": test_s["win_rate"],
                            "test_pnl": test_s["pnl"],
                            "test_pnl_per_trade": test_s["pnl_per_trade"],
                            "test_sharpe": test_s["sharpe"],
                            "test_max_drawdown": test_s["max_drawdown"],
                        }

                        if not is_result_significant(result):
                            continue

                        results.append(result)

    results = sort_results(results)
    logger.info(f"[{coin.upper()}] Tested {configs_tested} configs, "
                f"{len(results)} passed significance filter "
                f"(min {MIN_TRAIN_TRADES} train, {MIN_TEST_TRADES} test, Sharpe>{MIN_SHARPE})")
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
            f"Sharpe={best['test_sharpe']:.2f} "
            f"${best['test_pnl_per_trade']:+.4f}/trade ({elapsed:.1f}s)"
        )

        previous = current.get(coin, {}).get("best_strategy", {})
        prev_score = previous.get("test_sharpe", float("-inf"))

        if best["test_sharpe"] > prev_score:
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
