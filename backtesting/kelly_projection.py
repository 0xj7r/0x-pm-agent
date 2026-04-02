"""Project P&L over time using Kelly-optimized sizing.

Takes the top autoresearch strategies and simulates them sequentially
through markets with Kelly bet sizing, starting from $100.

Usage:
    python backtesting/kelly_projection.py
    python backtesting/kelly_projection.py --strategy volatility --move 0.08 --max-entry 0.55
"""
from __future__ import annotations

import argparse
import logging
import math
import sqlite3
from dataclasses import dataclass
from pathlib import Path

import sys
sys.path.insert(0, str(Path(__file__).parent.parent))
from backtesting.autoresearch import load_and_precompute, PrecomputedMarket

logger = logging.getLogger(__name__)

DB_PATH = Path(__file__).parent / "historical.db"
TAKER_FEE = 0.02


@dataclass
class Trade:
    market_id: str
    direction: str
    winner: str
    won: bool
    entry_price: float
    bet_size: float
    shares: float
    pnl: float
    balance_before: float
    balance_after: float
    kelly_fraction: float
    snap_index: int
    total_snaps: int


def kelly_fraction(win_rate: float, entry_price: float, fee: float = TAKER_FEE) -> float:
    """Compute Kelly fraction for a binary outcome bet."""
    cost = entry_price + fee * entry_price
    net_win = 1.0 - cost
    net_loss = cost

    if net_win <= 0 or net_loss <= 0:
        return 0.0

    b = net_win / net_loss
    p = win_rate
    q = 1.0 - p

    f = (b * p - q) / b
    return max(0.0, f)


def run_projection(
    db_path: Path = DB_PATH,
    starting_balance: float = 100.0,
    kelly_mult: float = 0.5,
    max_bet_pct: float = 0.25,
    min_bet: float = 1.0,
    strategy: str = "volatility",
    move_thresh: float = 0.08,
    vol_thresh: float = 0.002,
    accel_thresh: float = 0.01,
    max_entry: float = 0.55,
    estimated_win_rate: float = 0.95,
) -> tuple[list[Trade], float]:
    logger.info("Loading markets...")
    markets = load_and_precompute(db_path)
    logger.info(f"Loaded {len(markets)} markets, projecting with ${starting_balance:.0f} start")

    balance = starting_balance
    trades: list[Trade] = []

    for pm in markets:
        for i in range(10, pm.num_snaps):
            if pm.abs_move[i] < move_thresh:
                continue

            # Strategy-specific filters (once move threshold is hit, decide now)
            if strategy == "volatility" and pm.volatility[i] < vol_thresh:
                break
            elif strategy == "acceleration":
                d = "Up" if pm.move_pct[i] > 0 else "Down"
                if d == "Up" and pm.acceleration[i] < accel_thresh:
                    break
                if d == "Down" and pm.acceleration[i] > -accel_thresh:
                    break

            direction = "Up" if pm.move_pct[i] > 0 else "Down"
            entry = pm.price_up[i] if direction == "Up" else pm.price_down[i]

            if entry <= 0 or entry > max_entry:
                break  # first signal crossed, but entry too expensive; no trade

            kf = kelly_fraction(estimated_win_rate, entry)
            adjusted_kf = kf * kelly_mult
            bet_size = balance * min(adjusted_kf, max_bet_pct)

            if bet_size < min_bet:
                break

            shares = bet_size / entry
            won = direction == pm.winner
            fee = TAKER_FEE * entry
            pnl = shares * (1.0 - entry - fee) if won else -(shares * (entry + fee))

            balance_before = balance
            balance += pnl

            trades.append(Trade(
                market_id=pm.market_id,
                direction=direction,
                winner=pm.winner,
                won=won,
                entry_price=entry,
                bet_size=bet_size,
                shares=shares,
                pnl=pnl,
                balance_before=balance_before,
                balance_after=balance,
                kelly_fraction=adjusted_kf,
                snap_index=i,
                total_snaps=pm.num_snaps,
            ))
            break

    return trades, balance


def main() -> None:
    logging.basicConfig(level=logging.INFO, format="%(asctime)s %(levelname)s %(message)s")
    parser = argparse.ArgumentParser()
    parser.add_argument("--db", type=str, default=str(DB_PATH))
    parser.add_argument("--start", type=float, default=100.0)
    parser.add_argument("--kelly-mult", type=float, default=0.5,
                        help="Kelly multiplier (0.5 = half Kelly)")
    parser.add_argument("--max-bet-pct", type=float, default=0.25)
    parser.add_argument("--strategy", type=str, default="volatility",
                        choices=["volatility", "acceleration", "threshold"])
    parser.add_argument("--move", type=float, default=0.08)
    parser.add_argument("--vol", type=float, default=0.002)
    parser.add_argument("--accel", type=float, default=0.01)
    parser.add_argument("--max-entry", type=float, default=0.55)
    parser.add_argument("--win-rate", type=float, default=0.95,
                        help="Estimated win rate for Kelly sizing")
    args = parser.parse_args()

    trades, final = run_projection(
        db_path=Path(args.db),
        starting_balance=args.start,
        kelly_mult=args.kelly_mult,
        max_bet_pct=args.max_bet_pct,
        strategy=args.strategy,
        move_thresh=args.move,
        vol_thresh=args.vol,
        accel_thresh=args.accel,
        max_entry=args.max_entry,
        estimated_win_rate=args.win_rate,
    )

    if not trades:
        print("No trades triggered.")
        return

    wins = [t for t in trades if t.won]
    losses = [t for t in trades if not t.won]

    print(f"\n{'='*90}")
    print(f"KELLY PROJECTION: {args.strategy} strategy")
    print(f"{'='*90}")
    print(f"Start: ${args.start:.2f} | Final: ${final:.2f} | "
          f"Return: {(final/args.start - 1)*100:+.1f}%")
    print(f"Trades: {len(trades)} | Wins: {len(wins)} | Losses: {len(losses)} | "
          f"Win rate: {len(wins)/len(trades)*100:.0f}%")
    print(f"Kelly mult: {args.kelly_mult} | Max bet: {args.max_bet_pct*100:.0f}% | "
          f"Move thresh: {args.move}%")
    print(f"Max entry: ${args.max_entry}")
    print(f"{'='*90}\n")

    # Equity curve
    print(f"{'#':<4} {'Market':<12} {'Dir':<5} {'Won':<4} {'Entry$':<7} "
          f"{'Kelly%':<7} {'Bet$':<10} {'P&L$':<10} {'Balance$':<12}")
    print("-" * 90)

    peak = args.start
    max_dd = 0.0
    for i, t in enumerate(trades):
        peak = max(peak, t.balance_after)
        dd = (peak - t.balance_after) / peak if peak > 0 else 0
        max_dd = max(max_dd, dd)

        print(f"{i+1:<4} {t.market_id:<12} {t.direction:<5} "
              f"{'Y' if t.won else 'N':<4} {t.entry_price:<7.3f} "
              f"{t.kelly_fraction*100:<7.1f} {t.bet_size:<10.2f} "
              f"{t.pnl:<+10.2f} {t.balance_after:<12.2f}")

    print(f"\n{'='*90}")
    print("SUMMARY")
    print(f"{'='*90}")
    print(f"Starting balance:  ${args.start:>12.2f}")
    print(f"Final balance:     ${final:>12.2f}")
    print(f"Total P&L:         ${final - args.start:>+12.2f}")
    print(f"Return:            {(final/args.start - 1)*100:>+11.1f}%")
    print(f"Max drawdown:      {max_dd*100:>11.1f}%")
    print(f"Trades:            {len(trades):>12}")
    print(f"Win rate:          {len(wins)/len(trades)*100:>11.0f}%")
    if wins:
        print(f"Avg win:           ${sum(t.pnl for t in wins)/len(wins):>+12.2f}")
    if losses:
        print(f"Avg loss:          ${sum(t.pnl for t in losses)/len(losses):>+12.2f}")
    print(f"Avg bet size:      ${sum(t.bet_size for t in trades)/len(trades):>12.2f}")

    # Milestones
    milestones = [200, 500, 1000, 5000, 10000, 50000, 100000]
    print(f"\nMilestones:")
    for m in milestones:
        hit = next((i+1 for i, t in enumerate(trades) if t.balance_after >= m), None)
        if hit:
            print(f"  ${m:>8,} reached after trade #{hit}")


if __name__ == "__main__":
    main()
