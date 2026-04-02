"""Monte Carlo and Kelly projection for P&L simulation.

Merges stochastic simulation (multi-scenario Monte Carlo with slippage,
liquidity caps) and Kelly-optimized sequential projection into a single
MonteCarloSimulator class.

Usage:
    python backtesting/projection.py
"""
from __future__ import annotations

import argparse
import logging
import math
import random
import sqlite3
import sys
from dataclasses import dataclass
from pathlib import Path

sys.path.insert(0, str(Path(__file__).parent.parent))

from backtesting.precompute import load_and_precompute, PrecomputedMarket
from shared.fees import taker_fee

logger = logging.getLogger(__name__)
DB_PATH = Path(__file__).parent / "historical.db"

from shared.fees import taker_fee as _taker_fee


def slippage(bet_size: float, available_liquidity: float) -> float:
    """Estimate price impact as fraction of entry price.

    Linear model: filling X% of book moves price by X% * impact_factor.
    At 20% fill, ~2% slippage. At 50% fill, ~5%.
    """
    fill_pct = bet_size / available_liquidity if available_liquidity > 0 else 1.0
    impact_factor = 0.10
    return min(fill_pct * impact_factor, 0.15)


@dataclass
class ObservedTrade:
    market_id: str
    entry_price: float
    direction: str
    winner: str
    won: bool
    liquidity: float


@dataclass
class KellyTrade:
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


def kelly_fraction(win_rate: float, entry_price: float, fee: float | None = None) -> float:
    """Compute Kelly fraction for a binary outcome bet."""
    if fee is None:
        fee = _taker_fee(entry_price)
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


class MonteCarloSimulator:
    """Unified Monte Carlo and Kelly projection engine."""

    def __init__(self, db_path: Path = DB_PATH):
        self.db_path = db_path

    def extract_trades(
        self,
        move_thresh: float = 0.08,
        max_entry: float = 0.55,
    ) -> list[ObservedTrade]:
        """Extract trades that would have fired from the strategy."""
        markets_data = load_and_precompute(self.db_path)

        conn = sqlite3.connect(str(self.db_path))
        conn.row_factory = sqlite3.Row
        liquidity_map = {}
        for row in conn.execute("SELECT market_id, final_liquidity FROM markets").fetchall():
            liquidity_map[row["market_id"]] = row["final_liquidity"] or 15000

        trades = []
        for pm in markets_data:
            for i in range(10, pm.num_snaps):
                if pm.abs_move[i] < move_thresh:
                    continue
                d = "Up" if pm.move_pct[i] > 0 else "Down"
                e = pm.price_up[i] if d == "Up" else pm.price_down[i]
                if e <= 0 or e > max_entry:
                    break
                trades.append(ObservedTrade(
                    market_id=pm.market_id,
                    entry_price=e,
                    direction=d,
                    winner=pm.winner,
                    won=(d == pm.winner),
                    liquidity=liquidity_map.get(pm.market_id, 15000),
                ))
                break

        conn.close()
        return trades

    def run_scenarios(
        self,
        observed_trades: list[ObservedTrade],
        win_rate: float,
        starting_balance: float = 100.0,
        bet_pct: float = 0.10,
        liquidity_fill_pct: float = 0.20,
        num_simulations: int = 1000,
        months: int = 6,
        trades_per_day: float = 10.0,
    ) -> dict:
        """Run multi-scenario Monte Carlo simulation with slippage and fees."""
        if not observed_trades:
            return {}

        entry_prices = [t.entry_price for t in observed_trades]
        liquidities = [t.liquidity for t in observed_trades]

        total_trades = int(trades_per_day * 30 * months)
        trades_per_month = int(trades_per_day * 30)

        all_final: list[float] = []
        all_max_dd: list[float] = []
        ruin_count = 0
        monthly_snaps: dict[int, list[float]] = {m: [] for m in range(1, months + 1)}

        for sim in range(num_simulations):
            rng = random.Random(sim * 7919 + 42)
            balance = starting_balance
            peak = starting_balance
            max_dd = 0.0

            for trade_num in range(total_trades):
                entry = rng.choice(entry_prices)
                fee_rate = taker_fee(entry)
                liq = rng.choice(liquidities)

                uncapped_bet = balance * bet_pct
                liq_cap = liq * liquidity_fill_pct
                bet = min(uncapped_bet, liq_cap)

                if bet < 1.0:
                    ruin_count += 1
                    balance = 0
                    break

                slip = slippage(bet, liq)
                effective_entry = entry * (1 + slip)
                if effective_entry >= 0.99:
                    continue

                shares = bet / effective_entry
                fee_usd = bet * fee_rate

                won = rng.random() < win_rate
                if won:
                    pnl = shares * 1.0 - bet - fee_usd
                else:
                    pnl = -bet - fee_usd

                balance += pnl
                if balance <= 0:
                    ruin_count += 1
                    balance = 0
                    break

                peak = max(peak, balance)
                dd = (peak - balance) / peak if peak > 0 else 0
                max_dd = max(max_dd, dd)

                month_num = trade_num // trades_per_month + 1
                if month_num <= months and (trade_num + 1) % trades_per_month == 0:
                    monthly_snaps[month_num].append(balance)

            all_final.append(balance)
            all_max_dd.append(max_dd)

        all_final.sort()
        all_max_dd.sort()
        n = len(all_final)

        return {
            "num_simulations": num_simulations,
            "months": months,
            "total_trades": total_trades,
            "median_final": all_final[n // 2],
            "p5_final": all_final[int(n * 0.05)],
            "p25_final": all_final[int(n * 0.25)],
            "p75_final": all_final[int(n * 0.75)],
            "p95_final": all_final[int(n * 0.95)],
            "mean_final": sum(all_final) / n,
            "ruin_pct": ruin_count / num_simulations * 100,
            "median_max_dd": all_max_dd[n // 2],
            "p95_max_dd": all_max_dd[int(n * 0.95)],
            "monthly_snapshots": {
                m: {
                    "median": sorted(vals)[len(vals) // 2] if vals else 0,
                    "p5": sorted(vals)[int(len(vals) * 0.05)] if vals else 0,
                    "p95": sorted(vals)[int(len(vals) * 0.95)] if vals else 0,
                }
                for m, vals in monthly_snaps.items()
            },
        }

    def run_kelly_projection(
        self,
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
    ) -> tuple[list[KellyTrade], float]:
        """Run sequential Kelly-sized projection through historical markets."""
        markets = load_and_precompute(self.db_path)
        balance = starting_balance
        trades: list[KellyTrade] = []

        for pm in markets:
            for i in range(10, pm.num_snaps):
                if pm.abs_move[i] < move_thresh:
                    continue

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
                    break

                kf = kelly_fraction(estimated_win_rate, entry)
                adjusted_kf = kf * kelly_mult
                bet_size = balance * min(adjusted_kf, max_bet_pct)

                if bet_size < min_bet:
                    break

                shares = bet_size / entry
                won = direction == pm.winner
                fee = _taker_fee(entry) * entry
                pnl = shares * (1.0 - entry - fee) if won else -(shares * (entry + fee))

                balance_before = balance
                balance += pnl

                trades.append(KellyTrade(
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


# Backward-compatible aliases


if __name__ == "__main__":
    logging.basicConfig(level=logging.INFO, format="%(asctime)s %(levelname)s %(message)s")
    parser = argparse.ArgumentParser()
    parser.add_argument("--db", type=str, default=str(DB_PATH))
    parser.add_argument("--start", type=float, default=100.0)
    parser.add_argument("--months", type=int, default=6)
    parser.add_argument("--sims", type=int, default=5000)
    parser.add_argument("--move", type=float, default=0.08)
    parser.add_argument("--max-entry", type=float, default=0.55)
    args = parser.parse_args()

    sim = MonteCarloSimulator(Path(args.db))
    trades = sim.extract_trades(args.move, args.max_entry)

    if not trades:
        print("No trades found.")
        sys.exit(0)

    win_rate = sum(1 for t in trades if t.won) / len(trades)
    print(f"Found {len(trades)} trades, WR={win_rate:.1%}")

    result = sim.run_scenarios(
        trades, win_rate,
        starting_balance=args.start,
        num_simulations=args.sims,
        months=args.months,
    )

    print(f"Median final: ${result['median_final']:,.0f}")
    print(f"P5-P95: ${result['p5_final']:,.0f} - ${result['p95_final']:,.0f}")
