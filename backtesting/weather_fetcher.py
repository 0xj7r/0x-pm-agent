"""Weather market ingestion pipeline aligned to Gamma weather events."""

from __future__ import annotations

import argparse
import asyncio
import json
import logging
import re
from collections import defaultdict
from datetime import date, datetime, timezone
from pathlib import Path

import httpx

from backtesting.weather_db import (
    insert_forecast_snapshots,
    upsert_actual,
    upsert_weather_markets,
)
from clients.weather_api import OpenMeteoClient
from models.weather import ForecastSnapshot, TemperatureBand, WeatherLocation, WeatherMarket
from shared.constants import WEATHER_CITIES, WEATHER_DB_PATH

logger = logging.getLogger(__name__)

EVENTS_URL = "https://gamma-api.polymarket.com/events"
DEFAULT_MODELS = ("gfs_seamless", "ecmwf_ifs", "icon_seamless")
LIMIT = 50


def _safe_float(value, default: float = 0.0) -> float:
    try:
        return float(value)
    except (TypeError, ValueError):
        return default


def _parse_band(text: str) -> tuple[TemperatureBand, str] | None:
    normalized = text.lower()
    value_match = re.search(r"([-+]?\d+(?:\.\d+)?)\s*°?\s*([cf])?", normalized)
    if not value_match:
        return None
    val = float(value_match.group(1))
    unit = (value_match.group(2) or "c").lower()
    if "or below" in normalized or "or lower" in normalized:
        return TemperatureBand.from_unit(None, val, unit), unit
    if "or above" in normalized or "or higher" in normalized:
        return TemperatureBand.from_unit(val, None, unit), unit
    return TemperatureBand.from_unit(val, val, unit), unit


def _match_event_city(event: dict) -> WeatherLocation | None:
    def build_location(key: str) -> WeatherLocation:
        cfg = WEATHER_CITIES[key]
        return WeatherLocation(
            key=key,
            label=cfg["label"],
            latitude=cfg["latitude"],
            longitude=cfg["longitude"],
            timezone=cfg["timezone"],
            aliases=tuple(cfg["aliases"]),
        )

    for tag in event.get("tags") or []:
        slug = str(tag.get("slug", "")).lower()
        if slug in WEATHER_CITIES:
            return build_location(slug)

    text = str(event.get("title", ""))
    for key, cfg in WEATHER_CITIES.items():
        if any(alias in text.lower() for alias in cfg["aliases"]):
            return build_location(key)
    return None


def _band_from_market(market: dict) -> tuple[TemperatureBand, str] | None:
    title = market.get("groupItemTitle") or market.get("question", "")
    return _parse_band(str(title))


def parse_weather_event(event: dict) -> list[tuple[WeatherMarket, WeatherLocation]]:
    city = _match_event_city(event)
    if not city:
        return []

    event_date_str = event.get("eventDate") or event.get("startTime")
    if not event_date_str:
        return []

    try:
        event_date = date.fromisoformat(event_date_str[:10])
    except ValueError:
        return []

    markets = []
    for market in event.get("markets", []):
        band_info = _band_from_market(market)
        if not band_info:
            continue
        band, unit = band_info
        outcomes = market.get("outcomePrices") or []
        yes_price = _safe_float(outcomes[0]) if len(outcomes) >= 1 else 0.5
        no_price = _safe_float(outcomes[1]) if len(outcomes) >= 2 else 0.5
        markets.append(
            (
                WeatherMarket(
                    market_id=str(market.get("id", "")),
                    slug=str(market.get("slug", "")),
                    question=str(market.get("question", "")),
                    city_key=city.key,
                    target_date=event_date,
                    band=band,
                    unit=unit,
                    yes_price=yes_price,
                    no_price=no_price,
                    volume=_safe_float(market.get("volume"), 0.0),
                    liquidity=_safe_float(market.get("liquidity"), 0.0),
                    active=bool(market.get("active", True)),
                    raw=market,
                ),
                city,
            )
        )
    return markets


async def fetch_weather_events(limit: int, pages: int, active: str) -> list[dict]:
    params_active = {"true": "true", "false": "false"}.get(active.lower())
    params = {"category": "weather", "limit": limit}
    if params_active is not None:
        params["active"] = params_active
    events: list[dict] = []
    offset = 0
    async with httpx.AsyncClient(timeout=30.0) as http:
        for _ in range(pages):
            params["offset"] = offset
            resp = await http.get(EVENTS_URL, params=params)
            resp.raise_for_status()
            data = resp.json()
            if not data:
                break
            events.extend(data)
            offset += limit
    return events


def _probability_from_forecasts(forecasts: dict[str, float], band: TemperatureBand) -> float:
    temps = list(forecasts.values())
    if not temps:
        return 0.0
    hits = sum(1 for temp in temps if band.contains(temp))
    return hits / len(temps)


async def run_fetch(
    db_path: Path,
    limit: int,
    pages: int,
    active: str,
    models: tuple[str, ...],
) -> None:
    events = await fetch_weather_events(limit, pages, active)
    if not events:
        logger.warning("No weather events found")
        return

    parsed: list[tuple[WeatherMarket, WeatherLocation]] = []
    for event in events:
        parsed.extend(parse_weather_event(event))

    if not parsed:
        logger.warning("No parseable weather markets")
        return

    upsert_weather_markets([market for market, _ in parsed], db_path=db_path)

    groups: dict[tuple[str, date], list[tuple[WeatherMarket, WeatherLocation]]] = defaultdict(list)
    for market, location in parsed:
        groups[(market.city_key, market.target_date)].append((market, location))

    client = OpenMeteoClient()
    all_snapshots: list[ForecastSnapshot] = []
    today = date.today()

    try:
        for (city_key, target_date), entries in groups.items():
            location = entries[0][1]
            if target_date >= today:
                forecasts = await client.forecast_daily_max(location, target_date, target_date, models)
            else:
                forecasts = await client.historical_forecast_daily_max(location, target_date, target_date, models)

            actual = None
            if target_date < today:
                actual = await client.actual_daily_max(location, target_date)

            for market, _ in entries:
                prob = _probability_from_forecasts(forecasts, market.band)
                confidence = max(prob, 1.0 - prob)
                snapshot = ForecastSnapshot(
                    market_id=market.market_id,
                    as_of=datetime.now(timezone.utc).isoformat(),
                    source="open-meteo",
                    model="ensemble",
                    target_date=target_date,
                    forecast_temp_c=sum(forecasts.values()) / len(forecasts),
                    probability_yes=prob,
                    confidence=confidence,
                    raw={
                        "forecasts": forecasts,
                        "band": {
                            "low": market.band.low_c,
                            "high": market.band.high_c,
                        },
                    },
                )
                all_snapshots.append(snapshot)
                if actual is not None:
                    upsert_actual(
                        market.market_id,
                        target_date.isoformat(),
                        actual,
                        raw={"api": "open-meteo", "location": location.key},
                        db_path=db_path,
                    )
    finally:
        await client.close()

    insert_forecast_snapshots(all_snapshots, db_path=db_path)
    logger.info("Stored %s weather markets and %s forecast snapshots", len(parsed), len(all_snapshots))


def main() -> None:
    logging.basicConfig(level=logging.INFO, format="%(asctime)s %(levelname)s %(message)s")
    parser = argparse.ArgumentParser()
    parser.add_argument("--db", type=Path, default=WEATHER_DB_PATH)
    parser.add_argument("--limit", type=int, default=LIMIT)
    parser.add_argument("--pages", type=int, default=10)
    parser.add_argument("--active", choices=("true", "false", "all"), default="true")
    parser.add_argument("--models", nargs="+", default=list(DEFAULT_MODELS))
    args = parser.parse_args()
    asyncio.run(run_fetch(args.db, args.limit, args.pages, args.active, tuple(args.models)))


if __name__ == "__main__":
    main()
