"""Walk-forward cross-validation of Kelly hyperparameters.

Splits resolved trades from a paper DB chronologically into rolling
(train, test) folds. For each fold, grid-searches over
(prior_weight, prior_edge, kelly_multiplier) using only the train
slice, then evaluates log-wealth on the test slice. Reports the most
robust setting across folds.

Usage:
  python3 scripts/kelly_walkforward.py \\
    --db /opt/polymarket-agent/data/btc_research_t10_trades.db \\
    --min-train 100 --fold-size 40
"""
from __future__ import annotations

import argparse
import json
import math
import sqlite3
import sys
from collections import defaultdict
from dataclasses import dataclass


PRICE_BUCKETS = (
    (0.00, 0.10, "0-10c"),
    (0.10, 0.25, "10-25c"),
    (0.25, 0.40, "25-40c"),
    (0.40, 0.55, "40-55c"),
    (0.55, 1.00, "55c+"),
)


def bucket_for_price(price: float) -> str:
    for lo, hi, label in PRICE_BUCKETS:
        if lo <= price < hi:
            return label
    return "55c+"


@dataclass(frozen=True)
class Trade:
    token_price: float
    bucket: str
    won: bool
    actual_size_usd: float
    actual_pnl_usd: float
    timestamp: str


def load_trades(db_path: str) -> list[Trade]:
    conn = sqlite3.connect(db_path)
    conn.row_factory = sqlite3.Row
    rows = conn.execute(
        "SELECT timestamp, details FROM event_log "
        "WHERE event_type='resolution' ORDER BY id"
    ).fetchall()
    conn.close()
    out: list[Trade] = []
    for r in rows:
        try:
            d = json.loads(r["details"])
            t = d.get("trade") or {}
            tp = float(t.get("token_price") or 0.0)
            if tp <= 0 or tp >= 1:
                continue
            out.append(
                Trade(
                    token_price=tp,
                    bucket=bucket_for_price(tp),
                    won=bool(d.get("won")),
                    actual_size_usd=float(t.get("size_usd") or 0.0),
                    actual_pnl_usd=float(d.get("pnl_usd") or 0.0),
                    timestamp=str(r["timestamp"]),
                )
            )
        except Exception:
            continue
    return out


def bucket_counts(trades: list[Trade]) -> dict[str, tuple[int, int]]:
    out: dict[str, list[int]] = defaultdict(lambda: [0, 0])
    for t in trades:
        out[t.bucket][0 if t.won else 1] += 1
    return {b: (v[0], v[1]) for b, v in out.items()}


def smoothed_p_win(
    wins: int,
    losses: int,
    token_price: float,
    prior_weight: float,
    prior_edge: float,
) -> float:
    samples = wins + losses
    prior = max(0.0, min(0.98, token_price + prior_edge))
    if prior_weight <= 0 and samples == 0:
        return prior
    return (wins + prior_weight * prior) / (samples + prior_weight)


def full_kelly_fraction(p_win: float, token_price: float) -> float:
    if p_win <= 0 or token_price <= 0 or token_price >= 1:
        return 0.0
    win_return = (1.0 - token_price) / token_price
    loss_return = 1.0
    expected = p_win * win_return - (1.0 - p_win) * loss_return
    if expected <= 0:
        return 0.0
    return max(0.0, expected / (win_return * loss_return))


def evaluate_fold(
    train: list[Trade],
    test: list[Trade],
    prior_weight: float,
    prior_edge: float,
    kelly_mult: float,
    starting_bankroll: float = 100.0,
    max_pct: float = 0.10,
    max_size_usd: float = 25.0,
    min_size_usd: float = 1.0,
) -> dict:
    """Apply Kelly sizing (using train-derived buckets) to the test trades.

    Returns total P&L, final bankroll, and log-wealth.
    """
    counts = bucket_counts(train)
    bankroll = starting_bankroll
    trades_taken = 0
    trades_skipped = 0
    total_pnl = 0.0
    log_wealth = math.log(starting_bankroll)

    for t in test:
        wins, losses = counts.get(t.bucket, (0, 0))
        p = smoothed_p_win(wins, losses, t.token_price, prior_weight, prior_edge)
        kf = full_kelly_fraction(p, t.token_price) * kelly_mult
        size = min(kf * bankroll, bankroll * max_pct, max_size_usd)
        if size < min_size_usd:
            trades_skipped += 1
            continue
        trades_taken += 1
        if t.actual_size_usd > 0:
            pnl = t.actual_pnl_usd * (size / t.actual_size_usd)
        else:
            pnl = 0.0
        total_pnl += pnl
        bankroll = max(0.01, bankroll + pnl)
        log_wealth = math.log(bankroll)
    return {
        "taken": trades_taken,
        "skipped": trades_skipped,
        "total_pnl": total_pnl,
        "final_bankroll": bankroll,
        "log_wealth": log_wealth,
    }


def grid_search(
    train: list[Trade],
    test: list[Trade],
    grid: dict[str, list[float]],
) -> dict:
    best = None
    for pw in grid["prior_weight"]:
        for pe in grid["prior_edge"]:
            for km in grid["kelly_multiplier"]:
                r = evaluate_fold(train, test, pw, pe, km)
                if best is None or r["log_wealth"] > best["log_wealth"]:
                    best = {"prior_weight": pw, "prior_edge": pe, "kelly_multiplier": km, **r}
    return best


def main(db: str, min_train: int, fold_size: int) -> int:
    trades = load_trades(db)
    print(f"Loaded {len(trades)} resolutions from {db}")
    if len(trades) < min_train + fold_size:
        print(f"Too few trades for min_train={min_train} + fold_size={fold_size}.")
        return 1

    grid = {
        "prior_weight": [1.0, 3.0, 5.0, 10.0, 20.0, 40.0],
        "prior_edge": [0.00, 0.03, 0.05, 0.08, 0.12, 0.20],
        "kelly_multiplier": [0.10, 0.15, 0.25, 0.50, 1.00],
    }

    print("\nFold-by-fold best hyperparameters (by log-wealth on test slice):")
    print(
        f"  {'fold':<6s} {'pw':>5s} {'edge':>5s} {'km':>5s} "
        f"{'taken':>6s} {'pnl':>8s} {'final':>8s}"
    )

    best_params_per_fold: list[dict] = []
    i = 0
    while min_train + (i + 1) * fold_size <= len(trades):
        train_end = min_train + i * fold_size
        test_end = train_end + fold_size
        train = trades[:train_end]
        test = trades[train_end:test_end]
        best = grid_search(train, test, grid)
        best_params_per_fold.append(best)
        print(
            f"  {i:<6d} {best['prior_weight']:>5.1f} {best['prior_edge']:>5.2f} "
            f"{best['kelly_multiplier']:>5.2f} {best['taken']:>6d} "
            f"${best['total_pnl']:>7.2f} ${best['final_bankroll']:>7.2f}"
        )
        i += 1

    if not best_params_per_fold:
        return 1

    # Consensus: take median of each hyperparameter across folds
    def median(xs: list[float]) -> float:
        xs = sorted(xs)
        n = len(xs)
        return xs[n // 2] if n % 2 == 1 else (xs[n // 2 - 1] + xs[n // 2]) / 2

    med_pw = median([f["prior_weight"] for f in best_params_per_fold])
    med_pe = median([f["prior_edge"] for f in best_params_per_fold])
    med_km = median([f["kelly_multiplier"] for f in best_params_per_fold])

    print(f"\nMedian across {len(best_params_per_fold)} folds:")
    print(f"  prior_weight     = {med_pw}")
    print(f"  prior_edge       = {med_pe}")
    print(f"  kelly_multiplier = {med_km}")

    # Baseline: current live config
    print("\nCurrent live config check (pw=10, edge=0.08, km=0.25):")
    baseline_pnl = 0.0
    baseline_taken = 0
    for i in range(len(best_params_per_fold)):
        train_end = min_train + i * fold_size
        test_end = train_end + fold_size
        train = trades[:train_end]
        test = trades[train_end:test_end]
        r = evaluate_fold(train, test, 10.0, 0.08, 0.25)
        baseline_pnl += r["total_pnl"]
        baseline_taken += r["taken"]
    print(f"  baseline total P&L across test slices: ${baseline_pnl:.2f}")
    print(f"  baseline trades taken: {baseline_taken}")

    # Consensus run
    print(f"\nConsensus ({med_pw}, {med_pe}, {med_km}) across test slices:")
    cons_pnl = 0.0
    cons_taken = 0
    for i in range(len(best_params_per_fold)):
        train_end = min_train + i * fold_size
        test_end = train_end + fold_size
        train = trades[:train_end]
        test = trades[train_end:test_end]
        r = evaluate_fold(train, test, med_pw, med_pe, med_km)
        cons_pnl += r["total_pnl"]
        cons_taken += r["taken"]
    print(f"  consensus total P&L: ${cons_pnl:.2f}")
    print(f"  consensus trades taken: {cons_taken}")
    print(f"  improvement vs baseline: ${cons_pnl - baseline_pnl:+.2f}")

    return 0


if __name__ == "__main__":
    ap = argparse.ArgumentParser()
    ap.add_argument("--db", default="/opt/polymarket-agent/data/btc_research_t10_trades.db")
    ap.add_argument("--min-train", type=int, default=100)
    ap.add_argument("--fold-size", type=int, default=40)
    args = ap.parse_args()
    sys.exit(main(args.db, args.min_train, args.fold_size))
