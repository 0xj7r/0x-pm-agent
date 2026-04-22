#!/usr/bin/env python3
"""Recover older Polymarket wallet history with time-bucketed pulls.

This complements the existing newest-first /activity pulls by walking older
time windows and persisting:
  - activity rows per time bucket
  - unified deduped activity history
  - closed positions pages
  - accounting snapshot zip contents when available

Usage:
  python3 scripts/backfill_wallet_history.py \
      --wallet 0xb27bc932bf8110d8f78e55da7d5f0497a18b5b82 \
      --start-date 2026-03-20 \
      --end-date 2026-04-22
"""
from __future__ import annotations

import argparse
import csv
import io
import json
import zipfile
from collections import Counter, defaultdict
from dataclasses import dataclass
from datetime import UTC, datetime, timedelta
from pathlib import Path
from typing import Any

import httpx

DATA_API = "https://data-api.polymarket.com"
ROOT = Path(__file__).resolve().parent.parent
OUT_ROOT = ROOT / "data" / "wallet_research"


def parse_date(value: str) -> datetime:
    return datetime.strptime(value, "%Y-%m-%d").replace(tzinfo=UTC)


def ts(dt: datetime) -> int:
    return int(dt.timestamp())


def iso(dt: datetime) -> str:
    return dt.isoformat().replace("+00:00", "Z")


def infer_asset(slug: str) -> str:
    slug = (slug or "").lower()
    if slug.startswith("btc-") or slug.startswith("bitcoin-"):
        return "BTC"
    if slug.startswith("eth-") or slug.startswith("ethereum-"):
        return "ETH"
    if slug.startswith("sol-") or slug.startswith("solana-"):
        return "SOL"
    if slug.startswith("bnb-"):
        return "BNB"
    if slug.startswith("xrp-"):
        return "XRP"
    return "OTHER"


def market_family(slug: str) -> str:
    slug = (slug or "").lower()
    if "updown-5m" in slug:
        return "updown_5m"
    if "updown-15m" in slug:
        return "updown_15m"
    if "updown-1h" in slug:
        return "updown_1h"
    if "updown" in slug:
        return "updown_other"
    return "other"


def side_of(row: dict[str, Any]) -> str:
    value = str(row.get("outcome") or row.get("side") or "").lower()
    if "up" in value or value == "yes":
        return "UP"
    if "down" in value or value == "no":
        return "DOWN"
    return "UNK"


def event_key(row: dict[str, Any]) -> tuple[Any, ...]:
    return (
        row.get("transactionHash"),
        row.get("type"),
        row.get("slug"),
        row.get("conditionId"),
        row.get("size"),
        row.get("price"),
        row.get("timestamp"),
    )


@dataclass(frozen=True)
class Window:
    start: datetime
    end: datetime

    @property
    def label(self) -> str:
        return f"{self.start.strftime('%Y%m%dT%H%M%SZ')}_{self.end.strftime('%Y%m%dT%H%M%SZ')}"


def iter_windows(start: datetime, end: datetime, bucket_hours: int) -> list[Window]:
    windows: list[Window] = []
    cur = start
    step = timedelta(hours=bucket_hours)
    while cur < end:
        nxt = min(end, cur + step)
        windows.append(Window(cur, nxt))
        cur = nxt
    return windows


def fetch_activity_window(
    client: httpx.Client,
    wallet: str,
    window: Window,
    limit: int,
    sleep_free: bool = True,
) -> list[dict[str, Any]]:
    rows: list[dict[str, Any]] = []
    offset = 0
    while True:
        resp = client.get(
            f"{DATA_API}/activity",
            params={
                "user": wallet,
                "limit": str(limit),
                "offset": str(offset),
                "start": str(ts(window.start)),
                "end": str(ts(window.end)),
            },
        )
        if resp.status_code == 400:
            break
        resp.raise_for_status()
        batch = resp.json()
        if not isinstance(batch, list) or not batch:
            break
        rows.extend(d for d in batch if isinstance(d, dict))
        if len(batch) < limit:
            break
        offset += limit
        if offset > 10000:
            break
    return rows


def fetch_closed_positions(client: httpx.Client, wallet: str, limit: int = 500) -> list[dict[str, Any]]:
    rows: list[dict[str, Any]] = []
    offset = 0
    while True:
        resp = client.get(
            f"{DATA_API}/closed-positions",
            params={
                "user": wallet,
                "limit": str(limit),
                "offset": str(offset),
                "sortBy": "TIMESTAMP",
                "sortDirection": "DESC",
            },
        )
        resp.raise_for_status()
        batch = resp.json()
        if not isinstance(batch, list) or not batch:
            break
        rows.extend(d for d in batch if isinstance(d, dict))
        if len(batch) < limit:
            break
        offset += limit
        if offset > 10000:
            break
    return rows


def fetch_accounting_snapshot(client: httpx.Client, wallet: str) -> bytes | None:
    resp = client.get(f"{DATA_API}/v1/accounting/snapshot", params={"user": wallet})
    if resp.status_code >= 400:
        return None
    return resp.content


def save_json(path: Path, payload: object) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(json.dumps(payload, indent=2))


def summarize_activity(rows: list[dict[str, Any]]) -> dict[str, Any]:
    type_counts = Counter(str(r.get("type") or "") for r in rows)
    asset_counts = Counter(infer_asset(str(r.get("slug") or "")) for r in rows)
    family_counts = Counter(market_family(str(r.get("slug") or "")) for r in rows)
    by_slug: dict[str, dict[str, Any]] = defaultdict(
        lambda: {
            "asset": "OTHER",
            "family": "other",
            "trade_rows": 0,
            "merge_rows": 0,
            "redeem_rows": 0,
            "up_rows": 0,
            "down_rows": 0,
            "up_usd": 0.0,
            "down_usd": 0.0,
        }
    )
    for row in rows:
        slug = str(row.get("slug") or "")
        rec = by_slug[slug]
        rec["asset"] = infer_asset(slug)
        rec["family"] = market_family(slug)
        typ = str(row.get("type") or "")
        if typ == "TRADE":
            rec["trade_rows"] += 1
            usd = float(row.get("usdcSize") or 0.0)
            side = side_of(row)
            if side == "UP":
                rec["up_rows"] += 1
                rec["up_usd"] += usd
            elif side == "DOWN":
                rec["down_rows"] += 1
                rec["down_usd"] += usd
        elif typ == "MERGE":
            rec["merge_rows"] += 1
        elif typ == "REDEEM":
            rec["redeem_rows"] += 1

    two_sided = 0
    merge_markets = 0
    for rec in by_slug.values():
        if rec["up_rows"] and rec["down_rows"]:
            two_sided += 1
        if rec["merge_rows"]:
            merge_markets += 1

    timestamps = [int(r["timestamp"]) for r in rows if r.get("timestamp") is not None]
    return {
        "rows": len(rows),
        "type_counts": dict(type_counts),
        "asset_counts": dict(asset_counts),
        "family_counts": dict(family_counts),
        "market_count": len(by_slug),
        "two_sided_market_count": two_sided,
        "merge_market_count": merge_markets,
        "earliest_timestamp": min(timestamps) if timestamps else None,
        "latest_timestamp": max(timestamps) if timestamps else None,
        "top_markets": [
            {
                "slug": slug,
                "asset": rec["asset"],
                "family": rec["family"],
                "trade_rows": rec["trade_rows"],
                "merge_rows": rec["merge_rows"],
                "two_sided": bool(rec["up_rows"] and rec["down_rows"]),
                "imbalance": round(
                    abs(rec["up_usd"] - rec["down_usd"]) / (rec["up_usd"] + rec["down_usd"]),
                    3,
                )
                if (rec["up_usd"] + rec["down_usd"]) > 0
                else None,
            }
            for slug, rec in sorted(by_slug.items(), key=lambda kv: -kv[1]["trade_rows"])[:20]
        ],
    }


def summarize_closed_positions(rows: list[dict[str, Any]]) -> dict[str, Any]:
    asset_counts = Counter(infer_asset(str(r.get("slug") or "")) for r in rows)
    family_counts = Counter(market_family(str(r.get("slug") or "")) for r in rows)
    realized = [float(r.get("realizedPnl") or 0.0) for r in rows]
    return {
        "rows": len(rows),
        "asset_counts": dict(asset_counts),
        "family_counts": dict(family_counts),
        "positive_rows": sum(1 for x in realized if x > 0),
        "negative_rows": sum(1 for x in realized if x < 0),
        "net_realized_pnl": round(sum(realized), 2),
        "top_wins": sorted(
            (
                {
                    "slug": r.get("slug"),
                    "outcome": r.get("outcome"),
                    "realizedPnl": r.get("realizedPnl"),
                    "totalBought": r.get("totalBought"),
                    "avgPrice": r.get("avgPrice"),
                    "timestamp": r.get("timestamp"),
                }
                for r in rows
            ),
            key=lambda r: float(r.get("realizedPnl") or 0.0),
            reverse=True,
        )[:20],
        "top_losses": sorted(
            (
                {
                    "slug": r.get("slug"),
                    "outcome": r.get("outcome"),
                    "realizedPnl": r.get("realizedPnl"),
                    "totalBought": r.get("totalBought"),
                    "avgPrice": r.get("avgPrice"),
                    "timestamp": r.get("timestamp"),
                }
                for r in rows
            ),
            key=lambda r: float(r.get("realizedPnl") or 0.0),
        )[:20],
    }


def write_accounting_snapshot(zip_bytes: bytes, out_dir: Path) -> dict[str, Any]:
    out_dir.mkdir(parents=True, exist_ok=True)
    meta: dict[str, Any] = {"files": []}
    with zipfile.ZipFile(io.BytesIO(zip_bytes)) as zf:
        for name in zf.namelist():
            data = zf.read(name)
            target = out_dir / name
            target.write_bytes(data)
            row_count = None
            if name.endswith(".csv"):
                try:
                    row_count = sum(1 for _ in csv.reader(io.StringIO(data.decode("utf-8")))) - 1
                except Exception:
                    row_count = None
            meta["files"].append(
                {
                    "name": name,
                    "size_bytes": len(data),
                    "row_count": row_count,
                }
            )
    return meta


def build_args() -> argparse.Namespace:
    ap = argparse.ArgumentParser()
    ap.add_argument("--wallet", required=True)
    ap.add_argument("--label", default="")
    ap.add_argument("--start-date", required=True, help="UTC date YYYY-MM-DD")
    ap.add_argument("--end-date", required=True, help="UTC date YYYY-MM-DD (exclusive)")
    ap.add_argument("--bucket-hours", type=int, default=24)
    ap.add_argument("--limit", type=int, default=500)
    return ap.parse_args()


def main() -> None:
    args = build_args()
    wallet = args.wallet.lower()
    label = args.label or wallet[-6:]
    start = parse_date(args.start_date)
    end = parse_date(args.end_date)
    out_dir = OUT_ROOT / f"history_{label}"
    windows_dir = out_dir / "activity_windows"
    snapshot_dir = out_dir / "accounting_snapshot"
    windows = iter_windows(start, end, bucket_hours=args.bucket_hours)

    all_rows: list[dict[str, Any]] = []
    window_summaries: list[dict[str, Any]] = []

    with httpx.Client(timeout=60) as client:
        for window in windows:
            rows = fetch_activity_window(client, wallet, window, limit=args.limit)
            save_json(windows_dir / f"{window.label}.json", rows)
            summary = {
                "window_start": iso(window.start),
                "window_end": iso(window.end),
                "rows": len(rows),
                "type_counts": dict(Counter(str(r.get("type") or "") for r in rows)),
                "asset_counts": dict(Counter(infer_asset(str(r.get("slug") or "")) for r in rows)),
            }
            window_summaries.append(summary)
            all_rows.extend(rows)

        deduped: list[dict[str, Any]] = []
        seen: set[tuple[Any, ...]] = set()
        for row in sorted(all_rows, key=lambda r: int(r.get("timestamp") or 0)):
            key = event_key(row)
            if key in seen:
                continue
            seen.add(key)
            deduped.append(row)

        save_json(out_dir / "activity_history.json", deduped)
        save_json(out_dir / "activity_windows_summary.json", window_summaries)

        closed_positions = fetch_closed_positions(client, wallet)
        save_json(out_dir / "closed_positions.json", closed_positions)

        accounting_zip = fetch_accounting_snapshot(client, wallet)
        accounting_meta = None
        if accounting_zip:
            (out_dir / "accounting_snapshot.zip").write_bytes(accounting_zip)
            accounting_meta = write_accounting_snapshot(accounting_zip, snapshot_dir)
            save_json(out_dir / "accounting_snapshot_meta.json", accounting_meta)

    summary = {
        "wallet": wallet,
        "label": label,
        "fetched_at": iso(datetime.now(tz=UTC)),
        "activity": summarize_activity(deduped),
        "closed_positions": summarize_closed_positions(closed_positions),
        "windows_requested": len(windows),
        "windows_with_rows": sum(1 for s in window_summaries if s["rows"] > 0),
        "bucket_hours": args.bucket_hours,
        "start_date": args.start_date,
        "end_date": args.end_date,
        "accounting_snapshot": accounting_meta,
    }
    save_json(out_dir / "summary.json", summary)
    print(json.dumps(summary, indent=2))
    print(f"Saved: {out_dir}")


if __name__ == "__main__":
    main()
