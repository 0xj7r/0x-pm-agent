"""Run backtest + Monte Carlo for each coin using stored strategy results.

Reads strategy_results.json, runs Kelly projection and stochastic
simulation per coin, generates combined report.

Usage:
    python backtesting/run_backtest_all.py
    python backtesting/run_backtest_all.py --start 100 --months 6 --sims 3000
"""
from __future__ import annotations

import json
import logging
import random
import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).parent.parent))

from backtesting.autoresearch import load_and_precompute
from backtesting.stochastic_projection import polymarket_fee, slippage

logger = logging.getLogger(__name__)
BASE_DIR = Path(__file__).parent


def extract_trades(coin: str, params: dict, strategy_name: str) -> list[dict]:
    """Extract trades using strategy params from stored results."""
    db_path = BASE_DIR / f"{coin}.db"
    if not db_path.exists():
        return []

    markets = load_and_precompute(db_path, coin=coin)
    move_thresh = params.get("move", 0.08)
    max_entry = params.get("max_entry", 0.55)
    vol_thresh = params.get("vol", 0)
    accel_thresh = params.get("accel", 0)
    skew_thresh = params.get("skew", 999)
    vel_thresh = params.get("vel", 0)

    import sqlite3
    conn = sqlite3.connect(str(db_path))
    liq_map = {}
    for r in conn.execute("SELECT market_id, final_liquidity FROM markets").fetchall():
        liq_map[r[0]] = r[1] or 15000
    conn.close()

    trades = []
    for pm in markets:
        for i in range(10, pm.num_snaps):
            if pm.abs_move[i] < move_thresh:
                continue

            # Apply strategy-specific filter
            skip = False
            if strategy_name == "volatility" and pm.volatility[i] < vol_thresh:
                skip = True
            elif strategy_name == "acceleration":
                d = "Up" if pm.move_pct[i] > 0 else "Down"
                if d == "Up" and pm.acceleration[i] < accel_thresh:
                    skip = True
                if d == "Down" and pm.acceleration[i] > -accel_thresh:
                    skip = True
            elif strategy_name == "skew" and pm.token_skew[i] > skew_thresh:
                skip = True
            elif strategy_name == "velocity" and abs(pm.velocity[i]) < vel_thresh:
                skip = True

            if skip:
                break

            d = "Up" if pm.move_pct[i] > 0 else "Down"
            entry = pm.price_up[i] if d == "Up" else pm.price_down[i]

            if entry <= 0 or entry > max_entry:
                break

            trades.append({
                "market_id": pm.market_id,
                "coin": coin,
                "direction": d,
                "winner": pm.winner,
                "won": d == pm.winner,
                "entry_price": entry,
                "liquidity": liq_map.get(pm.market_id, 15000),
            })
            break

    return trades


def run_monte_carlo(
    trades: list[dict],
    starting_balance: float,
    bet_pct: float,
    months: int,
    trades_per_day: float,
    num_sims: int,
    liquidity_fill: float = 0.20,
) -> dict:
    if not trades:
        return {}

    win_rate = sum(1 for t in trades if t["won"]) / len(trades)
    entries = [t["entry_price"] for t in trades]
    liqs = [t["liquidity"] for t in trades]

    total_trades = int(trades_per_day * 30 * months)
    trades_per_month = int(trades_per_day * 30)

    all_final = []
    all_dd = []
    monthly = {m: [] for m in range(1, months + 1)}

    for sim in range(num_sims):
        rng = random.Random(sim * 7919 + 42)
        balance = starting_balance
        peak = starting_balance
        max_dd = 0.0

        for t in range(total_trades):
            entry = rng.choice(entries)
            liq = rng.choice(liqs)
            fee = polymarket_fee(entry)

            bet = min(balance * bet_pct, liq * liquidity_fill)
            if bet < 1.0:
                break

            slip = slippage(bet, liq)
            eff_entry = entry * (1 + slip)
            if eff_entry >= 0.99:
                continue

            shares = bet / eff_entry
            fee_usd = bet * fee

            if rng.random() < win_rate:
                balance += shares * 1.0 - bet - fee_usd
            else:
                balance -= bet + fee_usd

            if balance <= 0:
                balance = 0
                break

            peak = max(peak, balance)
            dd = (peak - balance) / peak
            max_dd = max(max_dd, dd)

            mo = t // trades_per_month + 1
            if mo <= months and (t + 1) % trades_per_month == 0:
                monthly[mo].append(balance)

        all_final.append(balance)
        all_dd.append(max_dd)

    all_final.sort()
    n = len(all_final)
    return {
        "win_rate": win_rate,
        "num_trades_observed": len(trades),
        "median_final": all_final[n // 2],
        "p5_final": all_final[int(n * 0.05)],
        "p95_final": all_final[int(n * 0.95)],
        "median_dd": sorted(all_dd)[n // 2],
        "p95_dd": sorted(all_dd)[int(n * 0.95)],
        "monthly": {
            m: {"median": sorted(v)[len(v) // 2] if v else 0}
            for m, v in monthly.items()
        },
    }


def main():
    logging.basicConfig(level=logging.INFO, format="%(asctime)s %(levelname)s %(message)s")

    import argparse
    parser = argparse.ArgumentParser()
    parser.add_argument("--start", type=float, default=100.0)
    parser.add_argument("--months", type=int, default=6)
    parser.add_argument("--sims", type=int, default=3000)
    parser.add_argument("--bet-pct", type=float, default=0.10)
    args = parser.parse_args()

    results_path = BASE_DIR / "strategy_results.json"
    if not results_path.exists():
        print("Run run_all_research.py first")
        return

    strategies = json.loads(results_path.read_text())

    print(f"\n{'='*80}")
    print(f"MULTI-COIN BACKTEST + MONTE CARLO")
    print(f"Start: ${args.start} | {args.months} months | {args.sims} sims | {args.bet_pct*100:.0f}% bet")
    print(f"{'='*80}")

    combined_trades = []

    for coin, data in strategies.items():
        best = data["best_strategy"]
        logger.info(f"Extracting {coin.upper()} trades...")
        trades = extract_trades(coin, best["params"], best["name"])
        combined_trades.extend(trades)

        if not trades:
            print(f"\n{coin.upper()}: No trades found")
            continue

        wins = sum(1 for t in trades if t["won"])
        wr = wins / len(trades)

        # Estimate trades per day from data
        import sqlite3
        conn = sqlite3.connect(str(BASE_DIR / f"{coin}.db"))
        row = conn.execute("""
            SELECT MIN(start_time), MAX(end_time) FROM markets
            WHERE market_id IN (SELECT DISTINCT market_id FROM snapshots)
        """).fetchone()
        conn.close()

        from datetime import datetime
        t0 = datetime.fromisoformat(row[0].replace("Z", ""))
        t1 = datetime.fromisoformat(row[1].replace("Z", ""))
        days = max((t1 - t0).total_seconds() / 86400, 0.1)
        tpd = len(trades) / days

        logger.info(f"[{coin.upper()}] {len(trades)} trades over {days:.1f} days = {tpd:.1f}/day")
        logger.info(f"[{coin.upper()}] Running Monte Carlo...")

        mc = run_monte_carlo(trades, args.start, args.bet_pct, args.months, tpd, args.sims)

        print(f"\n--- {coin.upper()}: {best['name']} {best['params']} ---")
        print(f"Observed: {len(trades)} trades, {wins}/{len(trades)} wins ({wr:.0%}), {tpd:.1f}/day")
        print(f"Monte Carlo ({args.sims} sims, {args.months}mo):")
        print(f"  Median final:  ${mc['median_final']:>12,.0f}")
        print(f"  P5-P95:        ${mc['p5_final']:>12,.0f} - ${mc['p95_final']:>12,.0f}")
        print(f"  Median max DD: {mc['median_dd']*100:>11.1f}%")
        print(f"  P95 max DD:    {mc['p95_dd']*100:>11.1f}%")

        if mc.get("monthly"):
            print(f"  Monthly median: ", end="")
            for m in sorted(mc["monthly"].keys()):
                med = mc["monthly"][m]["median"]
                if med > 0:
                    print(f"M{m}=${med:,.0f} ", end="")
            print()

    # Combined across all coins
    if combined_trades:
        total_wins = sum(1 for t in combined_trades if t["won"])
        total_wr = total_wins / len(combined_trades)
        print(f"\n{'='*80}")
        print(f"COMBINED: {len(combined_trades)} trades across all coins, "
              f"{total_wins}/{len(combined_trades)} wins ({total_wr:.0%})")

        # Rough combined trades per day
        combined_tpd = sum(
            len([t for t in combined_trades if t["coin"] == c])
            for c in strategies.keys()
        ) / max(days, 1)

        mc_combined = run_monte_carlo(
            combined_trades, args.start, args.bet_pct,
            args.months, combined_tpd, args.sims,
        )

        print(f"Combined Monte Carlo:")
        print(f"  Trades/day:    {combined_tpd:>11.1f}")
        print(f"  Median final:  ${mc_combined['median_final']:>12,.0f}")
        print(f"  P5-P95:        ${mc_combined['p5_final']:>12,.0f} - ${mc_combined['p95_final']:>12,.0f}")
        print(f"  Median max DD: {mc_combined['median_dd']*100:>11.1f}%")


if __name__ == "__main__":
    main()
