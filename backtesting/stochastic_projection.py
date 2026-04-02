"""Stochastic P&L projection with realistic constraints.

Monte Carlo simulation using observed trade parameters from
autoresearch, with proper fee model, slippage, and liquidity caps.

Usage:
    python backtesting/stochastic_projection.py
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

from backtesting.autoresearch import load_and_precompute

logger = logging.getLogger(__name__)
DB_PATH = Path(__file__).parent / "historical.db"


def polymarket_fee(price: float) -> float:
    """Polymarket dynamic taker fee per dollar: 0.072 * p * (1-p)."""
    return 0.072 * price * (1.0 - price)


def slippage(bet_size: float, available_liquidity: float) -> float:
    """Estimate price impact as fraction of entry price.

    Linear model: filling X% of book moves price by X% * impact_factor.
    At 20% fill, ~2% slippage. At 50% fill, ~5%.
    """
    fill_pct = bet_size / available_liquidity if available_liquidity > 0 else 1.0
    impact_factor = 0.10  # 10% of fill percentage
    return min(fill_pct * impact_factor, 0.15)  # cap at 15%


@dataclass
class ObservedTrade:
    market_id: str
    entry_price: float
    direction: str
    winner: str
    won: bool
    liquidity: float


def extract_observed_trades(
    db_path: Path = DB_PATH,
    move_thresh: float = 0.08,
    max_entry: float = 0.55,
) -> list[ObservedTrade]:
    """Extract trades that would have fired from the strategy."""
    markets_data = load_and_precompute(db_path)

    conn = sqlite3.connect(str(db_path))
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


def run_monte_carlo(
    observed_trades: list[ObservedTrade],
    starting_balance: float = 100.0,
    bet_pct: float = 0.10,
    liquidity_fill_pct: float = 0.20,
    num_simulations: int = 1000,
    months: int = 6,
    trades_per_day: float = 10.0,
) -> dict:
    """Run Monte Carlo simulation using observed trade distribution."""
    if not observed_trades:
        return {}

    win_rate = sum(1 for t in observed_trades if t.won) / len(observed_trades)
    entry_prices = [t.entry_price for t in observed_trades]
    liquidities = [t.liquidity for t in observed_trades]
    avg_entry = sum(entry_prices) / len(entry_prices)
    avg_liquidity = sum(liquidities) / len(liquidities)
    max_bet_liquidity = avg_liquidity * liquidity_fill_pct

    total_trades = int(trades_per_day * 30 * months)

    all_final_balances = []
    all_max_drawdowns = []
    all_ruin_count = 0
    monthly_snapshots = {m: [] for m in range(1, months + 1)}
    sample_paths = []

    for sim in range(num_simulations):
        rng = random.Random(sim * 7919 + 42)
        balance = starting_balance
        peak = starting_balance
        max_dd = 0.0
        path = [balance]

        for trade_num in range(total_trades):
            entry = rng.choice(entry_prices)
            fee_rate = polymarket_fee(entry)

            uncapped_bet = balance * bet_pct
            liq_cap = rng.choice(liquidities) * liquidity_fill_pct
            bet = min(uncapped_bet, liq_cap)

            if bet < 1.0:
                all_ruin_count += 1
                break

            slip = slippage(bet, rng.choice(liquidities))
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
            peak = max(peak, balance)
            dd = (peak - balance) / peak if peak > 0 else 0
            max_dd = max(max_dd, dd)

            trades_per_month = int(trades_per_day * 30)
            month_num = trade_num // trades_per_month + 1
            if month_num <= months and (trade_num + 1) % trades_per_month == 0:
                monthly_snapshots[month_num].append(balance)

            if balance <= 0:
                all_ruin_count += 1
                break

        if sim < 5:
            path.append(balance)
            sample_paths.append((sim, balance, max_dd))

        all_final_balances.append(balance)
        all_max_drawdowns.append(max_dd)

    all_final_balances.sort()
    all_max_drawdowns.sort()
    n = len(all_final_balances)

    return {
        "num_simulations": num_simulations,
        "months": months,
        "total_trades": total_trades,
        "observed_win_rate": win_rate,
        "avg_entry": avg_entry,
        "avg_liquidity": avg_liquidity,
        "max_bet_liquidity": max_bet_liquidity,
        "median_final": all_final_balances[n // 2],
        "p5_final": all_final_balances[int(n * 0.05)],
        "p25_final": all_final_balances[int(n * 0.25)],
        "p75_final": all_final_balances[int(n * 0.75)],
        "p95_final": all_final_balances[int(n * 0.95)],
        "mean_final": sum(all_final_balances) / n,
        "ruin_pct": all_ruin_count / num_simulations * 100,
        "median_max_dd": all_max_drawdowns[n // 2],
        "p95_max_dd": all_max_drawdowns[int(n * 0.95)],
        "monthly_snapshots": {
            m: {
                "median": sorted(vals)[len(vals) // 2] if vals else 0,
                "p5": sorted(vals)[int(len(vals) * 0.05)] if vals else 0,
                "p95": sorted(vals)[int(len(vals) * 0.95)] if vals else 0,
            }
            for m, vals in monthly_snapshots.items()
        },
    }


def main() -> None:
    logging.basicConfig(level=logging.INFO, format="%(asctime)s %(levelname)s %(message)s")
    parser = argparse.ArgumentParser()
    parser.add_argument("--db", type=str, default=str(DB_PATH))
    parser.add_argument("--start", type=float, default=100.0)
    parser.add_argument("--bet-pct", type=float, default=0.10)
    parser.add_argument("--months", type=int, default=6)
    parser.add_argument("--sims", type=int, default=5000)
    parser.add_argument("--trades-per-day", type=float, default=10.0)
    parser.add_argument("--move", type=float, default=0.08)
    parser.add_argument("--max-entry", type=float, default=0.55)
    parser.add_argument("--liquidity-fill", type=float, default=0.20)
    args = parser.parse_args()

    logger.info("Extracting observed trades from backtest...")
    trades = extract_observed_trades(Path(args.db), args.move, args.max_entry)
    logger.info(f"Found {len(trades)} observed trades")

    if not trades:
        print("No trades found.")
        return

    win_rate = sum(1 for t in trades if t.won) / len(trades)
    avg_entry = sum(t.entry_price for t in trades) / len(trades)
    avg_liq = sum(t.liquidity for t in trades) / len(trades)

    print(f"\n{'='*80}")
    print(f"OBSERVED TRADE STATISTICS")
    print(f"{'='*80}")
    print(f"Trades: {len(trades)}")
    print(f"Win rate: {win_rate*100:.1f}%")
    print(f"Avg entry: ${avg_entry:.3f}")
    print(f"Avg market liquidity: ${avg_liq:,.0f}")
    print(f"Max bet (at {args.liquidity_fill*100:.0f}% fill): ${avg_liq * args.liquidity_fill:,.0f}")

    # Run for multiple win rate scenarios
    for label, wr_override in [
        ("OBSERVED", None),
        ("DEGRADED (-10pp)", -0.10),
        ("DEGRADED (-20pp)", -0.20),
    ]:
        if wr_override is not None:
            adjusted_trades = []
            for t in trades:
                adjusted_wr = win_rate + wr_override
                adjusted_trades.append(ObservedTrade(
                    market_id=t.market_id,
                    entry_price=t.entry_price,
                    direction=t.direction,
                    winner=t.winner,
                    won=t.won,
                    liquidity=t.liquidity,
                ))
            effective_wr = win_rate + wr_override
        else:
            adjusted_trades = trades
            effective_wr = win_rate

        # Temporarily override win rate in simulation
        logger.info(f"Running {args.sims} simulations for {label} (WR={effective_wr*100:.0f}%)...")

        # Run simulation with adjusted win rate
        result = _run_with_win_rate(
            trades, effective_wr, args.start, args.bet_pct,
            args.liquidity_fill, args.sims, args.months, args.trades_per_day,
        )

        print(f"\n{'='*80}")
        print(f"MONTE CARLO: {label} (WR={effective_wr*100:.0f}%)")
        print(f"{'='*80}")
        print(f"Simulations: {result['num_simulations']} | "
              f"Period: {result['months']}mo | "
              f"Trades: {result['total_trades']}")
        print(f"Bet size: {args.bet_pct*100:.0f}% of bankroll, "
              f"capped at {args.liquidity_fill*100:.0f}% of market liquidity")
        print(f"Includes: dynamic fees, slippage model, liquidity cap")
        print()

        print(f"{'Metric':<25} {'Value':<15}")
        print("-" * 40)
        print(f"{'Median final balance':<25} ${result['median_final']:>12,.0f}")
        print(f"{'Mean final balance':<25} ${result['mean_final']:>12,.0f}")
        print(f"{'5th percentile':<25} ${result['p5_final']:>12,.0f}")
        print(f"{'25th percentile':<25} ${result['p25_final']:>12,.0f}")
        print(f"{'75th percentile':<25} ${result['p75_final']:>12,.0f}")
        print(f"{'95th percentile':<25} ${result['p95_final']:>12,.0f}")
        print(f"{'Ruin probability':<25} {result['ruin_pct']:>11.1f}%")
        print(f"{'Median max drawdown':<25} {result['median_max_dd']*100:>11.1f}%")
        print(f"{'95th pctl max drawdown':<25} {result['p95_max_dd']*100:>11.1f}%")

        if result["monthly_snapshots"]:
            print(f"\n{'Month':<8} {'P5':<14} {'Median':<14} {'P95':<14}")
            print("-" * 50)
            for m in sorted(result["monthly_snapshots"].keys()):
                s = result["monthly_snapshots"][m]
                if s["median"] > 0:
                    print(f"{m:<8} ${s['p5']:<13,.0f} ${s['median']:<13,.0f} ${s['p95']:<13,.0f}")


def _run_with_win_rate(
    observed_trades, win_rate, starting_balance, bet_pct,
    liquidity_fill_pct, num_simulations, months, trades_per_day,
):
    entry_prices = [t.entry_price for t in observed_trades]
    liquidities = [t.liquidity for t in observed_trades]

    total_trades = int(trades_per_day * 30 * months)
    trades_per_month = int(trades_per_day * 30)

    all_final = []
    all_max_dd = []
    ruin_count = 0
    monthly_snaps = {m: [] for m in range(1, months + 1)}

    for sim in range(num_simulations):
        rng = random.Random(sim * 7919 + 42)
        balance = starting_balance
        peak = starting_balance
        max_dd = 0.0

        for trade_num in range(total_trades):
            entry = rng.choice(entry_prices)
            fee_rate = polymarket_fee(entry)
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


if __name__ == "__main__":
    main()
