"""Open-Meteo client helpers for weather strategy research."""

from __future__ import annotations

from collections.abc import Iterable
from datetime import date
from typing import Any

import httpx

from models.weather import WeatherLocation

FORECAST_API = "https://api.open-meteo.com/v1/forecast"
ARCHIVE_API = "https://archive-api.open-meteo.com/v1/archive"
HISTORICAL_FORECAST_API = "https://historical-forecast-api.open-meteo.com/v1/forecast"


class OpenMeteoClient:
    def __init__(self, timeout: float = 30.0) -> None:
        self._http = httpx.AsyncClient(timeout=timeout)

    async def close(self) -> None:
        await self._http.aclose()

    async def forecast_daily_max(
        self,
        location: WeatherLocation,
        start_date: date,
        end_date: date,
        models: Iterable[str],
    ) -> dict[str, float]:
        results: dict[str, float] = {}
        for model in models:
            payload = await self._get_json(
                FORECAST_API,
                {
                    "latitude": location.latitude,
                    "longitude": location.longitude,
                    "models": model,
                    "daily": "temperature_2m_max",
                    "start_date": start_date.isoformat(),
                    "end_date": end_date.isoformat(),
                    "timezone": location.timezone,
                    "temperature_unit": "celsius",
                },
            )
            results[model] = self._extract_daily_value(payload, "temperature_2m_max", start_date)
        return results

    async def historical_forecast_daily_max(
        self,
        location: WeatherLocation,
        start_date: date,
        end_date: date,
        models: Iterable[str],
    ) -> dict[str, float]:
        results: dict[str, float] = {}
        for model in models:
            payload = await self._get_json(
                HISTORICAL_FORECAST_API,
                {
                    "latitude": location.latitude,
                    "longitude": location.longitude,
                    "models": model,
                    "daily": "temperature_2m_max",
                    "start_date": start_date.isoformat(),
                    "end_date": end_date.isoformat(),
                    "timezone": location.timezone,
                    "temperature_unit": "celsius",
                },
            )
            results[model] = self._extract_daily_value(payload, "temperature_2m_max", start_date)
        return results

    async def actual_daily_max(
        self,
        location: WeatherLocation,
        target_date: date,
        model: str = "best_match",
    ) -> float:
        payload = await self._get_json(
            ARCHIVE_API,
            {
                "latitude": location.latitude,
                "longitude": location.longitude,
                "models": model,
                "daily": "temperature_2m_max",
                "start_date": target_date.isoformat(),
                "end_date": target_date.isoformat(),
                "timezone": location.timezone,
                "temperature_unit": "celsius",
            },
        )
        return self._extract_daily_value(payload, "temperature_2m_max", target_date)

    async def _get_json(self, url: str, params: dict[str, Any]) -> dict[str, Any]:
        response = await self._http.get(url, params=params)
        response.raise_for_status()
        return response.json()

    @staticmethod
    def _extract_daily_value(payload: dict[str, Any], field: str, target_date: date) -> float:
        daily = payload.get("daily") or {}
        dates = daily.get("time") or []
        values = daily.get(field) or []
        target = target_date.isoformat()
        if target not in dates:
            raise ValueError(f"Missing {field} value for {target}")
        idx = dates.index(target)
        return float(values[idx])
