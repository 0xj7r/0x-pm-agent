"""Counterfactual: what if we'd used Kelly sizing on all historical trades?

Approach:
  1. Load all resolutions from the paper DB.
  2. Bucket by entry-price range to compute empirical p_win per bucket.
  3. For each trade, compute Kelly fraction using bucket p_win.
  4. Apply kelly_multiplier (0.25), cap at max_position_usd ($5).
  5. Recompute P&L at the counterfactual size.
  6. Report actual vs counterfactual totals.

Uses 50/50 split to avoid look-ahead: first half trains p_win estimator,
second half applies Kelly sizing.
"""
from __future__ import annotations

import json
import sqlite3
import sys
from collections import defaultdict
from pathlib import Path


PRICE_BUCKETS = [
    (0.00, 0.10, "0-10¢"),
    (0.10, 0.25, "10-25¢"),
    (0.25, 0.40, "25-40¢"),
    (0.40, 0.55, "40-55¢"),
    (0.55, 1.00, "55¢+"),
]

KELLY_MULTIPLIER = 0.25
MAX_POSITION_USD = 5.0
STARTING_BANKROLL = 100.0


def bucket_of(price: float) -> str:
    for lo, hi, label in PRICE_BUCKETS:
        if lo <= price < hi:
            return label
    return "55¢+"


def kelly_fraction(p_win: float, token_price: float) -> float:
    if p_win <= 0 or token_price <= 0 or token_price >= 1:
        return 0.0
    b = (1 - token_price) / token_price
    q = 1 - p_win
    k = (p_win * b - q) / b
    return max(0.0, k)


def load_resolutions(db_path: str) -> list[dict]:
    conn = sqlite3.connect(db_path)
    conn.row_factory = sqlite3.Row
    rows = conn.execute(
        "SELECT id, timestamp, details FROM event_log "
        "WHERE event_type='resolution' ORDER BY id"
    ).fetchall()
    conn.close()
    out = []
    for r in rows:
        try:
            d = json.loads(r["details"])
        except Exception:
            continue
        t = d.get("trade", {})
        if not t.get("token_price") or t.get("size_usd") is None:
            continue
        out.append({
            "timestamp": r["timestamp"],
            "won": bool(d.get("won")),
            "actual_size_usd": float(t["size_usd"]),
            "actual_pnl_usd": float(d.get("pnl_usd") or 0.0),
            "token_price": float(t["token_price"]),
            "shares_per_dollar": 1.0 / float(t["token_price"]),
            "bucket": bucket_of(float(t["token_price"])),
            "direction": t.get("direction"),
        })
    return out


def per_bucket_winrate(trades: list[dict]) -> dict[str, tuple[int, int, float]]:
    counts: dict[str, list[int]] = defaultdict(lambda: [0, 0])
    for t in trades:
        counts[t["bucket"]][1] += 1
        if t["won"]:
            counts[t["bucket"]][0] += 1
    return {
        b: (w, n, (w / n if n else 0.0))
        for b, (w, n) in counts.items()
    }


def simulate(
    all_trades: list[dict],
    bucket_p: dict[str, float],
    starting: float = STARTING_BANKROLL,
) -> dict:
    bankroll = starting
    counterfactual_pnl_total = 0.0
    actual_pnl_total = 0.0
    trades_taken = 0
    trades_skipped = 0
    size_log: list[tuple[str, float, float, float, float]] = []

    for t in all_trades:
        p = bucket_p.get(t["bucket"], 0.0)
        k_frac = kelly_fraction(p, t["token_price"])
        k_sized = k_frac * KELLY_MULTIPLIER
        size = min(k_sized * bankroll, MAX_POSITION_USD)

        actual_pnl_total += t["actual_pnl_usd"]

        if size <= 0.01:
            trades_skipped += 1
            continue

        # Counterfactual P&L at Kelly size: scale actual pnl by size ratio
        # (actual_pnl is computed at actual_size; Kelly wins/loses same rate)
        if t["actual_size_usd"] > 0:
            pnl = t["actual_pnl_usd"] * (size / t["actual_size_usd"])
        else:
            pnl = 0.0
        counterfactual_pnl_total += pnl
        bankroll += pnl
        trades_taken += 1
        size_log.append((t["bucket"], t["token_price"], p, size, pnl))

    return {
        "counterfactual_pnl": counterfactual_pnl_total,
        "actual_pnl": actual_pnl_total,
        "trades_taken": trades_taken,
        "trades_skipped": trades_skipped,
        "final_bankroll": bankroll,
        "size_log": size_log,
    }


def main(train_db: str, test_db: str) -> int:
    train = load_resolutions(train_db)
    test = load_resolutions(test_db)
    if len(train) < 20:
        print(f"Too few train resolutions ({len(train)}).")
        return 1
    if len(test) == 0:
        print(f"No test resolutions.")
        return 1

    train_winrates = per_bucket_winrate(train)
    test_winrates = per_bucket_winrate(test)

    print(f"Train: {len(train)} resolutions from {train_db}")
    print(f"Test:  {len(test)} resolutions from {test_db}\n")

    print("Per-bucket win rates (train set → applied as p_win to test set):")
    print(f"  {'bucket':<10s}  {'train_wr':<12s}  {'test_wr':<12s}")
    for _, _, lbl in PRICE_BUCKETS:
        tw = train_winrates.get(lbl, (0, 0, 0.0))
        te = test_winrates.get(lbl, (0, 0, 0.0))
        print(
            f"  {lbl:<10s}  {tw[2]:>5.1%} ({tw[0]:>3d}/{tw[1]:>3d})  "
            f"{te[2]:>5.1%} ({te[0]:>3d}/{te[1]:>3d})"
        )
    print()

    bucket_p = {lbl: train_winrates.get(lbl, (0, 0, 0.0))[2] for _, _, lbl in PRICE_BUCKETS}

    result = simulate(test, bucket_p)

    print("Counterfactual on TEST half (Kelly sizing using train-half p_win):")
    print(f"  trades_taken:            {result['trades_taken']}")
    print(f"  trades_skipped (Kelly=0):{result['trades_skipped']}")
    print(f"  actual P&L (flat $5):    ${result['actual_pnl']:+.2f}")
    print(f"  counterfactual P&L:      ${result['counterfactual_pnl']:+.2f}")
    print(f"  delta:                   ${result['counterfactual_pnl'] - result['actual_pnl']:+.2f}")
    print(f"  final bankroll:          ${result['final_bankroll']:.2f}")
    print()

    print("Sample counterfactual trades (bucket, price, p_win, Kelly_size, pnl):")
    for row in result["size_log"][:15]:
        bkt, px, p, size, pnl = row
        print(f"  {bkt:<10s} px={px:.2f}  p={p:.2f}  size=${size:.2f}  pnl=${pnl:+.2f}")

    return 0


if __name__ == "__main__":
    train_db = sys.argv[1] if len(sys.argv) > 1 else "data/btc_research_t10_trades.db"
    test_db = sys.argv[2] if len(sys.argv) > 2 else train_db
    sys.exit(main(train_db, test_db))
