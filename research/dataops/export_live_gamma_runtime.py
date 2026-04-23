#!/usr/bin/env python3
"""Export live BTC 5m runtime env/context directly from Gamma."""

from __future__ import annotations

import argparse
import json
import shlex
import time
from datetime import datetime, timezone
from pathlib import Path
from urllib.error import HTTPError
from urllib.request import Request, urlopen


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser()
    parser.add_argument("--env-out", required=True, help="Output env file path")
    parser.add_argument("--context-out", help="Optional output context JSON path")
    parser.add_argument("--slug", help="Optional exact BTC 5m slug override")
    parser.add_argument(
        "--epoch",
        type=int,
        help="Optional UTC epoch seconds to anchor the 5m slug selection",
    )
    return parser.parse_args()


def iso_to_ms(value: str | None) -> int | None:
    if not value:
        return None
    normalized = value.replace("Z", "+00:00")
    dt = datetime.fromisoformat(normalized)
    if dt.tzinfo is None:
        dt = dt.replace(tzinfo=timezone.utc)
    return int(dt.timestamp() * 1000)


def fetch_json(url: str) -> list[dict]:
    request = Request(url, headers={"User-Agent": "polymarket-agent/1.0"})
    with urlopen(request, timeout=10) as response:
        return json.loads(response.read().decode("utf-8"))


def fetch_live_event(anchor_epoch: int, slug_override: str | None = None) -> dict:
    if slug_override:
        candidate_epochs = [int(slug_override.rsplit("-", 1)[1])]
    else:
        base = anchor_epoch - (anchor_epoch % 300)
        candidate_epochs = []
        for step in range(0, 7):
            candidate_epochs.append(base - step * 300)
            if step > 0:
                candidate_epochs.append(base + step * 300)
    for event_epoch in candidate_epochs:
        slug = f"btc-updown-5m-{event_epoch}"
        url = f"https://gamma-api.polymarket.com/events?slug={slug}"
        try:
            payload = fetch_json(url)
        except HTTPError:
            continue
        if not payload:
            continue
        event = payload[0]
        markets = event.get("markets") or []
        if not markets:
            continue
        market = markets[0]
        if market.get("acceptingOrders") or (
            market.get("active") and not market.get("closed") and market.get("enableOrderBook", True)
        ):
            event["_resolved_slug"] = slug
            return event
    raise SystemExit("no active BTC 5m Gamma event found for current/adjacent buckets")


def main() -> None:
    args = parse_args()
    anchor_epoch = args.epoch or int(time.time())
    event = fetch_live_event(anchor_epoch, args.slug)
    market = event["markets"][0]
    token_ids = json.loads(market["clobTokenIds"])

    payload = {
        "WHALE_PAIR_ASSET_IDS": ",".join(token_ids),
        "WHALE_PAIR_INSTRUMENT_MARKETS": ",".join(
            f"{token_id}:{market['id']}" for token_id in token_ids
        ),
        "WHALE_PAIR_USER_MARKETS": str(market["id"]),
        "WHALE_PAIR_LATEST_MARKET_SLUG": market["slug"],
        "WHALE_PAIR_LATEST_MARKET_END_TIME": market.get("endDate"),
    }
    env_lines = [f"{key}={shlex.quote(str(value))}" for key, value in payload.items()]
    env_path = Path(args.env_out)
    env_path.parent.mkdir(parents=True, exist_ok=True)
    env_path.write_text("\n".join(env_lines) + "\n")

    if args.context_out:
        context_payload = [
            {
                "market_id": str(market["id"]),
                "instrument_ids": token_ids,
                "slug": market["slug"],
                "series_slug": event.get("seriesSlug"),
                "price_to_beat": (event.get("eventMetadata") or {}).get("priceToBeat"),
                "final_price": (event.get("eventMetadata") or {}).get("finalPrice"),
                "event_start_time_ms": iso_to_ms(market.get("eventStartTime")),
                "event_end_time_ms": iso_to_ms(market.get("endDate")),
            }
        ]
        context_path = Path(args.context_out)
        context_path.parent.mkdir(parents=True, exist_ok=True)
        context_path.write_text(json.dumps(context_payload, indent=2))

    print(
        json.dumps(
            {
                "slug": market["slug"],
                "market_id": str(market["id"]),
                "asset_count": len(token_ids),
                "env_out": str(env_path),
                "context_out": args.context_out,
            }
        )
    )


if __name__ == "__main__":
    main()
