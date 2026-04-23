"""Classify a Polymarket wallet as MM / hybrid / non-MM using:
  1. Data API /activity fingerprint
  2. Etherscan V2 tx + token transfer flow to/from CTF Exchange
  3. CLOB rebates endpoint (decisive MM check)
"""
from __future__ import annotations

import argparse
import json
import os
import sys
import time
from collections import Counter, defaultdict
from datetime import datetime, timedelta, timezone
from pathlib import Path

import httpx

DATA_API = "https://data-api.polymarket.com"
CLOB_API = "https://clob.polymarket.com"
ETHERSCAN_V2 = "https://api.etherscan.io/v2/api"
POLYGON_CHAIN_ID = 137

CTF_EXCHANGE = "0x4bfb41d5b3570defd03c39a9a4d8de6bd8b8982e".lower()
NEG_RISK_EXCHANGE = "0xc5d563a36ae78145c45a50134d48a1215220f80a".lower()
CONDITIONAL_TOKENS = "0x4d97dcd97ec945f40cf65f87097ace5ea0476045".lower()
USDC_E = "0x2791bca1f2de4661ed88a30c99a7a9449aa84174".lower()

EXCHANGE_ADDRESSES = {CTF_EXCHANGE, NEG_RISK_EXCHANGE}


def fetch_activity(wallet: str, pages: int = 5, limit: int = 500) -> list[dict]:
    rows: list[dict] = []
    with httpx.Client(timeout=30.0) as client:
        for offset in range(0, pages * limit, limit):
            resp = client.get(
                f"{DATA_API}/activity",
                params={
                    "user": wallet,
                    "limit": str(limit),
                    "offset": str(offset),
                    "sortBy": "TIMESTAMP",
                    "sortDirection": "DESC",
                },
            )
            resp.raise_for_status()
            payload = resp.json()
            if not isinstance(payload, list) or not payload:
                break
            rows.extend(payload)
            if len(payload) < limit:
                break
    return rows


def summarize_activity(rows: list[dict]) -> dict:
    type_counts: Counter[str] = Counter()
    side_counts: Counter[str] = Counter()
    markets: set[str] = set()
    market_sides: dict[str, set[str]] = defaultdict(set)
    ts_min = None
    ts_max = None
    for r in rows:
        t = str(r.get("type") or "").upper()
        type_counts[t] += 1
        side = str(r.get("side") or "").upper()
        if side:
            side_counts[side] += 1
        market = r.get("conditionId") or r.get("slug") or r.get("market")
        if market:
            markets.add(str(market))
            # "outcome" tells us which leg of the binary market
            outcome = str(r.get("outcome") or r.get("asset") or "")
            if outcome:
                market_sides[str(market)].add(outcome)
        ts = r.get("timestamp") or r.get("ts")
        if isinstance(ts, (int, float)):
            ts_i = int(ts)
            ts_min = ts_i if ts_min is None else min(ts_min, ts_i)
            ts_max = ts_i if ts_max is None else max(ts_max, ts_i)
    two_sided = sum(1 for outs in market_sides.values() if len(outs) >= 2)
    return {
        "rows": len(rows),
        "type_counts": dict(type_counts),
        "side_counts": dict(side_counts),
        "market_count": len(markets),
        "two_sided_market_count": two_sided,
        "ts_min": ts_min,
        "ts_max": ts_max,
        "ts_min_iso": datetime.fromtimestamp(ts_min, tz=timezone.utc).isoformat() if ts_min else None,
        "ts_max_iso": datetime.fromtimestamp(ts_max, tz=timezone.utc).isoformat() if ts_max else None,
    }


def fetch_etherscan(wallet: str, api_key: str | None) -> dict:
    out = {
        "outbound_usdc_to_exchange": 0.0,
        "inbound_usdc_from_ctf": 0.0,
        "tokentx_rows": 0,
        "error": None,
    }
    params_common = {
        "chainid": str(POLYGON_CHAIN_ID),
        "module": "account",
        "action": "tokentx",
        "contractaddress": USDC_E,
        "address": wallet,
        "startblock": "0",
        "endblock": "99999999",
        "sort": "desc",
        "page": "1",
        "offset": "10000",
    }
    if api_key:
        params_common["apikey"] = api_key
    try:
        with httpx.Client(timeout=60.0) as client:
            resp = client.get(ETHERSCAN_V2, params=params_common)
            resp.raise_for_status()
            body = resp.json()
            if body.get("status") != "1":
                out["error"] = body.get("message") or body.get("result") or "unknown"
                return out
            rows = body.get("result") or []
            out["tokentx_rows"] = len(rows)
            wallet_lc = wallet.lower()
            for row in rows:
                value = float(row.get("value") or 0) / 1e6  # USDC.e is 6 decimals
                from_a = (row.get("from") or "").lower()
                to_a = (row.get("to") or "").lower()
                if from_a == wallet_lc and to_a in EXCHANGE_ADDRESSES:
                    out["outbound_usdc_to_exchange"] += value
                if to_a == wallet_lc and from_a == CONDITIONAL_TOKENS:
                    out["inbound_usdc_from_ctf"] += value
    except Exception as e:
        out["error"] = f"{type(e).__name__}: {e}"
    return out


def fetch_rebates(wallet: str, dates: list[str]) -> dict:
    days_with_rebates = 0
    total = 0.0
    entries_per_day: dict[str, int] = {}
    rebates_per_day: dict[str, float] = {}
    errors: dict[str, str] = {}
    with httpx.Client(timeout=30.0) as client:
        for date in dates:
            try:
                resp = client.get(
                    f"{CLOB_API}/rebates/current",
                    params={"date": date, "maker_address": wallet},
                )
                if resp.status_code != 200:
                    errors[date] = f"http {resp.status_code}: {resp.text[:200]}"
                    continue
                body = resp.json()
            except Exception as e:
                errors[date] = f"{type(e).__name__}: {e}"
                continue

            if body is None:
                entries = []
            elif isinstance(body, list):
                entries = body
            elif isinstance(body, dict):
                entries = body.get("rebates") or body.get("data") or []
                if not isinstance(entries, list):
                    entries = [entries]
            else:
                entries = []
            n = 0
            s = 0.0
            for e in entries:
                if not isinstance(e, dict):
                    continue
                n += 1
                for k in ("rebated_fees_usdc", "rebate", "amount", "rebate_amount", "usdc"):
                    v = e.get(k)
                    if isinstance(v, (int, float)):
                        s += float(v)
                        break
                    if isinstance(v, str):
                        try:
                            s += float(v)
                            break
                        except ValueError:
                            pass
            entries_per_day[date] = n
            rebates_per_day[date] = s
            if n > 0:
                days_with_rebates += 1
                total += s
    return {
        "dates_checked": dates,
        "days_with_rebates": days_with_rebates,
        "total_rebates_usdc": total,
        "entries_per_day": entries_per_day,
        "rebates_per_day": rebates_per_day,
        "errors": errors,
    }


def classify(activity: dict, chain: dict, rebates: dict) -> tuple[str, str]:
    buy = activity["side_counts"].get("BUY", 0)
    sell = activity["side_counts"].get("SELL", 0)
    merges = activity["type_counts"].get("MERGE", 0)
    splits = activity["type_counts"].get("SPLIT", 0)
    two_sided = activity["two_sided_market_count"]

    has_rebates = rebates["days_with_rebates"] > 0

    mm_fingerprint = (
        buy >= max(1, sell * 2)
        and merges > 0
        and splits == 0
        and two_sided >= 3
    )
    taker_fingerprint = (
        sell > 0
        and (buy == 0 or buy < sell * 0.5)
        and merges == 0
    )

    if has_rebates and mm_fingerprint:
        return "confirmed_maker", "high"
    if has_rebates:
        return "hybrid", "medium"
    if mm_fingerprint:
        return "hybrid", "low"  # activity looks MM but no rebate proof
    if splits > 0 and sell > buy:
        return "non_mm", "medium"
    return "non_mm", "low" if rebates.get("errors") else "medium"


def _date_range(days: int) -> list[str]:
    today = datetime.now(tz=timezone.utc).date()
    return [(today - timedelta(days=i)).isoformat() for i in range(days)]


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("wallet")
    ap.add_argument("--activity-pages", type=int, default=3)
    ap.add_argument("--rebate-days", type=int, default=10)
    ap.add_argument("--out", type=str, default=None, help="Write JSON to file")
    args = ap.parse_args()

    wallet = args.wallet.lower()
    if not wallet.startswith("0x") or len(wallet) != 42:
        print(f"bad wallet: {args.wallet}", file=sys.stderr)
        return 2

    print(f"[1/4] activity for {wallet}", file=sys.stderr)
    rows = fetch_activity(wallet, pages=args.activity_pages)
    act = summarize_activity(rows)

    print(f"[2/4] etherscan tokentx for {wallet}", file=sys.stderr)
    api_key = os.environ.get("ETHERSCAN_API_KEY") or os.environ.get("POLYGONSCAN_API_KEY")
    chain = fetch_etherscan(wallet, api_key)

    print(f"[3/4] rebates for {wallet} ({args.rebate_days} days)", file=sys.stderr)
    dates = _date_range(args.rebate_days)
    rebates = fetch_rebates(wallet, dates)

    print("[4/4] classify", file=sys.stderr)
    classification, confidence = classify(act, chain, rebates)

    result = {
        "wallet": wallet,
        "activity": act,
        "chain": chain,
        "rebates": rebates,
        "classification": classification,
        "confidence": confidence,
    }

    text = json.dumps(result, indent=2, default=str)
    if args.out:
        Path(args.out).write_text(text)
        print(f"wrote {args.out}", file=sys.stderr)
    print(text)
    return 0


if __name__ == "__main__":
    sys.exit(main())
