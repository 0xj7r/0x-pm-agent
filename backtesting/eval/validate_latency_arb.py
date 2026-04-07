"""Validate latency arbitrage strategy against historical data.

For each resolved market, finds the first snapshot where BTC moved
more than MOVE_THRESHOLD from open, then checks the winning token
price at that moment. If it's below MAX_ENTRY_PRICE, the trade
would have been profitable (collect $1 per share at resolution).

Usage:
    python backtesting/eval/validate_latency_arb.py
    python backtesting/eval/validate_latency_arb.py --threshold 0.03 --max-entry 0.55
"""
from __future__ import annotations

import argparse
from dataclasses import dataclass
from pathlib import Path
import sys

sys.path.insert(0, str(Path(__file__).parent.parent.parent))

from shared.db import get_connection, market_start_col, snapshot_price_col
from shared.fees import taker_fee

DB_PATH = Path(__file__).parent.parent / "historical.db"


@dataclass
class TradeOpportunity:
    market_id: str
    winner: str
    btc_open: float
    btc_at_signal: float
    btc_move_pct: float
    winning_price: float
    snapshot_index: int
    total_snapshots: int
    cost_per_share: float
    payout_per_share: float
    profit_per_share: float
    profitable: bool


def validate(
    threshold_pct: float = 0.03,
    max_entry: float = 0.55,
    db_path: Path = DB_PATH,
) -> list[TradeOpportunity]:
    conn = get_connection(db_path)
    start_col = market_start_col(db_path)
    price_col = snapshot_price_col(db_path)

    markets = conn.execute(
        "SELECT * FROM markets WHERE winner IS NOT NULL ORDER BY start_time"
    ).fetchall()

    results: list[TradeOpportunity] = []

    for m in markets:
        market_id = m["market_id"]
        winner = m["winner"]
        btc_open = m[start_col]

        if not btc_open:
            continue

        snaps = conn.execute(
            "SELECT * FROM snapshots WHERE market_id = ? ORDER BY time",
            (market_id,),
        ).fetchall()

        if not snaps:
            continue

        for i, snap in enumerate(snaps):
            btc = snap[price_col]
            if not btc:
                continue

            move_pct = abs((btc - btc_open) / btc_open * 100)

            if move_pct >= threshold_pct:
                btc_direction = "Up" if btc > btc_open else "Down"
                winning_price = (
                    snap["price_up"] if winner == "Up" else snap["price_down"]
                )

                if winning_price is None or winning_price <= 0:
                    continue

                fee = winning_price * taker_fee(winning_price)
                cost = winning_price + fee
                payout = 1.0
                profit = payout - cost

                results.append(TradeOpportunity(
                    market_id=market_id,
                    winner=winner,
                    btc_open=btc_open,
                    btc_at_signal=btc,
                    btc_move_pct=move_pct,
                    winning_price=winning_price,
                    snapshot_index=i,
                    total_snapshots=len(snaps),
                    cost_per_share=round(cost, 4),
                    payout_per_share=payout,
                    profit_per_share=round(profit, 4),
                    profitable=profit > 0 and winning_price <= max_entry,
                ))
                break

    conn.close()
    return results


def validate_latency_arb_strategy(
    threshold_pct: float = 0.03,
    max_entry: float = 0.55,
    db_path: Path = DB_PATH,
) -> list[dict]:
    """Simulate the actual latency arb: buy whichever direction BTC is moving."""
    conn = get_connection(db_path)
    start_col = market_start_col(db_path)
    price_col = snapshot_price_col(db_path)

    markets = conn.execute(
        "SELECT * FROM markets WHERE winner IS NOT NULL ORDER BY start_time"
    ).fetchall()

    results: list[dict] = []

    for m in markets:
        market_id = m["market_id"]
        winner = m["winner"]
        btc_open = m[start_col]
        if not btc_open:
            continue

        snaps = conn.execute(
            "SELECT * FROM snapshots WHERE market_id = ? ORDER BY time",
            (market_id,),
        ).fetchall()

        if not snaps:
            continue

        for snap in snaps:
            btc = snap[price_col]
            if not btc:
                continue

            move_pct = (btc - btc_open) / btc_open * 100
            if abs(move_pct) < threshold_pct:
                continue

            btc_dir = "Up" if move_pct > 0 else "Down"
            entry_price = snap["price_up"] if btc_dir == "Up" else snap["price_down"]

            if entry_price is None or entry_price <= 0 or entry_price > max_entry:
                break

            won = btc_dir == winner
            fee = entry_price * taker_fee(entry_price)
            if won:
                pnl = 1.0 - entry_price - fee
            else:
                pnl = -(entry_price + fee)

            results.append({
                "market_id": market_id,
                "btc_dir": btc_dir,
                "winner": winner,
                "entry_price": entry_price,
                "pnl_per_share": round(pnl, 4),
                "won": won,
            })
            break

    conn.close()
    return results


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--threshold", type=float, default=0.03,
                        help="BTC move threshold in percent")
    parser.add_argument("--max-entry", type=float, default=0.55,
                        help="Max winning token price to enter")
    parser.add_argument("--db", type=str, default=str(DB_PATH))
    args = parser.parse_args()

    results = validate(args.threshold, args.max_entry, Path(args.db))

    if not results:
        print("No trade opportunities found.")
        return

    profitable = [r for r in results if r.profitable]
    unprofitable = [r for r in results if not r.profitable]

    print(f"\n{'='*80}")
    print(f"LATENCY ARB VALIDATION: {len(results)} markets analyzed")
    print(f"Threshold: {args.threshold}% BTC move | Max entry: {args.max_entry}")
    print("Taker fee: dynamic Polymarket fee")
    print(f"{'='*80}\n")

    print(f"Profitable: {len(profitable)}/{len(results)} "
          f"({len(profitable)/len(results)*100:.0f}%)\n")

    if profitable:
        avg_profit = sum(r.profit_per_share for r in profitable) / len(profitable)
        avg_price = sum(r.winning_price for r in profitable) / len(profitable)
        avg_idx = sum(r.snapshot_index for r in profitable) / len(profitable)
        print(f"  Avg winning token price at signal: {avg_price:.3f}")
        print(f"  Avg profit per share: ${avg_profit:.4f}")
        print(f"  Avg snapshot index at entry: {avg_idx:.0f}")
        print()

    print(f"{'Market':<12} {'Winner':<6} {'BTC Move%':<10} {'Win Price':<10} "
          f"{'Cost':<8} {'Profit':<8} {'Snap#':<8} {'OK?'}")
    print("-" * 80)

    for r in results:
        tag = "YES" if r.profitable else "NO"
        print(f"{r.market_id:<12} {r.winner:<6} {r.btc_move_pct:>8.4f}% "
              f"{r.winning_price:>8.3f}  {r.cost_per_share:>7.4f} "
              f"{r.profit_per_share:>+7.4f}  {r.snapshot_index:>5}/{r.total_snapshots:<5} "
              f"{tag}")

    # Summary stats by direction agreement
    print(f"\n{'='*80}")
    print("DIRECTION ANALYSIS")
    print(f"{'='*80}\n")

    agreed = [r for r in results
              if (r.btc_at_signal > r.btc_open and r.winner == "Up")
              or (r.btc_at_signal < r.btc_open and r.winner == "Down")]
    disagreed = [r for r in results if r not in agreed]

    print(f"BTC move AGREED with winner: {len(agreed)}/{len(results)}")
    if agreed:
        avg_p = sum(r.winning_price for r in agreed) / len(agreed)
        print(f"  Avg winning price when agreed: {avg_p:.3f}")
    print(f"BTC move DISAGREED with winner: {len(disagreed)}/{len(results)}")
    if disagreed:
        avg_p = sum(r.winning_price for r in disagreed) / len(disagreed)
        print(f"  Avg winning price when disagreed: {avg_p:.3f}")

    # Actual latency arb strategy: buy the direction BTC is moving
    print(f"\n{'='*80}")
    print("LATENCY ARB STRATEGY (buy direction of BTC move)")
    print(f"{'='*80}\n")

    latency_arb_results = validate_latency_arb_strategy(
        args.threshold, args.max_entry, Path(args.db)
    )
    if latency_arb_results:
        wins = [r for r in latency_arb_results if r["won"]]
        losses = [r for r in latency_arb_results if not r["won"]]
        total_pnl = sum(r["pnl_per_share"] for r in latency_arb_results)
        print(f"Trades: {len(latency_arb_results)} | "
              f"Wins: {len(wins)} | Losses: {len(losses)} | "
              f"Win rate: {len(wins)/len(latency_arb_results)*100:.0f}%")
        print(f"Total P&L per share: ${total_pnl:.4f}")
        if wins:
            print(f"Avg win profit: ${sum(r['pnl_per_share'] for r in wins)/len(wins):.4f}")
        if losses:
            print(f"Avg loss: ${sum(r['pnl_per_share'] for r in losses)/len(losses):.4f}")
        print()
        print(f"{'Market':<12} {'BTC Dir':<8} {'Winner':<7} {'Entry$':<8} "
              f"{'P&L/sh':<9} {'Won?'}")
        print("-" * 60)
        for r in latency_arb_results:
            print(f"{r['market_id']:<12} {r['btc_dir']:<8} {r['winner']:<7} "
                  f"{r['entry_price']:>6.3f}  {r['pnl_per_share']:>+8.4f}  "
                  f"{'YES' if r['won'] else 'NO'}")

    # Sweep thresholds
    print(f"\n{'='*80}")
    print("THRESHOLD SWEEP (oracle: buy winner)")
    print(f"{'='*80}\n")

    for t in [0.01, 0.02, 0.03, 0.05, 0.08, 0.10, 0.15]:
        sweep = validate(t, args.max_entry, Path(args.db))
        prof = [r for r in sweep if r.profitable]
        if sweep:
            avg = sum(r.winning_price for r in sweep) / len(sweep)
            print(f"  Threshold {t:>5.2f}%: {len(prof):>3}/{len(sweep):<3} profitable "
                  f"(avg win price: {avg:.3f})")

    print(f"\n{'='*80}")
    print("THRESHOLD SWEEP (latency arb: buy BTC direction)")
    print(f"{'='*80}\n")

    for t in [0.01, 0.02, 0.03, 0.05, 0.08, 0.10, 0.15]:
        sweep = validate_latency_arb_strategy(t, args.max_entry, Path(args.db))
        if sweep:
            wins = [r for r in sweep if r["won"]]
            avg_entry = sum(r["entry_price"] for r in sweep) / len(sweep)
            total_pnl = sum(r["pnl_per_share"] for r in sweep)
            print(f"  Threshold {t:>5.2f}%: {len(wins):>3}/{len(sweep):<3} wins "
                  f"(avg entry: {avg_entry:.3f}, net P&L/sh: {total_pnl/len(sweep):>+.4f})")


if __name__ == "__main__":
    main()
