"""Simple sizing-rule counterfactuals on live trades.

Apply different sizing rules and see how P&L would differ from the
current flat $5 allocation.
"""
from __future__ import annotations

import json
import sqlite3
import sys


def load_live_resolutions(db_path: str) -> list[dict]:
    conn = sqlite3.connect(db_path)
    conn.row_factory = sqlite3.Row
    rows = conn.execute(
        "SELECT details FROM event_log WHERE event_type='resolution' ORDER BY id"
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
            "won": bool(d.get("won")),
            "actual_size_usd": float(t["size_usd"]),
            "actual_pnl_usd": float(d.get("pnl_usd") or 0.0),
            "token_price": float(t["token_price"]),
            "direction": t.get("direction"),
        })
    return out


def simulate_rule(trades: list[dict], size_fn) -> dict:
    taken = 0
    skipped = 0
    total_pnl = 0.0
    total_deployed = 0.0
    wins = 0
    losses = 0
    for t in trades:
        size = size_fn(t["token_price"])
        if size is None or size <= 0:
            skipped += 1
            continue
        taken += 1
        total_deployed += size
        if t["actual_size_usd"] > 0:
            pnl = t["actual_pnl_usd"] * (size / t["actual_size_usd"])
        else:
            pnl = 0.0
        total_pnl += pnl
        if t["won"]:
            wins += 1
        else:
            losses += 1
    return {
        "taken": taken,
        "skipped": skipped,
        "wins": wins,
        "losses": losses,
        "deployed": total_deployed,
        "pnl": total_pnl,
    }


def main(db_path: str) -> int:
    trades = load_live_resolutions(db_path)
    print(f"Loaded {len(trades)} live resolutions\n")

    rules = [
        ("A: flat $5 (actual)",          lambda px: 5.0),
        ("B: skip px > 0.50",            lambda px: 5.0 if px <= 0.50 else None),
        ("C: skip px > 0.45",            lambda px: 5.0 if px <= 0.45 else None),
        ("D: skip px > 0.40",            lambda px: 5.0 if px <= 0.40 else None),
        ("E: skip px < 0.10 (no tails)", lambda px: 5.0 if px >= 0.10 else None),
        ("F: skip px > 0.50 AND < 0.10", lambda px: 5.0 if 0.10 <= px <= 0.50 else None),
        ("G: 2× if px ≤ 0.30, else $5",  lambda px: 10.0 if px <= 0.30 else 5.0),
        ("H: scale inv to px (cap $10)", lambda px: min(10.0, 2.5 / max(px, 0.05))),
        ("I: only px in [0.25, 0.50]",   lambda px: 5.0 if 0.25 <= px <= 0.50 else None),
    ]

    print(f"  {'rule':<36s}  {'taken':>5s}  {'W-L':>5s}  {'deployed':>9s}  {'pnl':>8s}")
    for name, fn in rules:
        r = simulate_rule(trades, fn)
        wl = f"{r['wins']}-{r['losses']}"
        print(
            f"  {name:<36s}  {r['taken']:>5d}  {wl:>5s}  "
            f"${r['deployed']:>7.2f}  ${r['pnl']:+7.2f}"
        )
    return 0


if __name__ == "__main__":
    db = sys.argv[1] if len(sys.argv) > 1 else "data/btc_live_t10_trades.db"
    sys.exit(main(db))
