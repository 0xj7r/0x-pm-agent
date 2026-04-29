#!/usr/bin/env python3
"""Daily performance summary for live polymarket-exec deployments.

Reads:
  - Bot journal.jsonl (per-event log)
  - On-chain wallet balances (USDC.e + pUSD via Polygon RPC)
  - Polymarket data API (positions + activity)

Writes one JSON line per day to data/perf/daily.jsonl, plus stdout summary.

Designed for cron / systemd timer:
  */60 * * * *  cd /path/to/repo && python3 scripts/daily_perf_summary.py

Usage:
  python3 scripts/daily_perf_summary.py [--journal PATH] [--wallet ADDR]
                                        [--out PATH] [--rpc URL]
"""
from __future__ import annotations

import argparse
import datetime
import json
import re
import subprocess
import sys
import urllib.request
from collections import Counter, defaultdict
from pathlib import Path

USDC_E_TOKEN = "0x2791Bca1f2de4661ED88A30C99A7a9449Aa84174"
PUSD_TOKEN = "0xC011a7E12a19f7B1f670d46F03B03f3342E82DFB"
DEFAULT_RPC = "https://polygon-bor-rpc.publicnode.com"
DEFAULT_WALLET = "0x97fBC6Bc0d3A70F17EC83aB9cF9d6838328b47c5"
DEFAULT_JOURNAL = Path("data/execution/live/btc-5m-mm-tinylive/journal.jsonl")
DEFAULT_OUT = Path("data/perf/daily.jsonl")


def erc20_balance(rpc: str, token: str, holder: str) -> float:
    """Read ERC-20 balance via eth_call. Returns balance in token units (6 decimals)."""
    addr_padded = "000000000000000000000000" + holder.lower().lstrip("0x")
    payload = {
        "jsonrpc": "2.0",
        "id": 1,
        "method": "eth_call",
        "params": [
            {"to": token, "data": "0x70a08231" + addr_padded},
            "latest",
        ],
    }
    req = urllib.request.Request(
        rpc,
        data=json.dumps(payload).encode(),
        headers={"content-type": "application/json"},
    )
    with urllib.request.urlopen(req, timeout=15) as resp:
        result = json.loads(resp.read())
    raw = result.get("result", "0x0")
    return int(raw, 16) / 1e6


def position_value(wallet: str) -> float:
    """Current redeemable position value from Polymarket data API."""
    url = f"https://data-api.polymarket.com/value?user={wallet}"
    try:
        with urllib.request.urlopen(url, timeout=15) as resp:
            data = json.loads(resp.read())
        return float(data[0]["value"]) if data else 0.0
    except Exception:
        return 0.0


def parse_journal_today(journal_path: Path, since_ms: int, until_ms: int) -> dict:
    """Aggregate fills from journal.jsonl in the [since, until) window."""
    if not journal_path.exists():
        return {"error": f"journal not found: {journal_path}"}

    fills_by_path = Counter()
    fills_by_path_qty = defaultdict(float)
    fills_by_path_usd = defaultdict(float)
    rejected_by_path = Counter()
    suppressions = Counter()
    signal_fires = Counter()

    coid_re = re.compile(r"client_order_id=(btc-5m-mm:[^\s]+)")

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
            if ts < since_ms or ts >= until_ms:
                continue
            msg = rec.get("message", "")
            coid = rec.get("client_order_id") or ""

            # path classification
            path = "?"
            if ":mm-paired-bid:" in coid:
                # extract level (l1 - l8)
                m = re.search(r":mm-paired-bid:(l\d+)", coid)
                path = f"paired-bid:{m.group(1)}" if m else "paired-bid"
            elif ":mm-hedge-rescue" in coid:
                path = "hedge-rescue"
            elif ":mm-convex-accum:" in coid:
                path = "convex-accum"
            elif ":mm-late-bar-core:" in coid:
                path = "late-bar-core"

            if "fill received" in msg.lower():
                fills_by_path[path] += 1
                # extract price + qty from COID
                parts = coid.split(":")
                if len(parts) >= 10:
                    try:
                        price = float(parts[8])
                        qty = float(parts[9])
                        fills_by_path_qty[path] += qty
                        fills_by_path_usd[path] += price * qty
                    except (ValueError, IndexError):
                        pass
            elif "PendingSubmit -> Rejected" in msg:
                rejected_by_path[path] += 1
            elif "fresh entry suppressed" in msg or "drift block engaged" in msg:
                # category of suppression
                if "drift block" in msg:
                    suppressions["drift_block"] += 1
                elif "duplicate" in msg:
                    suppressions["duplicate"] += 1
                else:
                    suppressions["other"] += 1
            elif "paired entry suppressed by order-flow imbalance" in msg:
                signal_fires["flow_imbalance"] += 1
            elif "paired entry suppressed by bar-phase pacing" in msg:
                signal_fires["bar_phase"] += 1

    return {
        "fills_by_path": dict(fills_by_path),
        "fills_by_path_qty": dict(fills_by_path_qty),
        "fills_by_path_usd": dict(fills_by_path_usd),
        "rejected_by_path": dict(rejected_by_path),
        "suppressions": dict(suppressions),
        "signal_fires": dict(signal_fires),
    }


def main() -> None:
    p = argparse.ArgumentParser()
    p.add_argument("--journal", default=str(DEFAULT_JOURNAL))
    p.add_argument("--wallet", default=DEFAULT_WALLET)
    p.add_argument("--rpc", default=DEFAULT_RPC)
    p.add_argument("--out", default=str(DEFAULT_OUT))
    p.add_argument("--date", help="UTC date YYYY-MM-DD (default: today)")
    args = p.parse_args()

    if args.date:
        day = datetime.datetime.strptime(args.date, "%Y-%m-%d").replace(
            tzinfo=datetime.timezone.utc
        )
    else:
        day = datetime.datetime.now(datetime.timezone.utc).replace(
            hour=0, minute=0, second=0, microsecond=0
        )
    next_day = day + datetime.timedelta(days=1)
    since_ms = int(day.timestamp() * 1000)
    until_ms = int(next_day.timestamp() * 1000)

    # On-chain balances (point-in-time)
    try:
        usdc_e = erc20_balance(args.rpc, USDC_E_TOKEN, args.wallet)
        pusd = erc20_balance(args.rpc, PUSD_TOKEN, args.wallet)
    except Exception as e:
        print(f"warning: rpc balance fetch failed: {e}", file=sys.stderr)
        usdc_e, pusd = 0.0, 0.0
    pos_value = position_value(args.wallet)
    total_equity = usdc_e + pusd + pos_value

    # Journal stats for the day
    j = parse_journal_today(Path(args.journal), since_ms, until_ms)

    summary = {
        "date_utc": day.strftime("%Y-%m-%d"),
        "snapshot_at_ms": int(datetime.datetime.now(datetime.timezone.utc).timestamp() * 1000),
        "wallet": args.wallet,
        "balances": {
            "usdc_e": round(usdc_e, 4),
            "pusd": round(pusd, 4),
            "position_mark": round(pos_value, 4),
            "total_equity": round(total_equity, 4),
        },
        "journal": j,
    }

    # Stdout summary
    print(f"=== daily perf snapshot {summary['date_utc']} (UTC) ===")
    print(f"wallet:           {args.wallet}")
    print(
        f"USDC.e: ${usdc_e:>9.2f}  pUSD: ${pusd:>9.2f}  positions: ${pos_value:>9.2f}"
        f"  TOTAL: ${total_equity:>9.2f}"
    )
    print()
    fills = j.get("fills_by_path", {})
    fills_usd = j.get("fills_by_path_usd", {})
    if fills:
        print("=== fills today ===")
        total_fills = sum(fills.values())
        total_usd = sum(fills_usd.values())
        for tag, n in sorted(fills.items(), key=lambda kv: -kv[1]):
            usd = fills_usd.get(tag, 0)
            print(f"  {tag:>22s}: {n:>3d} fills  ${usd:>7.2f}")
        print(f"  {'TOTAL':>22s}: {total_fills:>3d} fills  ${total_usd:>7.2f}")
        print()
    sup = j.get("suppressions", {})
    sig = j.get("signal_fires", {})
    if sup or sig:
        print("=== signals + suppressions ===")
        for k, v in sup.items():
            print(f"  suppressed: {k}: {v}")
        for k, v in sig.items():
            print(f"  fired: {k}: {v}")

    # Append to JSONL
    out_path = Path(args.out)
    out_path.parent.mkdir(parents=True, exist_ok=True)
    with out_path.open("a") as f:
        f.write(json.dumps(summary) + "\n")
    print()
    print(f"appended to: {out_path}")


if __name__ == "__main__":
    main()
