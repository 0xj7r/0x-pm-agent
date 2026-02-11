"""Weather market strategy for Polymarket temperature bucket markets.

Discovers weather events via Gamma API slug pattern, builds probability
distributions from NOAA GEFS ensemble forecasts, and finds mispriced
temperature buckets.
"""

from __future__ import annotations

import json
import logging
import re
from collections import defaultdict
from dataclasses import dataclass, field
from datetime import datetime, timedelta

from clients.polymarket import PolymarketClient
from clients.weather import WeatherClient
from config import Config
from models.market import Market, MarketCategory, Outcome
from models.trade import Side, Signal, SignalSource
from strategies.base import Strategy

logger = logging.getLogger(__name__)

# City name → slug component mapping
CITY_SLUG_MAP = {
    "Seoul": "seoul",
    "London": "london",
    "Toronto": "toronto",
    "New York": "nyc",
    "NYC": "nyc",
    "Atlanta": "atlanta",
    "Ankara": "ankara",
    "Chicago": "chicago",
    "Dallas": "dallas",
    "Miami": "miami",
    "Seattle": "seattle",
    "Auckland": "auckland",
    "Buenos Aires": "buenos-aires",
}

# Reverse: slug component → canonical city name (for weather client lookup)
SLUG_TO_CITY = {v: k for k, v in CITY_SLUG_MAP.items()}
# Fix NYC duplicate
SLUG_TO_CITY["nyc"] = "New York"


@dataclass
class TemperatureBucket:
    """A parsed temperature bucket from a Polymarket sub-market."""
    label: str           # raw groupItemTitle e.g. "36-37°F"
    low: float | None    # None for "X or below" buckets
    high: float | None   # None for "X or higher" buckets
    yes_price: float
    no_price: float
    market_id: str
    condition_id: str
    question: str
    yes_token_id: str
    no_token_id: str
    active: bool
    closed: bool
    accepting_orders: bool

    @property
    def tradeable(self) -> bool:
        return self.active and not self.closed and self.accepting_orders


@dataclass
class ForecastSnapshot:
    """A single forecast observation for trend tracking."""
    city: str
    bucket_label: str
    ensemble_prob: float
    prob_bucket: str  # rounded to nearest 5%
    timestamp: datetime = field(default_factory=datetime.utcnow)


def parse_bucket_label(label: str) -> tuple[float | None, float | None]:
    """Parse groupItemTitle to extract (low, high) temperature bounds.

    Returns:
        (low, high) where None means unbounded on that side.

    Examples:
        "35°F or below"  → (None, 35.0)
        "36-37°F"        → (36.0, 37.0)
        "46°F or higher" → (46.0, None)
    """
    label = label.strip()

    # "X°F or below" / "X°F or less"
    m = re.match(r"(\d+)°?F?\s+or\s+(?:below|less)", label, re.IGNORECASE)
    if m:
        return (None, float(m.group(1)))

    # "X°F or higher" / "X°F or more" / "X°F or above"
    m = re.match(r"(\d+)°?F?\s+or\s+(?:higher|more|above)", label, re.IGNORECASE)
    if m:
        return (float(m.group(1)), None)

    # "X-Y°F" range
    m = re.match(r"(\d+)\s*[-–]\s*(\d+)°?F?", label, re.IGNORECASE)
    if m:
        return (float(m.group(1)), float(m.group(2)))

    logger.warning(f"Could not parse bucket label: {label}")
    return (None, None)


def ensemble_prob_for_bucket(
    member_temps: list[float],
    low: float | None,
    high: float | None,
) -> float:
    """Calculate fraction of ensemble members whose high temp falls in bucket.

    Polymarket buckets are inclusive on both ends based on question wording.
    """
    if not member_temps:
        return 0.0

    count = 0
    for t in member_temps:
        # Round to nearest integer to match Polymarket's integer °F buckets
        t_rounded = round(t)
        if low is None and high is not None:
            # "X or below": temp <= high
            if t_rounded <= high:
                count += 1
        elif low is not None and high is None:
            # "X or higher": temp >= low
            if t_rounded >= low:
                count += 1
        elif low is not None and high is not None:
            # "X-Y": low <= temp <= high
            if low <= t_rounded <= high:
                count += 1
    return count / len(member_temps)


class WeatherStrategy(Strategy):
    def __init__(
        self,
        config: Config,
        weather_client: WeatherClient,
        polymarket_client: PolymarketClient,
    ):
        self.config = config
        self.weather = weather_client
        self.polymarket = polymarket_client
        self.min_edge = config.MIN_EDGE_THRESHOLD
        self.enable_trend = config.ENABLE_TREND_DETECTION
        # Trend tracking
        self._forecast_history: dict[str, list[ForecastSnapshot]] = defaultdict(list)
        self._max_history = 20

    @property
    def name(self) -> str:
        return "weather"

    # ── Trend detection (preserved from original) ──

    def _trend_key(self, city: str, bucket_label: str) -> str:
        return f"{city}:{bucket_label}"

    def _bucket_prob(self, prob: float) -> str:
        return f"{round(prob * 20) * 5}"

    def _record_snapshot(self, city: str, bucket_label: str, ensemble_prob: float):
        key = self._trend_key(city, bucket_label)
        snapshot = ForecastSnapshot(
            city=city,
            bucket_label=bucket_label,
            ensemble_prob=ensemble_prob,
            prob_bucket=self._bucket_prob(ensemble_prob),
        )
        history = self._forecast_history[key]
        history.append(snapshot)
        if len(history) > self._max_history:
            self._forecast_history[key] = history[-self._max_history:]

    def _get_trend_multiplier(self, city: str, bucket_label: str) -> float:
        if not self.enable_trend:
            return 1.0

        key = self._trend_key(city, bucket_label)
        history = self._forecast_history.get(key, [])
        if len(history) < 2:
            return 1.0

        current_bucket = history[-1].prob_bucket
        streak = 1
        for snap in reversed(history[:-1]):
            if snap.prob_bucket == current_bucket:
                streak += 1
            else:
                break

        if streak >= 5:
            return 1.5
        elif streak >= 3:
            return 1.25
        elif streak >= 2:
            return 1.1
        return 1.0

    # ── Core strategy ──

    async def evaluate(self, markets: list[Market]) -> list[Signal]:
        """Discover weather events and find mispriced temperature buckets."""

        # Build date list: today and tomorrow
        now = datetime.utcnow()
        dates = [now, now + timedelta(days=1)]

        # Determine cities to scan
        cities = list(CITY_SLUG_MAP.keys())

        # A) Discover weather events from Gamma API
        logger.info(f"Discovering weather events for {len(cities)} cities, {len(dates)} dates...")
        events = await self.polymarket.get_weather_events(cities, dates)

        if not events:
            logger.info("No weather events found")
            return []

        logger.info(f"Found {len(events)} weather events with sub-markets")

        # Collect all cities we need forecasts for, with their target dates
        forecast_requests: dict[str, datetime] = {}  # city -> target_date
        for event in events:
            city = event["city"]
            target_date = event["target_date"]
            # Use the latest date if multiple
            if city not in forecast_requests or target_date > forecast_requests[city]:
                forecast_requests[city] = target_date

        # B/C) Fetch ensemble forecasts
        forecasts = {}
        for city, target_date in forecast_requests.items():
            forecast = await self.weather.get_ensemble_forecast(city, target_date)
            if forecast:
                forecasts[city] = forecast

        logger.info(f"Got ensemble forecasts for {len(forecasts)} cities")

        # D/E) Compare ensemble vs market for each bucket
        signals = []
        for event in events:
            city = event["city"]
            forecast = forecasts.get(city)
            if not forecast:
                continue

            for bucket in event["buckets"]:
                if not bucket.tradeable:
                    continue

                # Calculate ensemble probability for this bucket
                ensemble_prob = ensemble_prob_for_bucket(
                    forecast.member_temps, bucket.low, bucket.high,
                )

                market_yes = bucket.yes_price
                edge = ensemble_prob - market_yes

                # Record for trend tracking
                self._record_snapshot(city, bucket.label, ensemble_prob)
                trend_mult = self._get_trend_multiplier(city, bucket.label)

                # Confidence based on ensemble size and trend
                confidence = min(0.9, (forecast.num_members / 21) * trend_mult)

                abs_edge = abs(edge)
                if abs_edge < self.min_edge:
                    continue

                if edge > 0:
                    # Ensemble says higher prob than market → BUY YES
                    outcome = Outcome.YES
                    signal_price = market_yes
                    signal_fair = ensemble_prob
                    signal_edge = edge
                else:
                    # Ensemble says lower prob than market → BUY NO
                    outcome = Outcome.NO
                    signal_price = bucket.no_price
                    signal_fair = 1.0 - ensemble_prob
                    signal_edge = signal_fair - signal_price

                signal = Signal(
                    market_id=bucket.market_id,
                    market_question=bucket.question,
                    outcome=outcome,
                    side=Side.BUY,
                    source=SignalSource.WEATHER,
                    fair_value=signal_fair,
                    market_price=signal_price,
                    edge=signal_edge,
                    confidence=confidence,
                    reasoning=(
                        f"City: {city} | Bucket: {bucket.label} | "
                        f"Ensemble: {ensemble_prob:.2%} vs Market YES: {market_yes:.2%} | "
                        f"Edge: {edge:+.2%} | "
                        f"Forecast range: [{forecast.min_temp:.0f}, {forecast.max_temp:.0f}]°F | "
                        f"Mean: {forecast.mean_temp:.1f}°F | "
                        f"Members: {forecast.num_members}"
                    ),
                )

                logger.info(
                    f"Signal: {city} {bucket.label} | "
                    f"Ensemble: {ensemble_prob:.2%} vs Market: {market_yes:.2%} | "
                    f"Edge: {edge:+.2%} | {Side.BUY.value} {outcome.value}"
                )
                signals.append(signal)

        logger.info(f"Generated {len(signals)} weather signals")
        return signals
