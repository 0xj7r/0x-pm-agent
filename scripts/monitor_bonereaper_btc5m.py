#!/usr/bin/env python3
"""Monitor Bonereaper BTC 5m activity across rolling markets.

Discovers BTC 5m slugs from wall-clock time, fetches wallet activity from
Polymarket's data API, and writes a compact per-market timing/notional/share
summary. Intended for live calibration of the observed Bonereaper pattern:
early paired accumulation, late favourite dollar loading, and very-late cheap
tail share accumulation.
"""

from __future__ import annotations

import argparse
import json
import math
import os
import time
import urllib.parse
import urllib.request
from collections import defaultdict
from datetime import datetime, timezone
from pathlib import Path
from typing import Any


DEFAULT_WALLET = "0xeebde7a0e019a63e6b476eb425505b7b3e6eba30"
DATA_API = "https://data-api.polymarket.com"
SLUG_PREFIX = "btc-updown-5m-"
WINDOW_SEC = 300


def utc_iso(ts: int | float) -> str:
    return datetime.fromtimestamp(ts, tz=timezone.utc).isoformat()


def current_window_start(now_s: int) -> int:
    return (now_s // WINDOW_SEC) * WINDOW_SEC


def rolling_slugs(now_s: int, prev: int, next_: int) -> list[str]:
    base = current_window_start(now_s)
    starts = [base + offset * WINDOW_SEC for offset in range(-prev, next_ + 1)]
    return [f"{SLUG_PREFIX}{start}" for start in starts]


def fetch_activity(wallet: str, pages: int, limit: int, timeout_s: float) -> list[dict[str, Any]]:
    rows: list[dict[str, Any]] = []
    for page in range(pages):
        query = urllib.parse.urlencode(
            {
                "user": wallet,
                "limit": limit,
                "offset": page * limit,
            }
        )
        req = urllib.request.Request(
            f"{DATA_API}/activity?{query}",
            headers={"User-Agent": "polymarket-agent-bonereaper-monitor/1.0"},
        )
        with urllib.request.urlopen(req, timeout=timeout_s) as resp:
            payload = json.load(resp)
        if not isinstance(payload, list):
            raise RuntimeError(f"unexpected activity payload: {payload!r}")
        if not payload:
            break
        rows.extend(payload)
        if len(payload) < limit:
            break
    return rows


def row_key(row: dict[str, Any]) -> tuple[Any, ...]:
    return (
        row.get("transactionHash"),
        row.get("timestamp"),
        row.get("slug"),
        row.get("outcome"),
        row.get("side"),
        row.get("price"),
        row.get("size"),
        row.get("usdcSize"),
    )


def dedupe(rows: list[dict[str, Any]]) -> list[dict[str, Any]]:
    seen: set[tuple[Any, ...]] = set()
    unique: list[dict[str, Any]] = []
    for row in rows:
        key = row_key(row)
        if key in seen:
            continue
        seen.add(key)
        unique.append(row)
    return unique


def safe_float(value: Any) -> float:
    try:
        out = float(value)
    except (TypeError, ValueError):
        return 0.0
    return out if math.isfinite(out) else 0.0


def parse_start_from_slug(slug: str) -> int | None:
    try:
        return int(slug.rsplit("-", 1)[1])
    except (IndexError, ValueError):
        return None


def summarize_market(slug: str, rows: list[dict[str, Any]], bucket_sec: int) -> dict[str, Any]:
    start_s = parse_start_from_slug(slug)
    rows = sorted(rows, key=lambda row: int(row.get("timestamp") or 0))
    trades = [row for row in rows if str(row.get("type") or "").upper() == "TRADE"]

    by_outcome: dict[str, dict[str, Any]] = {}
    by_bucket: dict[int, dict[str, dict[str, float]]] = defaultdict(
        lambda: defaultdict(lambda: {"trades": 0, "shares": 0.0, "notional": 0.0})
    )
    side_counts: dict[str, int] = defaultdict(int)

    for row in trades:
        outcome = str(row.get("outcome") or "unknown")
        side = str(row.get("side") or "unknown").upper()
        price = safe_float(row.get("price"))
        size = safe_float(row.get("size"))
        notional = safe_float(row.get("usdcSize")) or price * size
        side_counts[side] += 1

        item = by_outcome.setdefault(
            outcome,
            {
                "trades": 0,
                "buy_trades": 0,
                "sell_trades": 0,
                "shares": 0.0,
                "notional": 0.0,
                "min_price": None,
                "max_price": None,
            },
        )
        item["trades"] += 1
        item["buy_trades"] += int(side == "BUY")
        item["sell_trades"] += int(side == "SELL")
        item["shares"] += size
        item["notional"] += notional
        item["min_price"] = price if item["min_price"] is None else min(item["min_price"], price)
        item["max_price"] = price if item["max_price"] is None else max(item["max_price"], price)

        if start_s is not None and bucket_sec > 0:
            elapsed = max(0, int(row.get("timestamp") or start_s) - start_s)
            bucket_start = (elapsed // bucket_sec) * bucket_sec
            bucket = by_bucket[bucket_start][outcome]
            bucket["trades"] += 1
            bucket["shares"] += size
            bucket["notional"] += notional

    for item in by_outcome.values():
        item["vwap"] = item["notional"] / item["shares"] if item["shares"] > 0 else None
        for key in ["shares", "notional", "vwap", "min_price", "max_price"]:
            if item[key] is not None:
                item[key] = round(float(item[key]), 6)

    bucket_rows = []
    cumulative: dict[str, dict[str, float]] = defaultdict(lambda: {"shares": 0.0, "notional": 0.0})
    for bucket_start in sorted(by_bucket):
        outcomes = {}
        for outcome, value in sorted(by_bucket[bucket_start].items()):
            cumulative[outcome]["shares"] += value["shares"]
            cumulative[outcome]["notional"] += value["notional"]
            outcomes[outcome] = {
                "trades": int(value["trades"]),
                "shares": round(value["shares"], 6),
                "notional": round(value["notional"], 6),
                "cum_shares": round(cumulative[outcome]["shares"], 6),
                "cum_notional": round(cumulative[outcome]["notional"], 6),
            }
        bucket_rows.append({"start_sec": bucket_start, "end_sec": bucket_start + bucket_sec, "outcomes": outcomes})

    first_ts = int(trades[0]["timestamp"]) if trades else None
    last_ts = int(trades[-1]["timestamp"]) if trades else None
    return {
        "slug": slug,
        "window_start_utc": utc_iso(start_s) if start_s is not None else None,
        "window_end_utc": utc_iso(start_s + WINDOW_SEC) if start_s is not None else None,
        "trade_count": len(trades),
        "activity_count": len(rows),
        "first_trade_utc": utc_iso(first_ts) if first_ts else None,
        "last_trade_utc": utc_iso(last_ts) if last_ts else None,
        "side_counts": dict(sorted(side_counts.items())),
        "total_notional": round(sum(item["notional"] for item in by_outcome.values()), 6),
        "total_shares": round(sum(item["shares"] for item in by_outcome.values()), 6),
        "by_outcome": dict(sorted(by_outcome.items())),
        "buckets": bucket_rows,
    }


def write_outputs(out_dir: Path, snapshot: dict[str, Any]) -> None:
    out_dir.mkdir(parents=True, exist_ok=True)
    latest_path = out_dir / "latest.json"
    latest_path.write_text(json.dumps(snapshot, indent=2, sort_keys=True) + "\n")

    with (out_dir / "history.jsonl").open("a") as fh:
        fh.write(json.dumps(snapshot, sort_keys=True) + "\n")

    lines = [
        f"# Bonereaper BTC 5m monitor",
        "",
        f"- generated: `{snapshot['generated_at_utc']}`",
        f"- wallet: `{snapshot['wallet']}`",
        f"- slugs: `{', '.join(snapshot['slugs'])}`",
        "",
    ]
    for market in snapshot["markets"]:
        lines.append(f"## {market['slug']}")
        lines.append("")
        lines.append(
            f"- trades: `{market['trade_count']}`, notional: `${market['total_notional']:.2f}`, shares: `{market['total_shares']:.2f}`"
        )
        lines.append(f"- first/last: `{market['first_trade_utc']}` -> `{market['last_trade_utc']}`")
        for outcome, item in market["by_outcome"].items():
            lines.append(
                f"- {outcome}: `{item['trades']}` trades, `${item['notional']:.2f}`, `{item['shares']:.2f}` shares, VWAP `{item['vwap']}`"
            )
        lines.append("")
        if market["buckets"]:
            lines.append("| sec | outcome | bucket notional | bucket shares | cum notional | cum shares |")
            lines.append("|---:|---|---:|---:|---:|---:|")
            for bucket in market["buckets"]:
                for outcome, item in bucket["outcomes"].items():
                    lines.append(
                        f"| {bucket['start_sec']}-{bucket['end_sec']} | {outcome} | ${item['notional']:.2f} | {item['shares']:.2f} | ${item['cum_notional']:.2f} | {item['cum_shares']:.2f} |"
                    )
            lines.append("")
    (out_dir / "latest.md").write_text("\n".join(lines) + "\n")


def run_once(args: argparse.Namespace) -> dict[str, Any]:
    now_s = int(time.time())
    slugs = rolling_slugs(now_s, args.prev, args.next)
    raw_rows = fetch_activity(args.wallet, args.pages, args.limit, args.timeout_s)
    rows = dedupe(raw_rows)
    rows_by_slug: dict[str, list[dict[str, Any]]] = defaultdict(list)
    wanted = set(slugs)
    for row in rows:
        slug = str(row.get("slug") or "")
        if slug in wanted:
            rows_by_slug[slug].append(row)

    snapshot = {
        "generated_at_utc": utc_iso(now_s),
        "wallet": args.wallet.lower(),
        "slugs": slugs,
        "raw_rows_fetched": len(raw_rows),
        "rows_after_dedupe": len(rows),
        "markets": [summarize_market(slug, rows_by_slug.get(slug, []), args.bucket_sec) for slug in slugs],
    }
    write_outputs(args.out_dir, snapshot)
    return snapshot


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--wallet", default=os.getenv("BONEREAPER_WALLET", DEFAULT_WALLET))
    parser.add_argument("--prev", type=int, default=2, help="number of previous 5m BTC markets")
    parser.add_argument("--next", type=int, default=3, help="number of current/future 5m BTC markets")
    parser.add_argument("--pages", type=int, default=7, help="activity pages to fetch; max useful offset is 3000")
    parser.add_argument("--limit", type=int, default=500)
    parser.add_argument("--timeout-s", type=float, default=10.0)
    parser.add_argument("--bucket-sec", type=int, default=15)
    parser.add_argument("--interval-sec", type=int, default=60)
    parser.add_argument("--watch", action="store_true")
    parser.add_argument("--out-dir", type=Path, default=Path("data/research/bonereaper/live_btc5m_monitor"))
    args = parser.parse_args()

    while True:
        snapshot = run_once(args)
        active = [m for m in snapshot["markets"] if m["trade_count"] > 0]
        print(
            json.dumps(
                {
                    "generated_at_utc": snapshot["generated_at_utc"],
                    "markets_with_trades": len(active),
                    "out_dir": str(args.out_dir),
                    "slugs": snapshot["slugs"],
                },
                sort_keys=True,
            ),
            flush=True,
        )
        if not args.watch:
            break
        time.sleep(max(5, args.interval_sec))


if __name__ == "__main__":
    main()
