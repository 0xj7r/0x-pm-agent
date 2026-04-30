#!/usr/bin/env python3
"""Incrementally ingest Polymarket wallet activity into AWS.

Features:
- Full backfill or incremental mode
- Parallel pagination fetches from data-api.polymarket.com/activity
- DynamoDB watermark per wallet (latest event timestamp)
- S3 output as parquet (preferred) or jsonl.gz

Example:
python3 scripts/pull_wallet_activity_incremental.py \
  --wallets 0xb27... 0xeeb... \
  --s3-bucket my-polymarket-raw \
  --s3-prefix wallet-activity \
  --dynamodb-table polymarket-wallet-watermarks \
  --mode incremental \
  --format parquet
"""

from __future__ import annotations

import argparse
import gzip
import hashlib
import io
import json
import os
import urllib.parse
import urllib.request
import uuid
from concurrent.futures import ThreadPoolExecutor, as_completed
from dataclasses import dataclass
from datetime import datetime, timezone
from types import SimpleNamespace
from typing import Any

import boto3

DATA_API = "https://data-api.polymarket.com"
MAX_ACTIVITY_OFFSET = 3000
DEFAULT_WALLETS = [
    "0xb27bc932bf8110d8f78e55da7d5f0497a18b5b82",
    "0xeebde7a0e019a63e6b476eb425505b7b3e6eba30",
]


def utc_now() -> datetime:
    return datetime.now(tz=timezone.utc).replace(microsecond=0)


def utc_now_iso() -> str:
    return utc_now().isoformat()


def parse_ts(value: Any) -> int | None:
    if value is None:
        return None
    if isinstance(value, (int, float)):
        v = int(value)
        if v > 10**12:
            return int(v / 1000)
        return v
    if isinstance(value, str):
        s = value.strip()
        if not s:
            return None
        if s.isdigit():
            return parse_ts(int(s))
        try:
            return int(datetime.fromisoformat(s.replace("Z", "+00:00")).timestamp())
        except ValueError:
            return None
    return None


def stable_row_hash(wallet: str, row: dict[str, Any]) -> str:
    src = {
        "wallet": wallet.lower(),
        "timestamp": row.get("timestamp") or row.get("ts"),
        "type": row.get("type"),
        "conditionId": row.get("conditionId"),
        "asset": row.get("asset"),
        "outcome": row.get("outcome"),
        "side": row.get("side"),
        "size": row.get("size"),
        "price": row.get("price"),
        "txHash": row.get("transactionHash") or row.get("txHash"),
        "orderId": row.get("orderID") or row.get("orderId"),
    }
    payload = json.dumps(src, sort_keys=True, separators=(",", ":")).encode("utf-8")
    return hashlib.sha256(payload).hexdigest()


def fetch_activity_page(wallet: str, limit: int, offset: int, timeout_s: float) -> tuple[int, list[dict[str, Any]]]:
    query = urllib.parse.urlencode(
        {
            "user": wallet,
            "limit": str(limit),
            "offset": str(offset),
            "sortBy": "TIMESTAMP",
            "sortDirection": "DESC",
        }
    )
    url = f"{DATA_API}/activity?{query}"
    req = urllib.request.Request(url, headers={"User-Agent": "polymarket-wallet-ingestor/1.0"})
    with urllib.request.urlopen(req, timeout=timeout_s) as resp:
        payload = json.loads(resp.read().decode("utf-8"))
    if not isinstance(payload, list):
        payload = []
    return offset, payload


def fetch_wallet_activity_parallel(
    wallet: str,
    watermark_ts: int | None,
    mode: str,
    page_limit: int,
    max_pages: int,
    batch_pages: int,
    workers: int,
    timeout_s: float,
    overlap_seconds: int,
) -> list[dict[str, Any]]:
    out: list[dict[str, Any]] = []
    seen_hashes: set[str] = set()
    max_api_pages = (MAX_ACTIVITY_OFFSET // page_limit) + 1
    max_pages = min(max_pages, max_api_pages)
    effective_watermark = None
    if mode == "incremental" and watermark_ts is not None:
        effective_watermark = max(0, watermark_ts - overlap_seconds)

    page_idx = 0
    reached_end = False

    while page_idx < max_pages and not reached_end:
        page_slice = list(range(page_idx, min(page_idx + batch_pages, max_pages)))

        batch_results: dict[int, list[dict[str, Any]]] = {}
        with ThreadPoolExecutor(max_workers=workers) as ex:
            futs = {
                ex.submit(fetch_activity_page, wallet, page_limit, p * page_limit, timeout_s): p
                for p in page_slice
            }
            for fut in as_completed(futs):
                p = futs[fut]
                _, rows = fut.result()
                batch_results[p] = rows

        keep_going = False
        for p in sorted(batch_results):
            rows = batch_results[p]
            if not rows:
                reached_end = True
                break
            if len(rows) < page_limit:
                reached_end = True

            page_has_new = False
            for row in rows:
                ts = parse_ts(row.get("timestamp") or row.get("ts"))
                if effective_watermark is not None and ts is not None and ts <= effective_watermark:
                    continue
                h = stable_row_hash(wallet, row)
                if h in seen_hashes:
                    continue
                seen_hashes.add(h)
                row["wallet"] = wallet.lower()
                row["event_ts"] = ts
                row["ingested_at"] = utc_now_iso()
                row["row_hash"] = h
                out.append(row)
                page_has_new = True

            if page_has_new:
                keep_going = True

        if mode == "incremental" and not keep_going:
            break

        page_idx += batch_pages

    out.sort(key=lambda r: (r.get("event_ts") or 0, r.get("row_hash") or ""), reverse=True)
    return out


@dataclass
class PullStats:
    wallet: str
    fetched_rows: int
    newest_ts: int | None
    watermark_before: int | None
    watermark_after: int | None
    s3_key: str | None


class AwsStateStore:
    def __init__(self, table_name: str, region: str | None = None) -> None:
        self.ddb = boto3.resource("dynamodb", region_name=region)
        self.table = self.ddb.Table(table_name)

    def get_wallet_watermark(self, wallet: str) -> int | None:
        resp = self.table.get_item(Key={"wallet": wallet.lower()})
        item = resp.get("Item")
        if not item:
            return None
        val = item.get("max_event_ts")
        if val is None:
            return None
        return int(val)

    def update_wallet_watermark(self, wallet: str, max_event_ts: int) -> None:
        self.table.update_item(
            Key={"wallet": wallet.lower()},
            UpdateExpression="SET max_event_ts = :ts, updated_at = :u",
            ExpressionAttributeValues={
                ":ts": int(max_event_ts),
                ":u": utc_now_iso(),
            },
        )


class S3Sink:
    def __init__(self, bucket: str, prefix: str, region: str | None = None) -> None:
        self.bucket = bucket
        self.prefix = prefix.strip("/")
        self.client = boto3.client("s3", region_name=region)

    def _build_key(self, wallet: str, suffix: str) -> str:
        now = utc_now()
        day = now.strftime("%Y-%m-%d")
        stamp = now.strftime("%Y%m%dT%H%M%SZ")
        run_id = uuid.uuid4().hex[:12]
        return (
            f"{self.prefix}/wallet={wallet.lower()}/date={day}/"
            f"activity_{stamp}_{run_id}.{suffix}"
        )

    def write_jsonl_gz(self, wallet: str, rows: list[dict[str, Any]]) -> str:
        key = self._build_key(wallet, "jsonl.gz")
        buf = io.BytesIO()
        with gzip.GzipFile(fileobj=buf, mode="wb") as gz:
            for row in rows:
                gz.write(json.dumps(row, separators=(",", ":"), ensure_ascii=False).encode("utf-8"))
                gz.write(b"\n")
        buf.seek(0)
        self.client.put_object(
            Bucket=self.bucket,
            Key=key,
            Body=buf.getvalue(),
            ContentType="application/gzip",
        )
        return key

    def write_parquet(self, wallet: str, rows: list[dict[str, Any]]) -> str:
        try:
            import pyarrow as pa
            import pyarrow.parquet as pq
        except Exception as exc:
            raise RuntimeError("parquet output requires pyarrow in runtime") from exc

        key = self._build_key(wallet, "parquet")
        table = pa.Table.from_pylist(rows)
        out = io.BytesIO()
        pq.write_table(table, out, compression="snappy")
        out.seek(0)
        self.client.put_object(
            Bucket=self.bucket,
            Key=key,
            Body=out.getvalue(),
            ContentType="application/octet-stream",
        )
        return key


def run(args: argparse.Namespace) -> int:
    wallets = [w.strip().lower() for w in args.wallets if w.strip()]
    if not wallets:
        raise SystemExit("No wallets provided")

    state = AwsStateStore(args.dynamodb_table, region=args.aws_region)
    sink = S3Sink(args.s3_bucket, args.s3_prefix, region=args.aws_region)

    totals: list[PullStats] = []
    for wallet in wallets:
        watermark_before = state.get_wallet_watermark(wallet) if args.mode == "incremental" else None
        rows = fetch_wallet_activity_parallel(
            wallet=wallet,
            watermark_ts=watermark_before,
            mode=args.mode,
            page_limit=args.page_limit,
            max_pages=args.max_pages,
            batch_pages=args.batch_pages,
            workers=args.workers,
            timeout_s=args.timeout_s,
            overlap_seconds=args.overlap_seconds,
        )

        newest_ts = None
        if rows:
            newest_ts = max((r.get("event_ts") or 0) for r in rows)

        s3_key = None
        if rows:
            if args.format == "parquet":
                s3_key = sink.write_parquet(wallet, rows)
            else:
                s3_key = sink.write_jsonl_gz(wallet, rows)

        watermark_after = watermark_before
        if args.mode == "incremental" and newest_ts:
            watermark_after = max(watermark_before or 0, newest_ts)
            state.update_wallet_watermark(wallet, watermark_after)

        totals.append(
            PullStats(
                wallet=wallet,
                fetched_rows=len(rows),
                newest_ts=newest_ts,
                watermark_before=watermark_before,
                watermark_after=watermark_after,
                s3_key=s3_key,
            )
        )

    result = {
        "run_at": utc_now_iso(),
        "mode": args.mode,
        "s3_bucket": args.s3_bucket,
        "s3_prefix": args.s3_prefix,
        "dynamodb_table": args.dynamodb_table,
        "wallets": [
            {
                "wallet": s.wallet,
                "fetched_rows": s.fetched_rows,
                "newest_ts": s.newest_ts,
                "newest_ts_iso": datetime.fromtimestamp(s.newest_ts, tz=timezone.utc).isoformat()
                if s.newest_ts
                else None,
                "watermark_before": s.watermark_before,
                "watermark_after": s.watermark_after,
                "s3_key": s.s3_key,
            }
            for s in totals
        ],
    }
    print(json.dumps(result, indent=2))
    return 0


def lambda_handler(event: dict[str, Any] | None, _context: Any) -> dict[str, Any]:
    """AWS Lambda entrypoint.

    Event keys can override env defaults:
    - wallets (list[str] or comma-separated str)
    - mode (incremental|full)
    - format (parquet|jsonl)
    - s3_bucket, s3_prefix, dynamodb_table, aws_region
    - page_limit, max_pages, batch_pages, workers, timeout_s, overlap_seconds
    """

    payload = event or {}

    wallets_raw = payload.get("wallets", os.getenv("POLYMARKET_WALLETS", ""))
    if isinstance(wallets_raw, str):
        wallets = [w.strip() for w in wallets_raw.split(",") if w.strip()]
    elif isinstance(wallets_raw, list):
        wallets = [str(w).strip() for w in wallets_raw if str(w).strip()]
    else:
        wallets = DEFAULT_WALLETS
    if not wallets:
        wallets = DEFAULT_WALLETS

    args = SimpleNamespace(
        wallets=wallets,
        mode=str(payload.get("mode", os.getenv("POLYMARKET_MODE", "incremental"))),
        format=str(payload.get("format", os.getenv("POLYMARKET_OUTPUT_FORMAT", "jsonl"))),
        s3_bucket=str(payload.get("s3_bucket", os.environ["POLYMARKET_S3_BUCKET"])),
        s3_prefix=str(payload.get("s3_prefix", os.getenv("POLYMARKET_S3_PREFIX", "wallet-activity"))),
        dynamodb_table=str(
            payload.get("dynamodb_table", os.environ["POLYMARKET_DDB_TABLE"])
        ),
        aws_region=payload.get("aws_region", os.getenv("AWS_REGION")),
        page_limit=int(payload.get("page_limit", os.getenv("POLYMARKET_PAGE_LIMIT", "500"))),
        max_pages=int(payload.get("max_pages", os.getenv("POLYMARKET_MAX_PAGES", "10000"))),
        batch_pages=int(payload.get("batch_pages", os.getenv("POLYMARKET_BATCH_PAGES", "8"))),
        workers=int(payload.get("workers", os.getenv("POLYMARKET_WORKERS", "8"))),
        timeout_s=float(payload.get("timeout_s", os.getenv("POLYMARKET_TIMEOUT_S", "30"))),
        overlap_seconds=int(
            payload.get("overlap_seconds", os.getenv("POLYMARKET_OVERLAP_SECONDS", "300"))
        ),
    )

    run(args)
    return {"ok": True, "ran_at": utc_now_iso(), "wallet_count": len(wallets)}


def build_parser() -> argparse.ArgumentParser:
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument("--wallets", nargs="+", default=DEFAULT_WALLETS)
    p.add_argument("--mode", choices=["incremental", "full"], default="incremental")
    p.add_argument("--format", choices=["parquet", "jsonl"], default="parquet")
    p.add_argument("--s3-bucket", required=True)
    p.add_argument("--s3-prefix", default="wallet-activity")
    p.add_argument("--dynamodb-table", required=True)
    p.add_argument("--aws-region", default=None)
    p.add_argument("--page-limit", type=int, default=500)
    p.add_argument("--max-pages", type=int, default=10000)
    p.add_argument("--batch-pages", type=int, default=8)
    p.add_argument("--workers", type=int, default=8)
    p.add_argument("--timeout-s", type=float, default=30.0)
    p.add_argument(
        "--overlap-seconds",
        type=int,
        default=300,
        help="Incremental lookback window to avoid missing same-ts late events",
    )
    return p


if __name__ == "__main__":
    raise SystemExit(run(build_parser().parse_args()))
