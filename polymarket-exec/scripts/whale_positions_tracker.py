#!/usr/bin/env python3
"""Poll whale wallets for live positions and append to JSONL.

Why this exists: the whale strategies (unlawful_shear, Bonereaper,
xuanxuan008, penny-tail) carry directional information in WHICH side
they're holding and HOW LARGE the position is. Knowing this in real
time lets a future signal-aware strategy lean with whales we trust.

This is intentionally a Python polling script (not a Rust runtime
module) so we can iterate cheaply without touching the live trading
binary. Output is JSONL — one line per (whale, sample) — easy to
consume from any analysis notebook.

Usage:
  # Single snapshot
  ./whale_positions_tracker.py --once --out data/whale_positions/snapshot.jsonl

  # Continuous polling every 30s
  ./whale_positions_tracker.py --interval 30 --out data/whale_positions/2026-04-26.jsonl

  # Filter to specific markets (condition IDs)
  ./whale_positions_tracker.py --markets 0xabc...,0xdef... --out ...

Wallets tracked (default set; override with --wallets):
  unlawful-shear  0xb27bc932bf8110d8f78e55da7d5f0497a18b5b82
  bonereaper      0xeebde7a0e019a63e6b476eb425505b7b3e6eba30
  xuanxuan008     0xcfb103c37c0234f524c632d964ed31f117b5f694
  split-sell      0xe51b3d64da5b0b8a07a55f8bb3c3170237f73cad
  penny-tail      0x7da07b2a8b009a406198677debda46ad651b6be2
  multi-asset-A   0x76d4d4703add6e94cfdb1107f3d991d85ff2c512
"""

from __future__ import annotations

import argparse
import json
import sys
import time
import urllib.parse
import urllib.request
from datetime import datetime, timezone
from pathlib import Path

DATA_API = "https://data-api.polymarket.com"
DEFAULT_USER_AGENT = "polymarket-exec-whale-tracker/1.0"

DEFAULT_WALLETS = {
    "unlawful-shear": "0xb27bc932bf8110d8f78e55da7d5f0497a18b5b82",
    "bonereaper": "0xeebde7a0e019a63e6b476eb425505b7b3e6eba30",
    "xuanxuan008": "0xcfb103c37c0234f524c632d964ed31f117b5f694",
    "split-sell": "0xe51b3d64da5b0b8a07a55f8bb3c3170237f73cad",
    "penny-tail": "0x7da07b2a8b009a406198677debda46ad651b6be2",
    "multi-asset-a": "0x76d4d4703add6e94cfdb1107f3d991d85ff2c512",
}


def fetch_positions(wallet: str, size_threshold: float = 0.0) -> list[dict]:
    """Pull all current positions for a wallet via /positions."""
    params = {"user": wallet, "sizeThreshold": str(size_threshold), "limit": 500}
    url = f"{DATA_API}/positions?{urllib.parse.urlencode(params)}"
    req = urllib.request.Request(url, headers={"User-Agent": DEFAULT_USER_AGENT})
    with urllib.request.urlopen(req, timeout=15) as resp:
        return json.loads(resp.read())


def normalize_position(pos: dict, label: str, wallet: str, observed_at_iso: str) -> dict:
    """Strip to the fields a strategy actually needs. Keep raw available
    via 'raw' field for ad-hoc analysis."""
    return {
        "observed_at": observed_at_iso,
        "whale_label": label,
        "whale_wallet": wallet,
        "asset": pos.get("asset"),
        "condition_id": pos.get("conditionId"),
        "outcome": pos.get("outcome"),
        "outcome_index": pos.get("outcomeIndex"),
        "size": pos.get("size"),
        "avg_price": pos.get("avgPrice"),
        "current_price": pos.get("curPrice"),
        "current_value_usd": pos.get("currentValue"),
        "cash_pnl_usd": pos.get("cashPnl"),
        "realized_pnl_usd": pos.get("realizedPnl"),
        "redeemable": pos.get("redeemable"),
        "mergeable": pos.get("mergeable"),
        "title": pos.get("title"),
        "slug": pos.get("slug"),
    }


def poll_once(wallets: dict[str, str], markets_filter: set[str] | None) -> list[dict]:
    """Return one normalized record per (whale, position)."""
    observed_at = datetime.now(timezone.utc).isoformat()
    records: list[dict] = []
    for label, wallet in wallets.items():
        try:
            positions = fetch_positions(wallet)
        except Exception as e:
            print(f"[{label}] fetch failed: {e}", file=sys.stderr)
            continue
        for pos in positions:
            if markets_filter and pos.get("conditionId") not in markets_filter:
                continue
            records.append(normalize_position(pos, label, wallet, observed_at))
    return records


def write_records(records: list[dict], out_path: Path) -> None:
    out_path.parent.mkdir(parents=True, exist_ok=True)
    with out_path.open("a") as f:
        for rec in records:
            f.write(json.dumps(rec) + "\n")


def summarize(records: list[dict]) -> str:
    """One-line summary per whale."""
    by_whale: dict[str, list[dict]] = {}
    for r in records:
        by_whale.setdefault(r["whale_label"], []).append(r)
    lines = []
    for label in sorted(by_whale):
        recs = by_whale[label]
        n = len(recs)
        total_val = sum(float(r.get("current_value_usd") or 0) for r in recs)
        total_pnl = sum(float(r.get("cash_pnl_usd") or 0) for r in recs)
        lines.append(
            f"  {label:<16} positions={n:>3}  total_value=${total_val:>10.2f}  cash_pnl=${total_pnl:>+9.2f}"
        )
    return "\n".join(lines)


def main():
    ap = argparse.ArgumentParser(
        description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter
    )
    ap.add_argument("--out", required=True, help="JSONL output path (append mode)")
    ap.add_argument(
        "--once", action="store_true", help="Poll once and exit (default: continuous)"
    )
    ap.add_argument(
        "--interval", type=float, default=30.0, help="Polling interval in seconds (default 30)"
    )
    ap.add_argument(
        "--markets",
        help="Comma-separated condition IDs to filter to (default: all positions)",
    )
    ap.add_argument(
        "--wallets",
        help="Comma-separated label=address overrides (default: 6 known whales)",
    )
    args = ap.parse_args()

    wallets = dict(DEFAULT_WALLETS)
    if args.wallets:
        for entry in args.wallets.split(","):
            label, _, addr = entry.strip().partition("=")
            if label and addr:
                wallets[label] = addr
    markets_filter = (
        {m.strip() for m in args.markets.split(",")} if args.markets else None
    )
    out_path = Path(args.out)

    print(f"=== whale positions tracker ===")
    print(f"wallets: {len(wallets)}, output: {out_path}")
    if markets_filter:
        print(f"market filter: {len(markets_filter)} condition IDs")
    print()

    iteration = 0
    while True:
        iteration += 1
        records = poll_once(wallets, markets_filter)
        write_records(records, out_path)
        print(f"--- iter {iteration} @ {datetime.now(timezone.utc):%H:%M:%S} ---")
        print(summarize(records))
        print(f"  wrote {len(records)} records (total file size: {out_path.stat().st_size if out_path.exists() else 0} bytes)")
        if args.once:
            break
        try:
            time.sleep(args.interval)
        except KeyboardInterrupt:
            print("interrupted; exiting")
            break


if __name__ == "__main__":
    main()
