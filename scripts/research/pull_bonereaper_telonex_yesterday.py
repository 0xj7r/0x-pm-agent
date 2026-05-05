#!/usr/bin/env python3
"""Pull Bonereaper wallet activity and matching Telonex market data.

The script keeps Telonex auth outside tracked files. Set TELONEX_API_KEY in the
environment before requesting Telonex channel downloads.
"""

from __future__ import annotations

import argparse
import csv
import hashlib
import json
import math
import os
import re
import sys
import urllib.parse
import urllib.request
from concurrent.futures import ThreadPoolExecutor, as_completed
from dataclasses import asdict, dataclass
from datetime import date, datetime, timedelta, timezone
from pathlib import Path
from typing import Any

import httpx

DEFAULT_WALLET = "0xeebde7a0e019a63e6b476eb425505b7b3e6eba30"
DATA_API = "https://data-api.polymarket.com"
BTC_5M_RE = re.compile(r"^btc-updown-5m-\d+$")
MAX_ACTIVITY_OFFSET = 3000


@dataclass(frozen=True)
class DownloadRecord:
    slug: str
    asset_id: str
    outcome: str
    requested_channel: str
    downloaded_channel: str | None
    path: str | None
    bytes_written: int
    skipped_existing: bool
    status: str
    error: str | None = None


def utc_day_bounds(day: date) -> tuple[int, int]:
    start = datetime(day.year, day.month, day.day, tzinfo=timezone.utc)
    end = start + timedelta(days=1)
    return int(start.timestamp()), int(end.timestamp())


def parse_day(raw: str | None) -> date:
    if raw:
        return date.fromisoformat(raw)
    return datetime.now(tz=timezone.utc).date() - timedelta(days=1)


def stable_row_hash(row: dict[str, Any]) -> str:
    payload = json.dumps(
        {
            "timestamp": row.get("timestamp"),
            "type": row.get("type"),
            "slug": row.get("slug"),
            "asset": row.get("asset") or row.get("asset_id"),
            "outcome": row.get("outcome"),
            "side": row.get("side"),
            "price": row.get("price"),
            "size": row.get("size"),
            "usdcSize": row.get("usdcSize"),
            "transactionHash": row.get("transactionHash") or row.get("txHash"),
        },
        sort_keys=True,
        separators=(",", ":"),
    )
    return hashlib.sha256(payload.encode("utf-8")).hexdigest()


def safe_float(value: Any) -> float:
    try:
        out = float(value)
    except (TypeError, ValueError):
        return 0.0
    return out if math.isfinite(out) else 0.0


def fetch_activity_window(
    *,
    wallet: str,
    start_ts: int,
    end_ts: int,
    limit: int,
    timeout_s: float,
) -> list[dict[str, Any]]:
    rows: list[dict[str, Any]] = []
    offset = 0
    while offset <= MAX_ACTIVITY_OFFSET:
        params = {
            "user": wallet,
            "limit": limit,
            "offset": offset,
            "start": start_ts,
            "end": end_ts,
            "sortBy": "TIMESTAMP",
            "sortDirection": "ASC",
        }
        url = f"{DATA_API}/activity?{urllib.parse.urlencode(params)}"
        req = urllib.request.Request(
            url,
            headers={"User-Agent": "polymarket-agent-bonereaper-telonex/1.0"},
        )
        with urllib.request.urlopen(req, timeout=timeout_s) as resp:
            payload = json.loads(resp.read().decode("utf-8"))
        if not isinstance(payload, list) or not payload:
            break
        rows.extend(payload)
        if len(payload) < limit:
            break
        offset += limit
    return rows


def fetch_activity_day(args: argparse.Namespace, day: date) -> list[dict[str, Any]]:
    start_ts, end_ts = utc_day_bounds(day)
    windows = []
    cursor = start_ts
    while cursor < end_ts:
        nxt = min(end_ts, cursor + args.slice_seconds)
        windows.append((cursor, nxt))
        cursor = nxt

    all_rows: list[dict[str, Any]] = []
    seen: set[str] = set()
    with ThreadPoolExecutor(max_workers=args.activity_workers) as pool:
        futures = [
            pool.submit(
                fetch_activity_window,
                wallet=args.wallet,
                start_ts=lo,
                end_ts=hi,
                limit=args.limit,
                timeout_s=args.timeout_s,
            )
            for lo, hi in windows
        ]
        for fut in as_completed(futures):
            for row in fut.result():
                key = stable_row_hash(row)
                if key in seen:
                    continue
                seen.add(key)
                row["row_hash"] = key
                all_rows.append(row)

    all_rows.sort(key=lambda r: (int(r.get("timestamp") or 0), r.get("row_hash") or ""))
    return all_rows


def btc5m_trades(rows: list[dict[str, Any]]) -> list[dict[str, Any]]:
    trades = []
    for row in rows:
        if str(row.get("type") or "").upper() != "TRADE":
            continue
        slug = str(row.get("slug") or "")
        if not BTC_5M_RE.match(slug):
            continue
        trades.append(row)
    return trades


def slug_summary(trades: list[dict[str, Any]]) -> list[dict[str, Any]]:
    by_slug: dict[str, dict[str, Any]] = {}
    for row in trades:
        slug = str(row.get("slug") or "")
        item = by_slug.setdefault(
            slug,
            {
                "slug": slug,
                "trade_count": 0,
                "notional": 0.0,
                "shares": 0.0,
                "first_ts": None,
                "last_ts": None,
                "assets": set(),
                "outcomes": set(),
            },
        )
        ts = int(row.get("timestamp") or 0)
        item["trade_count"] += 1
        item["notional"] += safe_float(row.get("usdcSize")) or safe_float(row.get("price")) * safe_float(row.get("size"))
        item["shares"] += safe_float(row.get("size"))
        item["first_ts"] = ts if item["first_ts"] is None else min(item["first_ts"], ts)
        item["last_ts"] = ts if item["last_ts"] is None else max(item["last_ts"], ts)
        asset = row.get("asset") or row.get("asset_id")
        if asset:
            item["assets"].add(str(asset))
        outcome = row.get("outcome")
        if outcome:
            item["outcomes"].add(str(outcome))

    out = []
    for item in by_slug.values():
        first_ts = item["first_ts"]
        last_ts = item["last_ts"]
        out.append(
            {
                "slug": item["slug"],
                "trade_count": item["trade_count"],
                "notional": round(item["notional"], 6),
                "shares": round(item["shares"], 6),
                "first_utc": datetime.fromtimestamp(first_ts, tz=timezone.utc).isoformat() if first_ts else None,
                "last_utc": datetime.fromtimestamp(last_ts, tz=timezone.utc).isoformat() if last_ts else None,
                "assets": sorted(item["assets"]),
                "outcomes": sorted(item["outcomes"]),
            }
        )
    return sorted(out, key=lambda r: r["slug"])


def write_activity_outputs(out_dir: Path, rows: list[dict[str, Any]], trades: list[dict[str, Any]], summary: list[dict[str, Any]]) -> None:
    out_dir.mkdir(parents=True, exist_ok=True)
    with (out_dir / "wallet_activity_raw.jsonl").open("w") as fh:
        for row in rows:
            fh.write(json.dumps(row, sort_keys=True, separators=(",", ":")) + "\n")
    with (out_dir / "btc5m_wallet_trades.jsonl").open("w") as fh:
        for row in trades:
            fh.write(json.dumps(row, sort_keys=True, separators=(",", ":")) + "\n")
    (out_dir / "btc5m_slug_summary.json").write_text(json.dumps(summary, indent=2, sort_keys=True) + "\n")
    if summary:
        with (out_dir / "btc5m_slug_summary.csv").open("w", newline="") as fh:
            writer = csv.DictWriter(fh, fieldnames=["slug", "trade_count", "notional", "shares", "first_utc", "last_utc", "assets", "outcomes"])
            writer.writeheader()
            for row in summary:
                writer.writerow({**row, "assets": ",".join(row["assets"]), "outcomes": ",".join(row["outcomes"])})

    try:
        import pyarrow as pa
        import pyarrow.parquet as pq

        if rows:
            pq.write_table(pa.Table.from_pylist(rows), out_dir / "wallet_activity_raw.parquet", compression="snappy")
        if trades:
            pq.write_table(pa.Table.from_pylist(trades), out_dir / "btc5m_wallet_trades.parquet", compression="snappy")
    except Exception as exc:
        (out_dir / "parquet_write_warning.txt").write_text(f"{type(exc).__name__}: {exc}\n")


def selected_slugs(summary: list[dict[str, Any]], args: argparse.Namespace) -> list[str]:
    rows = list(summary)
    if args.slug:
        wanted = set(args.slug)
        rows = [row for row in rows if row["slug"] in wanted]
    if args.slug_order == "notional_desc":
        rows.sort(key=lambda r: float(r["notional"]), reverse=True)
    else:
        rows.sort(key=lambda r: r["slug"])
    if args.limit_slugs is not None:
        rows = rows[: args.limit_slugs]
    return [row["slug"] for row in rows]


def load_telonex_client(research_root: Path):
    sys.path.insert(0, str(research_root))
    from data_collection.telonex_client import TelonexClient, TelonexDatasetRequest

    return TelonexClient, TelonexDatasetRequest


def market_assets_from_metadata(markets_path: Path, slugs: list[str]) -> dict[str, list[tuple[str, str]]]:
    import pandas as pd

    wanted = set(slugs)
    df = pd.read_parquet(markets_path, columns=["slug", "outcome_0", "outcome_1", "asset_id_0", "asset_id_1"])
    out: dict[str, list[tuple[str, str]]] = {}
    for row in df[df["slug"].isin(wanted)].to_dict(orient="records"):
        pairs = []
        for idx in ("0", "1"):
            asset_id = row.get(f"asset_id_{idx}")
            outcome = row.get(f"outcome_{idx}") or idx
            if asset_id:
                pairs.append((str(asset_id), str(outcome)))
        if pairs:
            out[str(row["slug"])] = pairs
    return out


def fallback_assets_from_activity(summary: list[dict[str, Any]], slugs: list[str]) -> dict[str, list[tuple[str, str]]]:
    by_slug = {row["slug"]: row for row in summary}
    out: dict[str, list[tuple[str, str]]] = {}
    for slug in slugs:
        row = by_slug.get(slug) or {}
        assets = list(row.get("assets") or [])
        outcomes = list(row.get("outcomes") or [])
        out[slug] = [(asset, outcomes[idx] if idx < len(outcomes) else "") for idx, asset in enumerate(assets)]
    return out


def download_one(args: argparse.Namespace, day: date, slug: str, asset_id: str, outcome: str, channel: str) -> DownloadRecord:
    TelonexClient, TelonexDatasetRequest = load_telonex_client(args.research_root)
    client = TelonexClient(timeout=args.timeout_s)
    channels_to_try = [channel]
    if channel == "book_snapshot_25" and args.book_fallback and args.book_fallback not in channels_to_try:
        channels_to_try.append(args.book_fallback)

    last_error = None
    for candidate in channels_to_try:
        existing = existing_telonex_file(args.out_dir, day, asset_id, candidate)
        if existing is not None and not args.overwrite:
            return DownloadRecord(
                slug=slug,
                asset_id=asset_id,
                outcome=outcome,
                requested_channel=channel,
                downloaded_channel=candidate,
                path=str(existing),
                bytes_written=existing.stat().st_size,
                skipped_existing=True,
                status="ok",
            )
        try:
            result = client.download_day(
                TelonexDatasetRequest(
                    exchange="polymarket",
                    channel=candidate,
                    day=day,
                    params={"asset_id": asset_id},
                ),
                args.out_dir / "telonex",
                overwrite=args.overwrite,
            )
            return DownloadRecord(
                slug=slug,
                asset_id=asset_id,
                outcome=outcome,
                requested_channel=channel,
                downloaded_channel=candidate,
                path=result.path,
                bytes_written=result.bytes_written,
                skipped_existing=result.skipped_existing,
                status="ok",
            )
        except Exception as exc:
            if isinstance(exc, httpx.HTTPStatusError):
                detail = exc.response.text[:500].replace("\n", " ")
                last_error = f"{type(exc).__name__}: {exc}; response={detail}"
            else:
                last_error = f"{type(exc).__name__}: {exc}"
    return DownloadRecord(
        slug=slug,
        asset_id=asset_id,
        outcome=outcome,
        requested_channel=channel,
        downloaded_channel=None,
        path=None,
        bytes_written=0,
        skipped_existing=False,
        status="error",
        error=last_error,
    )


def existing_telonex_file(out_dir: Path, day: date, asset_id: str, channel: str) -> Path | None:
    partition = (
        out_dir
        / "telonex"
        / "exchange=polymarket"
        / f"channel={channel}"
        / f"date={day.isoformat()}"
        / f"asset_id={asset_id}"
    )
    if not partition.exists():
        return None
    matches = sorted(partition.glob("*.parquet"))
    return matches[0] if matches else None


def download_telonex(args: argparse.Namespace, day: date, summary: list[dict[str, Any]], slugs: list[str]) -> tuple[list[DownloadRecord], dict[str, list[tuple[str, str]]]]:
    TelonexClient, _ = load_telonex_client(args.research_root)
    client = TelonexClient(timeout=args.timeout_s)
    metadata_result = client.download_metadata_dataset("polymarket", "markets", args.out_dir / "telonex", overwrite=args.overwrite)
    markets_path = Path(metadata_result.path)

    assets_by_slug = market_assets_from_metadata(markets_path, slugs)
    fallback = fallback_assets_from_activity(summary, slugs)
    for slug, pairs in fallback.items():
        assets_by_slug.setdefault(slug, pairs)

    jobs = []
    channels = [ch.strip() for ch in args.channels.split(",") if ch.strip()]
    for slug in slugs:
        for asset_id, outcome in assets_by_slug.get(slug, []):
            for channel in channels:
                jobs.append((slug, asset_id, outcome, channel))

    records: list[DownloadRecord] = []
    with ThreadPoolExecutor(max_workers=args.telonex_workers) as pool:
        futures = [
            pool.submit(download_one, args, day, slug, asset_id, outcome, channel)
            for slug, asset_id, outcome, channel in jobs
        ]
        for fut in as_completed(futures):
            records.append(fut.result())
    records.sort(key=lambda r: (r.slug, r.asset_id, r.requested_channel))
    return records, assets_by_slug


def infer_book_alignment(out_dir: Path, trades: list[dict[str, Any]], downloads: list[DownloadRecord], max_time_delta_us: int) -> dict[str, Any]:
    try:
        import pandas as pd
    except Exception as exc:
        return {"status": "skipped", "reason": f"pandas unavailable: {exc}"}

    book_paths = [
        record.path
        for record in downloads
        if record.status == "ok" and record.downloaded_channel in {"book_snapshot_25", "book_snapshot_5"} and record.path
    ]
    if not book_paths:
        return {"status": "skipped", "reason": "no book snapshot downloads"}

    trade_rows = []
    for row in trades:
        if str(row.get("side") or "").upper() != "BUY":
            continue
        asset = str(row.get("asset") or row.get("asset_id") or "")
        if not asset:
            continue
        trade_rows.append(
            {
                "wallet_ts_us": int(row.get("timestamp") or 0) * 1_000_000,
                "asset_id": asset,
                "slug": row.get("slug"),
                "outcome": row.get("outcome"),
                "price": safe_float(row.get("price")),
                "size": safe_float(row.get("size")),
                "notional": safe_float(row.get("usdcSize")) or safe_float(row.get("price")) * safe_float(row.get("size")),
            }
        )
    if not trade_rows:
        return {"status": "skipped", "reason": "no wallet buy trades"}

    wallet_df = pd.DataFrame(trade_rows).sort_values(["asset_id", "wallet_ts_us"])
    book_frames = []
    for path in book_paths:
        try:
            cols = ["timestamp_us", "asset_id", "slug", "outcome", "bid_price_0", "ask_price_0", "bid_size_0", "ask_size_0"]
            frame = pd.read_parquet(path, columns=cols)
            book_frames.append(frame)
        except Exception:
            continue
    if not book_frames:
        return {"status": "skipped", "reason": "could not read book snapshots"}
    book_df = pd.concat(book_frames, ignore_index=True)
    book_df["timestamp_us"] = book_df["timestamp_us"].astype("int64")
    book_df = book_df.sort_values(["asset_id", "timestamp_us"])
    for col in ["bid_price_0", "ask_price_0", "bid_size_0", "ask_size_0"]:
        book_df[col] = pd.to_numeric(book_df[col], errors="coerce")

    merged_parts = []
    for asset_id, group in wallet_df.groupby("asset_id", sort=False):
        book_group = book_df[book_df["asset_id"].astype(str) == str(asset_id)]
        if book_group.empty:
            continue
        merged = pd.merge_asof(
            group.sort_values("wallet_ts_us"),
            book_group.sort_values("timestamp_us"),
            left_on="wallet_ts_us",
            right_on="timestamp_us",
            direction="nearest",
            tolerance=max_time_delta_us,
            suffixes=("", "_book"),
        )
        merged_parts.append(merged)
    if not merged_parts:
        return {"status": "skipped", "reason": "no wallet rows matched book snapshots"}
    aligned = pd.concat(merged_parts, ignore_index=True)
    aligned = aligned.dropna(subset=["bid_price_0", "ask_price_0"])
    if aligned.empty:
        return {"status": "skipped", "reason": "book snapshots outside time tolerance"}

    tolerance = 0.005
    aligned["taker_like"] = aligned["price"] >= (aligned["ask_price_0"] - tolerance)
    aligned["maker_like"] = aligned["price"] <= (aligned["bid_price_0"] + tolerance)
    aligned["spread"] = aligned["ask_price_0"] - aligned["bid_price_0"]
    sample_path = out_dir / "wallet_book_alignment_sample.csv"
    aligned.head(500).to_csv(sample_path, index=False)
    return {
        "status": "ok",
        "matched_wallet_trades": int(len(aligned)),
        "taker_like_count": int(aligned["taker_like"].sum()),
        "maker_like_count": int(aligned["maker_like"].sum()),
        "taker_like_notional": round(float(aligned.loc[aligned["taker_like"], "notional"].sum()), 6),
        "maker_like_notional": round(float(aligned.loc[aligned["maker_like"], "notional"].sum()), 6),
        "median_spread": round(float(aligned["spread"].median()), 6),
        "sample_path": str(sample_path),
        "note": "Heuristic: wallet BUY price near prevailing ask is taker-like; near prevailing bid is maker-like.",
    }


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--date", default=None, help="UTC date, default yesterday")
    parser.add_argument("--wallet", default=os.getenv("BONEREAPER_WALLET", DEFAULT_WALLET))
    parser.add_argument("--out-dir", type=Path, default=Path("data/research/bonereaper/telonex_yesterday"))
    parser.add_argument("--research-root", type=Path, default=Path(os.getenv("POLYMARKET_RESEARCH_ROOT", "/Users/jackreid/go/polymarket-research")))
    parser.add_argument("--limit", type=int, default=500)
    parser.add_argument("--slice-seconds", type=int, default=3600)
    parser.add_argument("--activity-workers", type=int, default=6)
    parser.add_argument("--telonex-workers", type=int, default=4)
    parser.add_argument("--timeout-s", type=float, default=120.0)
    parser.add_argument("--channels", default="trades,book_snapshot_25")
    parser.add_argument("--book-fallback", default="book_snapshot_5")
    parser.add_argument("--limit-slugs", type=int, default=None)
    parser.add_argument("--slug", action="append", default=[])
    parser.add_argument("--slug-order", choices=["chronological", "notional_desc"], default="chronological")
    parser.add_argument("--overwrite", action="store_true")
    parser.add_argument("--skip-telonex", action="store_true")
    parser.add_argument("--book-align-tolerance-ms", type=int, default=2000)
    args = parser.parse_args()

    day = parse_day(args.date)
    args.out_dir.mkdir(parents=True, exist_ok=True)

    activity_rows = fetch_activity_day(args, day)
    trades = btc5m_trades(activity_rows)
    summary = slug_summary(trades)
    write_activity_outputs(args.out_dir, activity_rows, trades, summary)
    slugs = selected_slugs(summary, args)

    downloads: list[DownloadRecord] = []
    assets_by_slug: dict[str, list[tuple[str, str]]] = {}
    telonex_error = None
    if not args.skip_telonex and slugs:
        try:
            downloads, assets_by_slug = download_telonex(args, day, summary, slugs)
        except Exception as exc:
            telonex_error = f"{type(exc).__name__}: {exc}"

    inference = infer_book_alignment(args.out_dir, trades, downloads, args.book_align_tolerance_ms * 1000) if downloads else {"status": "skipped", "reason": "no Telonex downloads"}
    manifest = {
        "generated_at_utc": datetime.now(tz=timezone.utc).replace(microsecond=0).isoformat(),
        "wallet": args.wallet.lower(),
        "date": day.isoformat(),
        "out_dir": str(args.out_dir),
        "activity_rows": len(activity_rows),
        "btc5m_trade_rows": len(trades),
        "btc5m_slugs": len(summary),
        "selected_slugs": slugs,
        "assets_by_slug": {slug: [{"asset_id": a, "outcome": o} for a, o in pairs] for slug, pairs in assets_by_slug.items()},
        "downloads": [asdict(record) for record in downloads],
        "telonex_error": telonex_error,
        "book_alignment_inference": inference,
    }
    (args.out_dir / "manifest.json").write_text(json.dumps(manifest, indent=2, sort_keys=True) + "\n")
    print(json.dumps({k: manifest[k] for k in ["date", "out_dir", "activity_rows", "btc5m_trade_rows", "btc5m_slugs", "selected_slugs", "telonex_error"]}, sort_keys=True))
    print(json.dumps({"downloads_ok": sum(1 for d in downloads if d.status == "ok"), "downloads_error": sum(1 for d in downloads if d.status != "ok"), "book_alignment_inference": inference}, sort_keys=True))
    return 0 if telonex_error is None else 1


if __name__ == "__main__":
    raise SystemExit(main())
