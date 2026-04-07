"""Weather market ingestion pipeline for Polymarket + Open-Meteo."""

from __future__ import annotations

import argparse
import logging
import re
from datetime import date, datetime, timedelta, timezone
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

GAMMA_URL = "https://gamma-api.polymarket.com/markets"
MARKET_KEYWORDS = ("temperature", "weather", "precipitation")
DEFAULT_MODELS = ("gfs_seamless", "ecmwf_ifs", "icon_seamless")
MONTH_RE = (
    r"january|february|march|april|may|june|july|august|"
    r"september|october|november|december"
)
RANGE_PATTERNS = (
    re.compile(rf"(?P<low>-?\d+(?:\.\d+)?)\s*-\s*(?P<high>-?\d+(?:\.\d+)?)\s*(?P<unit>[cf])", re.I),
    re.compile(rf"between\s+(?P<low>-?\d+(?:\.\d+)?)\s*(?P<unit>[cf])?\s+and\s+(?P<high>-?\d+(?:\.\d+)?)\s*(?P=unit)", re.I),
    re.compile(rf"(?P<value>-?\d+(?:\.\d+)?)\s*(?P<unit>[cf])\s*(?:or above|or higher|and above)", re.I),
    re.compile(rf"(?P<value>-?\d+(?:\.\d+)?)\s*(?P<unit>[cf])\+", re.I),
    re.compile(rf"(?P<value>-?\d+(?:\.\d+)?)\s*(?P<unit>[cf])\s*(?:or below|or lower|and below)", re.I),
)


def _safe_float(value, default: float = 0.0) -> float:
    try:
        return float(value)
    except (TypeError, ValueError):
        return default


def _extract_yes_no_prices(raw: dict) -> tuple[float, float]:
    yes_price = 0.5
    no_price = 0.5
    for token in raw.get("tokens", []):
        outcome = str(token.get("outcome", "")).strip().lower()
        if outcome == "yes":
            yes_price = _safe_float(token.get("price"), 0.5)
        elif outcome == "no":
            no_price = _safe_float(token.get("price"), 0.5)
    return yes_price, no_price


def _match_city(text: str) -> WeatherLocation | None:
    text_norm = text.lower()
    for key, cfg in WEATHER_CITIES.items():
        aliases = cfg["aliases"]
        if any(alias in text_norm for alias in aliases):
            return WeatherLocation(
                key=key,
                label=cfg["label"],
                latitude=cfg["latitude"],
                longitude=cfg["longitude"],
                timezone=cfg["timezone"],
                aliases=tuple(aliases),
            )
    return None


def _infer_target_date(text: str) -> date | None:
    match = re.search(rf"\b(?:on|for)\s+({MONTH_RE})\s+(\d{{1,2}})\b", text, re.I)
    if not match:
        return None
    month = datetime.strptime(match.group(1), "%B").month
    day = int(match.group(2))
    year = datetime.now(timezone.utc).year
    candidate = date(year, month, day)
    if candidate < date.today() - timedelta(days=180):
        candidate = date(year + 1, month, day)
    return candidate


def _infer_band(text: str) -> tuple[TemperatureBand, str] | None:
    lowered = text.lower()
    for pattern in RANGE_PATTERNS:
        match = pattern.search(lowered)
        if not match:
            continue
        groups = match.groupdict()
        unit = (groups.get("unit") or "c").lower()
        if "low" in groups and "high" in groups:
            return TemperatureBand.from_unit(
                float(groups["low"]),
                float(groups["high"]),
                unit,
            ), unit
        value = float(groups["value"])
        if "above" in match.group(0) or "+" in match.group(0):
            return TemperatureBand.from_unit(value, None, unit), unit
        return TemperatureBand.from_unit(None, value, unit), unit
    return None


def parse_weather_market(raw: dict) -> tuple[WeatherMarket, WeatherLocation] | None:
    question = str(raw.get("question", ""))
    description = str(raw.get("description", ""))
    text = f"{question} {description}".strip()
    text_norm = text.lower()
    if not any(keyword in text_norm for keyword in MARKET_KEYWORDS):
        return None

    location = _match_city(text)
    target_date = _infer_target_date(text)
    band_match = _infer_band(text)
    if not location or not target_date or not band_match:
        return None

    band, unit = band_match
    yes_price, no_price = _extract_yes_no_prices(raw)
    return (
        WeatherMarket(
            market_id=str(raw.get("id", "")),
            slug=str(raw.get("slug", "")),
            question=question,
            city_key=location.key,
            target_date=target_date,
            band=band,
            unit=unit,
            yes_price=yes_price,
            no_price=no_price,
            volume=_safe_float(raw.get("volume"), 0.0),
            liquidity=_safe_float(raw.get("liquidity"), 0.0),
            active=bool(raw.get("active", True)),
            raw=raw,
        ),
        location,
    )


async def fetch_weather_markets(limit: int, pages: int, active: str) -> list[tuple[WeatherMarket, WeatherLocation]]:
    params_active = {"true": "true", "false": "false"}.get(active.lower())
    results: list[tuple[WeatherMarket, WeatherLocation]] = []
    async with httpx.AsyncClient(timeout=30.0) as http:
        offset = 0
        for _ in range(pages):
            params = {"limit": limit, "offset": offset}
            if params_active is not None:
                params["active"] = params_active
                params["closed"] = "false" if params_active == "true" else "true"
            response = await http.get(GAMMA_URL, params=params)
            response.raise_for_status()
            markets = response.json()
            if not markets:
                break
            for raw in markets:
                parsed = parse_weather_market(raw)
                if parsed:
                    results.append(parsed)
            offset += limit
    return results


def _probability_from_models(forecasts: dict[str, float], band: TemperatureBand) -> tuple[float, float, float]:
    temps = list(forecasts.values())
    if not temps:
        raise ValueError("No forecast temperatures available")
    votes = sum(1 for temp in temps if band.contains(temp))
    probability_yes = votes / len(temps)
    spread = max(temps) - min(temps) if len(temps) > 1 else 0.0
    confidence = max(probability_yes, 1.0 - probability_yes)
    return probability_yes, confidence, spread if spread > 0 else 0.1


async def enrich_market(
    market: WeatherMarket,
    location: WeatherLocation,
    client: OpenMeteoClient,
    models: tuple[str, ...],
    as_of: str,
) -> tuple[list[ForecastSnapshot], float | None]:
    today = date.today()
    if market.target_date >= today:
        forecasts = await client.forecast_daily_max(location, market.target_date, market.target_date, models)
    else:
        forecasts = await client.historical_forecast_daily_max(location, market.target_date, market.target_date, models)

    probability_yes, confidence, _ = _probability_from_models(forecasts, market.band)
    snapshots = [
        ForecastSnapshot(
            market_id=market.market_id,
            as_of=as_of,
            source="open-meteo",
            model=model,
            target_date=market.target_date,
            forecast_temp_c=temp_c,
            probability_yes=probability_yes,
            confidence=confidence,
            raw={"city_key": market.city_key},
        )
        for model, temp_c in forecasts.items()
    ]

    actual = None
    if market.target_date < today:
        actual = await client.actual_daily_max(location, market.target_date)
    return snapshots, actual


async def run_fetch(db_path: Path, limit: int, pages: int, active: str, models: tuple[str, ...]) -> None:
    parsed = await fetch_weather_markets(limit=limit, pages=pages, active=active)
    if not parsed:
        logger.warning("No parseable weather markets found")
        return

    markets = [market for market, _ in parsed]
    upsert_weather_markets(markets, db_path=db_path)

    as_of = datetime.now(timezone.utc).isoformat()
    client = OpenMeteoClient()
    try:
        all_snapshots: list[ForecastSnapshot] = []
        for market, location in parsed:
            snapshots, actual = await enrich_market(market, location, client, models, as_of)
            all_snapshots.extend(snapshots)
            if actual is not None:
                upsert_actual(
                    market.market_id,
                    market.target_date.isoformat(),
                    actual,
                    raw={"city_key": market.city_key},
                    db_path=db_path,
                )
        insert_forecast_snapshots(all_snapshots, db_path=db_path)
    finally:
        await client.close()

    logger.info("Stored %s weather markets and %s forecast snapshots", len(markets), len(all_snapshots))


def main() -> None:
    logging.basicConfig(level=logging.INFO, format="%(asctime)s %(levelname)s %(message)s")
    parser = argparse.ArgumentParser()
    parser.add_argument("--db", type=Path, default=WEATHER_DB_PATH)
    parser.add_argument("--limit", type=int, default=100)
    parser.add_argument("--pages", type=int, default=10)
    parser.add_argument("--active", choices=("true", "false", "all"), default="all")
    parser.add_argument("--models", nargs="+", default=list(DEFAULT_MODELS))
    args = parser.parse_args()

    import asyncio

    asyncio.run(run_fetch(args.db, args.limit, args.pages, args.active, tuple(args.models)))


if __name__ == "__main__":
    main()
