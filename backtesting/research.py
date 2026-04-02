"""Autoresearch orchestration: grid-search strategies, validate OOS.

Usage:
    python backtesting/research.py
    python backtesting/research.py --min-markets 100 --test-pct 0.3
"""
from __future__ import annotations

import argparse
import logging
from pathlib import Path

from shared.fees import taker_fee

from backtesting.precompute import load_and_precompute
from backtesting.simulator import (
    StrategyResult,
    build_strategy_grid,
    simulate_strategy,
)

logger = logging.getLogger(__name__)

DB_PATH = Path(__file__).parent / "historical.db"


def run_autoresearch(
    db_path: Path = DB_PATH,
    test_pct: float = 0.3,
    min_markets: int = 30,
    coin: str = "btc",
) -> list[StrategyResult]:
    logger.info(f"Loading and precomputing features for {coin.upper()}...")
    all_markets = load_and_precompute(db_path, coin=coin)
    logger.info(f"Precomputed {len(all_markets)} {coin.upper()} markets")

    if len(all_markets) < min_markets:
        logger.error(f"Need at least {min_markets} markets, have {len(all_markets)}")
        return []

    split = int(len(all_markets) * (1 - test_pct))
    train = all_markets[:split]
    test = all_markets[split:]
    logger.info(f"Train: {len(train)}, Test: {len(test)}")

    strategy_defs = build_strategy_grid()
    logger.info(f"Testing {len(strategy_defs)} strategy configurations...")

    results = []
    for i, (name, params, fn) in enumerate(strategy_defs):
        tr_trades, tr_wins, tr_pnl, tr_avg = simulate_strategy(train, fn)
        if tr_trades < 5 or tr_pnl <= 0:
            continue

        te_trades, te_wins, te_pnl, te_avg = simulate_strategy(test, fn)
        if te_trades < 3 or te_pnl <= 0:
            continue

        avg_entry = (tr_avg + te_avg) / 2
        total_trades = tr_trades + te_trades
        total_pnl = tr_pnl + te_pnl

        results.append(StrategyResult(
            name=name, params=params,
            train_trades=tr_trades, train_wins=tr_wins, train_pnl=tr_pnl,
            test_trades=te_trades, test_wins=te_wins, test_pnl=te_pnl,
            avg_entry=avg_entry,
            avg_profit_per_trade=total_pnl / total_trades if total_trades else 0,
        ))

        if (i + 1) % 1000 == 0:
            logger.info(f"  [{i+1}/{len(strategy_defs)}] {len(results)} profitable so far")

    results.sort(key=lambda r: r.test_pnl_per_trade, reverse=True)
    logger.info(f"Done: {len(results)} strategies profitable on both train and test")
    return results


def main() -> None:
    logging.basicConfig(level=logging.INFO, format="%(asctime)s %(levelname)s %(message)s")
    parser = argparse.ArgumentParser()
    parser.add_argument("--db", type=str, default=str(DB_PATH))
    parser.add_argument("--test-pct", type=float, default=0.3)
    parser.add_argument("--min-markets", type=int, default=30)
    parser.add_argument("--coin", type=str, default="btc", choices=["btc", "eth", "sol"])
    args = parser.parse_args()

    results = run_autoresearch(Path(args.db), args.test_pct, args.min_markets, args.coin)

    if not results:
        print("\nNo profitable strategies found on both train and test sets.")
        return

    print(f"\n{'='*100}")
    print(f"AUTORESEARCH RESULTS: Top strategies profitable on held-out test data")
    print(f"{'='*100}\n")

    top = results[:30]
    print(f"{'Rank':<5} {'Strategy':<14} {'Train':<20} {'Test':<20} "
          f"{'Avg Entry':<10} {'Test $/trade':<12} {'Params'}")
    print("-" * 120)

    for i, r in enumerate(top):
        train_str = f"{r.train_wins}/{r.train_trades} ${r.train_pnl:+.2f}"
        test_str = f"{r.test_wins}/{r.test_trades} ${r.test_pnl:+.2f}"
        print(f"{i+1:<5} {r.name:<14} {train_str:<20} {test_str:<20} "
              f"{r.avg_entry:<10.3f} {r.test_pnl_per_trade:<+12.4f} {r.params}")

    print(f"\n{'='*100}")
    print("TOP 5 DETAILED")
    print(f"{'='*100}")

    for i, r in enumerate(top[:5]):
        print(f"\n#{i+1}: {r.name} {r.params}")
        print(f"  Train: {r.train_wins}/{r.train_trades} wins "
              f"({r.train_win_rate*100:.0f}%), P&L ${r.train_pnl:+.2f}")
        print(f"  Test:  {r.test_wins}/{r.test_trades} wins "
              f"({r.test_win_rate*100:.0f}%), P&L ${r.test_pnl:+.2f}")
        print(f"  Avg entry: ${r.avg_entry:.3f}")
        print(f"  Test P&L per trade: ${r.test_pnl_per_trade:+.4f}")
        fee_at_entry = taker_fee(r.avg_entry) * r.avg_entry
        breakeven_wr = (r.avg_entry + fee_at_entry) / (1.0 - fee_at_entry + r.avg_entry)
        print(f"  Breakeven win rate at this entry: {breakeven_wr*100:.0f}%")
        print(f"  Actual test win rate: {r.test_win_rate*100:.0f}%")
        margin = r.test_win_rate - breakeven_wr
        print(f"  Edge over breakeven: {margin*100:+.1f}pp")


if __name__ == "__main__":
    main()
