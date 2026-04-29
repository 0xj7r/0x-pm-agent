#!/usr/bin/env python3
"""Compare benchmark wallet activity against a local/live execution journal.

This is an operator analysis tool, not a trading path. It answers questions like:

- Is the benchmark wallet actually two-sided in the same windows?
- How much merge/redeem recycling is visible?
- Are our fills coming from paired bids, convex bids, or hedge rescue?
- Are our logs dominated by degraded/rejected/non-scoring states?
"""

from __future__ import annotations

import argparse
import json
import urllib.parse
import urllib.request
from collections import Counter, defaultdict
from datetime import datetime, timezone
from pathlib import Path
from typing import Any


DATA_API_ACTIVITY = "https://data-api.polymarket.com/activity"
DEFAULT_WHALE_WALLET = "0xb27bc932bf8110d8f78e55da7d5f0497a18b5b82"
WINDOW_SECONDS = 5 * 60


def parse_utc(value: str) -> int:
    if value.isdigit():
        return int(value)
    normalized = value.replace("Z", "+00:00")
    return int(datetime.fromisoformat(normalized).timestamp())


def iso_utc(timestamp_seconds: int | None) -> str | None:
    if timestamp_seconds is None:
        return None
    return datetime.fromtimestamp(timestamp_seconds, tz=timezone.utc).isoformat()


def fetch_activity_page(wallet: str, start: int, end: int, offset: int, limit: int) -> list[dict[str, Any]]:
    params = urllib.parse.urlencode(
        {
            "user": wallet,
            "limit": limit,
            "offset": offset,
            "start": start,
            "end": end,
            "sortBy": "TIMESTAMP",
            "sortDirection": "DESC",
        }
    )
    request = urllib.request.Request(
        f"{DATA_API_ACTIVITY}?{params}",
        headers={"User-Agent": "polymarket-agent-wallet-analysis/1.0"},
    )
    with urllib.request.urlopen(request, timeout=30) as response:
        payload = json.loads(response.read())
    if not isinstance(payload, list):
        raise RuntimeError(f"unexpected activity response at offset={offset}: {payload}")
    return payload


def fetch_activity(wallet: str, start: int, end: int, bucket_seconds: int, limit: int) -> tuple[list[dict[str, Any]], list[dict[str, Any]]]:
    rows: list[dict[str, Any]] = []
    coverage: list[dict[str, Any]] = []
    for bucket_start in range(start, end, bucket_seconds):
        bucket_end = min(bucket_start + bucket_seconds, end)
        bucket_rows: list[dict[str, Any]] = []
        truncated = False
        for offset in range(0, 3001, limit):
            page = fetch_activity_page(wallet, bucket_start, bucket_end, offset, limit)
            if not page:
                break
            bucket_rows.extend(page)
            if len(page) < limit:
                break
            if offset + limit > 3000:
                truncated = True
                break
        rows.extend(bucket_rows)
        coverage.append(
            {
                "start": iso_utc(bucket_start),
                "end": iso_utc(bucket_end),
                "rows": len(bucket_rows),
                "truncated_by_offset_cap": truncated,
            }
        )
    return dedupe_activity(rows), coverage


def dedupe_activity(rows: list[dict[str, Any]]) -> list[dict[str, Any]]:
    seen: set[tuple[Any, ...]] = set()
    deduped: list[dict[str, Any]] = []
    for row in rows:
        key = (
            row.get("timestamp"),
            row.get("transactionHash"),
            row.get("type"),
            row.get("asset"),
            row.get("size"),
            row.get("price"),
            row.get("slug"),
        )
        if key in seen:
            continue
        seen.add(key)
        deduped.append(row)
    return deduped


def summarize_wallet(rows: list[dict[str, Any]]) -> dict[str, Any]:
    type_counts: Counter[str] = Counter(str(row.get("type") or "") for row in rows)
    trade_rows = [row for row in rows if row.get("type") == "TRADE"]
    by_slug: dict[str, list[dict[str, Any]]] = defaultdict(list)
    for row in rows:
        if row.get("slug"):
            by_slug[str(row["slug"])].append(row)

    market_summaries: list[dict[str, Any]] = []
    for slug, market_rows in by_slug.items():
        trades = [row for row in market_rows if row.get("type") == "TRADE"]
        if not trades:
            continue
        outcomes: dict[str, dict[str, float]] = defaultdict(lambda: {"rows": 0, "shares": 0.0, "usdc": 0.0, "min_price": 9.0, "max_price": 0.0})
        for trade in trades:
            outcome = str(trade.get("outcome") or trade.get("outcomeIndex") or "unknown")
            stats = outcomes[outcome]
            stats["rows"] += 1
            stats["shares"] += float(trade.get("size") or 0.0)
            stats["usdc"] += float(trade.get("usdcSize") or 0.0)
            price = float(trade.get("price") or 0.0)
            stats["min_price"] = min(stats["min_price"], price)
            stats["max_price"] = max(stats["max_price"], price)
        normalized_outcomes = {}
        for outcome, stats in outcomes.items():
            normalized_outcomes[outcome] = {
                **stats,
                "avg_price": stats["usdc"] / stats["shares"] if stats["shares"] > 0 else None,
            }
        market_summaries.append(
            {
                "slug": slug,
                "trade_rows": len(trades),
                "trade_usdc": sum(float(row.get("usdcSize") or 0.0) for row in trades),
                "merge_rows": sum(1 for row in market_rows if row.get("type") == "MERGE"),
                "redeem_rows": sum(1 for row in market_rows if row.get("type") == "REDEEM"),
                "outcome_count": len(normalized_outcomes),
                "outcomes": normalized_outcomes,
            }
        )
    market_summaries.sort(key=lambda item: item["trade_usdc"], reverse=True)
    timestamps = [int(row["timestamp"]) for row in rows if isinstance(row.get("timestamp"), int)]
    return {
        "rows": len(rows),
        "first_ts": iso_utc(min(timestamps) if timestamps else None),
        "last_ts": iso_utc(max(timestamps) if timestamps else None),
        "type_counts": dict(type_counts),
        "trade_usdc": sum(float(row.get("usdcSize") or 0.0) for row in trade_rows),
        "markets_with_trades": len(market_summaries),
        "two_sided_trade_markets": sum(1 for item in market_summaries if item["outcome_count"] > 1),
        "top_markets": market_summaries[:20],
    }


def classify_client_order_id(client_order_id: str) -> str:
    if "mm-paired-bid" in client_order_id:
        return "paired"
    if "mm-hedge-rescue" in client_order_id:
        return "rescue"
    if "convex" in client_order_id:
        return "convex"
    return "unknown"


def summarize_journal(path: Path, start: int, end: int) -> dict[str, Any]:
    start_ms = start * 1000
    end_ms = end * 1000
    messages: Counter[str] = Counter()
    categories: Counter[str] = Counter()
    markets: Counter[str] = Counter()
    lifecycle_tags: Counter[str] = Counter()
    fill_reports: list[dict[str, Any]] = []
    free_cash_values: list[float] = []

    with path.open() as handle:
        for line in handle:
            try:
                payload = json.loads(line)
            except json.JSONDecodeError:
                continue
            record = payload.get("record") or {}
            observed_at_ms = record.get("observed_at_ms")
            if not isinstance(observed_at_ms, int) or not (start_ms <= observed_at_ms < end_ms):
                continue
            message = record.get("message") or ""
            messages[message] += 1
            categories[str(record.get("category") or "unknown")] += 1
            if record.get("market_id"):
                markets[str(record["market_id"])] += 1
            metrics = record.get("metrics") or {}
            if isinstance(metrics.get("free_cash_after_usd"), (int, float)):
                free_cash_values.append(float(metrics["free_cash_after_usd"]))
            tag = classify_client_order_id(str(record.get("client_order_id") or ""))
            if message in {
                "order acknowledged by downstream execution layer",
                "received fill report",
                "inventory updated from fill",
            }:
                lifecycle_tags[tag] += 1
            if message == "received fill report":
                fill_reports.append(
                    {
                        "observed_at": iso_utc(observed_at_ms // 1000),
                        "market_id": record.get("market_id"),
                        "tag": tag,
                        "notional_usd": metrics.get("notional_usd"),
                        "client_order_id": record.get("client_order_id"),
                    }
                )

    return {
        "events": sum(messages.values()),
        "categories": dict(categories),
        "top_messages": messages.most_common(25),
        "markets_touched": len(markets),
        "top_markets": markets.most_common(15),
        "lifecycle_tags": dict(lifecycle_tags),
        "fill_reports": len(fill_reports),
        "recent_fill_reports": fill_reports[-25:],
        "free_cash_min": min(free_cash_values) if free_cash_values else None,
        "free_cash_max": max(free_cash_values) if free_cash_values else None,
        "free_cash_last": free_cash_values[-1] if free_cash_values else None,
    }


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--wallet", default=DEFAULT_WHALE_WALLET)
    parser.add_argument("--start", required=True, help="UTC ISO timestamp or unix seconds")
    parser.add_argument("--end", required=True, help="UTC ISO timestamp or unix seconds")
    parser.add_argument("--bucket-seconds", type=int, default=3600)
    parser.add_argument("--limit", type=int, default=500)
    parser.add_argument("--journal-path", type=Path)
    parser.add_argument("--out", type=Path, default=Path("reports/wallet_live_execution_compare.json"))
    args = parser.parse_args()

    start = parse_utc(args.start)
    end = parse_utc(args.end)
    wallet_rows, coverage = fetch_activity(args.wallet, start, end, args.bucket_seconds, args.limit)
    report: dict[str, Any] = {
        "wallet": args.wallet,
        "start": iso_utc(start),
        "end": iso_utc(end),
        "wallet_fetch_coverage": coverage,
        "wallet_summary": summarize_wallet(wallet_rows),
    }
    if args.journal_path:
        report["journal_path"] = str(args.journal_path)
        report["live_execution_summary"] = summarize_journal(args.journal_path, start, end)

    args.out.parent.mkdir(parents=True, exist_ok=True)
    args.out.write_text(json.dumps(report, indent=2))
    print(json.dumps(report, indent=2))


if __name__ == "__main__":
    main()
