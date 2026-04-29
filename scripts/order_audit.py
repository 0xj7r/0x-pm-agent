#!/usr/bin/env python3
"""Per-order submit / fill / reject audit log from journal.jsonl.

Reads the runtime journal (default tinylive path) and emits one row per
client_order_id with its full lifecycle: submitted -> {Filled, Rejected,
Cancelled} along with prices, quantities, ladder level, and latency.

Usage:
  python3 scripts/order_audit.py [JOURNAL_PATH] [--out PATH] [--since SECONDS]

Outputs:
  - stdout: human-readable summary table (counts by terminal_state, by ladder level)
  - --out  : per-order JSONL (default: data/execution/live/btc-5m-mm-tinylive/orders.jsonl)
"""
from __future__ import annotations

import argparse
import json
import re
import sys
from collections import Counter, defaultdict
from pathlib import Path

DEFAULT_JOURNAL = Path("data/execution/live/btc-5m-mm-tinylive/journal.jsonl")

COID_RE = re.compile(
    r"client_order_id=(?P<coid>btc-5m-mm:[^\s]+) "
    r"from=(?P<from>\w+) to=(?P<to>\w+) reason=\"(?P<reason>[^\"]*)\""
)


def parse_coid(coid: str) -> dict:
    """Pull market, instrument, side, level, attempt_ms, price, qty out of a COID."""
    parts = coid.split(":")
    out = {"raw": coid}
    if len(parts) >= 9:
        out["market_id"] = parts[1]
        out["instrument_id"] = parts[2]
        out["side"] = "Buy" if parts[3] == "b" else "Sell"
        out["reduce_only"] = parts[4] == "y"
        out["level_tag"] = parts[5] + ":" + parts[6]
        try:
            out["attempt_ms"] = int(parts[7].split("-", 1)[1])
        except (ValueError, IndexError):
            out["attempt_ms"] = None
        try:
            out["price"] = float(parts[8])
            out["qty"] = float(parts[9])
        except (ValueError, IndexError):
            pass
    return out


def main():
    p = argparse.ArgumentParser()
    p.add_argument("journal", nargs="?", default=str(DEFAULT_JOURNAL))
    p.add_argument("--out")
    p.add_argument("--since", type=int, default=0, help="Only include events from last N seconds")
    args = p.parse_args()

    journal_path = Path(args.journal)
    if not journal_path.exists():
        sys.exit(f"journal not found: {journal_path}")

    out_path = Path(args.out) if args.out else journal_path.with_name("orders.jsonl")

    cutoff_ms = 0
    if args.since > 0:
        import time
        cutoff_ms = int(time.time() * 1000) - args.since * 1000

    orders: dict[str, dict] = {}

    transitions = 0
    fills_seen = 0

    with journal_path.open() as f:
        for line in f:
            try:
                obj = json.loads(line)
            except json.JSONDecodeError:
                continue
            if obj.get("kind") != "runtime_event":
                continue
            rec = obj.get("record", {})
            ts = rec.get("observed_at_ms", 0)
            if ts < cutoff_ms:
                continue
            msg = rec.get("message", "")
            cat = rec.get("category", "")
            coid = rec.get("client_order_id")

            # Order state transitions live in tracing logs, not in event_log.
            # But event_log captures fill events in Strategy/Inventory/Execution.
            if coid and msg:
                ent = orders.setdefault(coid, parse_coid(coid))
                lower = msg.lower()
                if "fill" in lower and cat in ("Inventory", "Strategy", "Execution"):
                    fills_seen += 1
                    ent["filled_at_ms"] = ts
                    ent.setdefault("terminal_state", "Filled")
                    # try to extract qty/price from message
                    m = re.search(r"qty=([\d.]+)@([\d.]+)", msg)
                    if m:
                        ent["fill_qty"] = float(m.group(1))
                        ent["fill_price"] = float(m.group(2))
                elif "cancel" in lower and "submitted" in lower:
                    ent["cancelled_at_ms"] = ts
                    if ent.get("terminal_state") not in ("Filled",):
                        ent["terminal_state"] = "Cancelled"
                elif "reject" in lower:
                    ent["rejected_at_ms"] = ts
                    if ent.get("terminal_state") not in ("Filled", "Cancelled"):
                        ent["terminal_state"] = "Rejected"
                        # capture venue reason if present
                        m = re.search(r'"error":"([^"]+)"', msg)
                        if m:
                            ent["reject_reason"] = m.group(1)
                elif "reserved inventory for submit" in lower:
                    ent.setdefault("submitted_at_ms", ts)
                transitions += 1

    # If terminal state never set, default to "Open"
    for coid, ent in orders.items():
        if "terminal_state" not in ent:
            ent["terminal_state"] = "Open"
        if ent.get("submitted_at_ms") and ent.get("filled_at_ms"):
            ent["fill_latency_ms"] = ent["filled_at_ms"] - ent["submitted_at_ms"]

    # Write per-order JSONL
    out_path.parent.mkdir(parents=True, exist_ok=True)
    with out_path.open("w") as f:
        for coid, ent in sorted(orders.items(), key=lambda kv: kv[1].get("submitted_at_ms") or 0):
            ent["client_order_id"] = coid
            f.write(json.dumps(ent) + "\n")

    # Summary
    states = Counter(ent["terminal_state"] for ent in orders.values())
    by_level = defaultdict(Counter)
    for ent in orders.values():
        lvl = ent.get("level_tag", "?")
        by_level[lvl][ent["terminal_state"]] += 1

    print(f"journal:   {journal_path}")
    print(f"orders:    {len(orders):,} (rows scanned: {transitions:,}, fills seen: {fills_seen})")
    print(f"output:    {out_path}")
    print()
    print("Terminal state distribution:")
    for state, c in sorted(states.items(), key=lambda kv: -kv[1]):
        print(f"  {state:>10s}: {c:>6,}")
    print()
    print("By ladder level (terminal state):")
    for lvl in sorted(by_level.keys()):
        states = by_level[lvl]
        total = sum(states.values())
        breakdown = ", ".join(f"{s}={c}" for s, c in sorted(states.items()))
        print(f"  {lvl:>20s}: {total:>5d}  ({breakdown})")

    fill_latencies = [
        ent["fill_latency_ms"] for ent in orders.values() if "fill_latency_ms" in ent
    ]
    if fill_latencies:
        fill_latencies.sort()
        n = len(fill_latencies)
        p50 = fill_latencies[n // 2]
        p90 = fill_latencies[int(n * 0.9)]
        print()
        print(f"Fill latency (ms): n={n}, median={p50}, p90={p90}, max={fill_latencies[-1]}")


if __name__ == "__main__":
    main()
