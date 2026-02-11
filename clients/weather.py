"""NOAA GEFS ensemble weather data via Open-Meteo API.

Pulls all 21 ensemble members to build probability distributions
for temperature forecasts, then compares against Polymarket prices.
"""

from __future__ import annotations

import logging
from dataclasses import dataclass, field
from datetime import datetime, timedelta

import httpx

logger = logging.getLogger(__name__)

# Open-Meteo GEFS ensemble endpoint (free, no API key)
ENSEMBLE_URL = "https://ensemble-api.open-meteo.com/v1/ensemble"

# City coordinates for weather markets
CITY_COORDS: dict[str, tuple[float, float]] = {
    "New York": (40.7128, -74.0060),
    "Los Angeles": (34.0522, -118.2437),
    "Chicago": (41.8781, -87.6298),
    "Houston": (29.7604, -95.3698),
    "Phoenix": (33.4484, -112.0740),
    "Philadelphia": (39.9526, -75.1652),
    "San Antonio": (29.4241, -98.4936),
    "San Diego": (32.7157, -117.1611),
    "Dallas": (32.7767, -96.7970),
    "Austin": (30.2672, -97.7431),
    "Miami": (25.7617, -80.1918),
    "Denver": (39.7392, -104.9903),
    "Seattle": (47.6062, -122.3321),
    "Boston": (42.3601, -71.0589),
    "Nashville": (36.1627, -86.7816),
    "Portland": (45.5152, -122.6784),
    "Las Vegas": (36.1699, -115.1398),
    "Atlanta": (33.7490, -84.3880),
    "Minneapolis": (44.9778, -93.2650),
    "Detroit": (42.3314, -83.0458),
    "Washington DC": (38.9072, -77.0369),
    "San Francisco": (37.7749, -122.4194),
}


@dataclass
class EnsembleForecast:
    """Temperature forecast from all GEFS ensemble members."""

    city: str
    target_date: datetime
    # All 21 ensemble member forecasts (temperature in °F)
    member_temps: list[float] = field(default_factory=list)
    # Derived probabilities
    fetched_at: datetime = field(default_factory=datetime.utcnow)

    @property
    def num_members(self) -> int:
        return len(self.member_temps)

    @property
    def mean_temp(self) -> float:
        if not self.member_temps:
            return 0.0
        return sum(self.member_temps) / len(self.member_temps)

    @property
    def min_temp(self) -> float:
        return min(self.member_temps) if self.member_temps else 0.0

    @property
    def max_temp(self) -> float:
        return max(self.member_temps) if self.member_temps else 0.0

    def prob_above(self, threshold_f: float) -> float:
        """Probability that temperature is strictly above threshold (°F)."""
        if not self.member_temps:
            return 0.5
        count = sum(1 for t in self.member_temps if t > threshold_f)
        return count / len(self.member_temps)

    def prob_below(self, threshold_f: float) -> float:
        """Probability that temperature is strictly below threshold (°F)."""
        if not self.member_temps:
            return 0.5
        count = sum(1 for t in self.member_temps if t < threshold_f)
        return count / len(self.member_temps)

    def prob_in_range(self, low_f: float, high_f: float) -> float:
        """Probability that temperature falls in [low, high] range (°F)."""
        if not self.member_temps:
            return 0.0
        count = sum(1 for t in self.member_temps if low_f <= t <= high_f)
        return count / len(self.member_temps)

    def prob_per_bucket(self, bucket_size: float = 2.0) -> dict[str, float]:
        """Build probability distribution in buckets of bucket_size °F.

        Returns dict like {"78-80": 0.24, "80-82": 0.38, ...}
        """
        if not self.member_temps:
            return {}

        # Find range
        min_t = int(self.min_temp // bucket_size) * bucket_size
        max_t = (int(self.max_temp // bucket_size) + 1) * bucket_size

        buckets = {}
        t = min_t
        while t < max_t:
            label = f"{t:.0f}-{t + bucket_size:.0f}"
            count = sum(
                1 for temp in self.member_temps if t <= temp < t + bucket_size
            )
            buckets[label] = count / len(self.member_temps)
            t += bucket_size

        return buckets


class WeatherClient:
    def __init__(self):
        self._http = httpx.AsyncClient(timeout=30.0)

    async def get_ensemble_forecast(
        self,
        city: str,
        target_date: datetime | None = None,
    ) -> EnsembleForecast | None:
        """Fetch GEFS ensemble forecast for a city.

        Pulls temperature_2m from all ensemble members for the target date.
        """
        coords = CITY_COORDS.get(city)
        if not coords:
            logger.warning(f"Unknown city: {city}")
            return None

        lat, lon = coords
        if target_date is None:
            target_date = datetime.utcnow() + timedelta(days=1)

        date_str = target_date.strftime("%Y-%m-%d")

        params = {
            "latitude": lat,
            "longitude": lon,
            "hourly": "temperature_2m",
            "temperature_unit": "fahrenheit",
            "start_date": date_str,
            "end_date": date_str,
            "models": "gfs_seamless",
        }

        try:
            resp = await self._http.get(ENSEMBLE_URL, params=params)
            resp.raise_for_status()
            data = resp.json()
        except Exception as e:
            logger.error(f"Failed to fetch ensemble for {city}: {e}")
            return None

        # Parse ensemble members
        hourly = data.get("hourly", {})
        member_temps = []

        # Open-Meteo returns ensemble members as temperature_2m_member01, etc.
        # For the daily high, take the max across hours for each member
        for key, values in hourly.items():
            if key.startswith("temperature_2m_member") and values:
                # Take max temperature (daily high) for this member
                valid = [v for v in values if v is not None]
                if valid:
                    member_temps.append(max(valid))

        # Fallback: if no ensemble members, use the main forecast
        if not member_temps and "temperature_2m" in hourly:
            values = hourly["temperature_2m"]
            valid = [v for v in values if v is not None]
            if valid:
                member_temps = [max(valid)]
                logger.warning(
                    f"No ensemble members for {city}, using single forecast"
                )

        if not member_temps:
            logger.warning(f"No temperature data for {city} on {date_str}")
            return None

        forecast = EnsembleForecast(
            city=city,
            target_date=target_date,
            member_temps=member_temps,
        )

        logger.info(
            f"{city}: {forecast.num_members} members, "
            f"mean={forecast.mean_temp:.1f}°F, "
            f"range=[{forecast.min_temp:.1f}, {forecast.max_temp:.1f}]"
        )

        return forecast

    async def get_forecasts_for_cities(
        self,
        cities: list[str],
        target_date: datetime | None = None,
    ) -> dict[str, EnsembleForecast]:
        """Fetch ensemble forecasts for multiple cities in parallel."""
        import asyncio

        tasks = [
            self.get_ensemble_forecast(city, target_date) for city in cities
        ]
        results = await asyncio.gather(*tasks, return_exceptions=True)

        forecasts = {}
        for city, result in zip(cities, results):
            if isinstance(result, EnsembleForecast):
                forecasts[city] = result
            elif isinstance(result, Exception):
                logger.error(f"Error fetching {city}: {result}")

        return forecasts

    async def close(self):
        await self._http.aclose()
