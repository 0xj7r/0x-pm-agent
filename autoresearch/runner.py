"""Continuous autoresearch runner for threshold strategy candidates."""
from __future__ import annotations

import argparse
import asyncio
import json
from datetime import datetime, timezone
from pathlib import Path

from backtesting.research import run_autoresearch
from core.notifier import SlackNotifier

BASE_DIR = Path(__file__).resolve().parent.parent
RESULTS_PATH = BASE_DIR / "backtesting" / "strategy_results.json"
CANDIDATES_DIR = BASE_DIR / "autoresearch" / "candidates"


def _current_best() -> dict:
    if not RESULTS_PATH.exists():
        return {}
    return json.loads(RESULTS_PATH.read_text())


def _write_candidate(coin: str, candidate: dict) -> Path:
    CANDIDATES_DIR.mkdir(parents=True, exist_ok=True)
    stamp = datetime.now(timezone.utc).strftime("%Y%m%dT%H%M%SZ")
    path = CANDIDATES_DIR / f"{stamp}-{coin}.json"
    path.write_text(json.dumps(candidate, indent=2))
    return path


async def run_once(coins: list[str], min_markets: int = 50) -> list[Path]:
    current = _current_best()
    notifier = SlackNotifier()
    saved: list[Path] = []

    try:
        for coin in coins:
            db_path = BASE_DIR / "backtesting" / f"{coin}.db"
            if not db_path.exists():
                continue

            results = run_autoresearch(db_path, test_pct=0.3, min_markets=min_markets, coin=coin)
            if not results:
                continue

            best = results[0]
            previous = current.get(coin, {}).get("best_strategy", {})
            previous_score = previous.get("test_pnl_per_trade", float("-inf"))

            if best.test_pnl_per_trade <= previous_score:
                continue

            candidate = {
                "coin": coin,
                "created_at": datetime.now(timezone.utc).isoformat(),
                "current_best": previous,
                "candidate_best": {
                    "name": best.name,
                    "params": best.params,
                    "train_win_rate": best.train_win_rate,
                    "test_win_rate": best.test_win_rate,
                    "train_trades": best.train_trades,
                    "test_trades": best.test_trades,
                    "train_pnl": best.train_pnl,
                    "test_pnl": best.test_pnl,
                    "avg_entry": best.avg_entry,
                    "test_pnl_per_trade": best.test_pnl_per_trade,
                },
            }
            path = _write_candidate(coin, candidate)
            saved.append(path)

            await notifier._send(
                f"*Autoresearch candidate found*\n"
                f"Coin: {coin.upper()}\n"
                f"Current: {previous.get('name', 'none')} {previous.get('params', {})}\n"
                f"Candidate: {best.name} {best.params}\n"
                f"Test $/trade: {best.test_pnl_per_trade:+.4f}\n"
                f"Saved: {path.name}"
            )
    finally:
        await notifier.close()

    return saved


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--coin", action="append", choices=["btc", "eth", "sol"])
    parser.add_argument("--min-markets", type=int, default=50)
    args = parser.parse_args()
    coins = args.coin or ["btc", "eth", "sol"]
    saved = asyncio.run(run_once(coins, min_markets=args.min_markets))
    for path in saved:
        print(path)


if __name__ == "__main__":
    main()
