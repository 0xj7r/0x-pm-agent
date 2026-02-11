"""Weather market strategy.

Uses NOAA GEFS ensemble forecasts to build probability distributions,
then finds mispriced weather markets on Polymarket.

The edge: government satellites + 21 independent model runs vs. retail gut feeling.
"""

from __future__ import annotations

import logging
import re
from collections import defaultdict
from dataclasses import dataclass, field
from datetime import datetime

from clients.claude_client import ClaudeClient
from clients.weather import CITY_COORDS, WeatherClient
from config import Config
from models.market import Market, MarketCategory, Outcome
from models.trade import Side, Signal, SignalSource
from strategies.base import Strategy

logger = logging.getLogger(__name__)


@dataclass
class ForecastSnapshot:
    """A single forecast observation for trend tracking."""

    city: str
    threshold: float
    direction: str
    ensemble_prob: float
    bucket: str  # rounded to nearest 5% for agreement detection
    timestamp: datetime = field(default_factory=datetime.utcnow)

# Common patterns in Polymarket weather questions.
QUESTION_PATTERNS = [
    r"(?:Will|will)\s+(.+?)\s+(?:high\s+)?temperature.*?(?:above|over|exceed|reach)\s+(\d+)",
    r"(?:Will|will)\s+(.+?)\s+(?:high\s+)?temperature.*?(?:below|under)\s+(\d+)",
    r"(.+?)\s+(?:high\s+)?temp.*?(?:above|over|exceed)\s+(\d+)",
    r"(.+?)\s+(?:high\s+)?temp.*?(?:below|under)\s+(\d+)",
]


def extract_temperature_threshold(question: str) -> tuple[str | None, float | None, str | None]:
    """Parse a weather market question to extract city, temperature, and direction.

    Examples:
        "Will NYC high temperature be above 80°F tomorrow?" → ("New York", 80.0, "above")
        "Will Chicago temperature reach 32°F or below?" → ("Chicago", 32.0, "below")
    """
    question_lower = question.lower()
    direction = "above" if any(w in question_lower for w in ["above", "over", "exceed"]) else "below"

    for pattern in QUESTION_PATTERNS:
        match = re.search(pattern, question, re.IGNORECASE)
        if match:
            city_raw = match.group(1).strip()
            threshold = float(match.group(2))
            return city_raw, threshold, direction

    return None, None, None


# Map common abbreviations/variants to our canonical city names
CITY_ALIASES = {
    "nyc": "New York",
    "new york city": "New York",
    "la": "Los Angeles",
    "sf": "San Francisco",
    "dc": "Washington DC",
    "washington": "Washington DC",
    "philly": "Philadelphia",
    "vegas": "Las Vegas",
}


def resolve_city(raw: str) -> str | None:
    """Map a raw city name from a market question to our canonical name."""
    lower = raw.lower().strip()

    # Check aliases
    if lower in CITY_ALIASES:
        return CITY_ALIASES[lower]

    # Check direct match
    for city in CITY_COORDS:
        if city.lower() == lower or city.lower() in lower or lower in city.lower():
            return city

    return None


class WeatherStrategy(Strategy):
    def __init__(self, config: Config, weather_client: WeatherClient, claude_client: ClaudeClient):
        self.config = config
        self.weather = weather_client
        self.claude = claude_client
        self.cities = config.WEATHER_CITIES
        self.enable_trend = config.ENABLE_TREND_DETECTION
        # Trend detection: track forecast history per (city, threshold, direction)
        self._forecast_history: dict[str, list[ForecastSnapshot]] = defaultdict(list)
        self._max_history = 20  # keep last N snapshots per key

    def _trend_key(self, city: str, threshold: float, direction: str) -> str:
        return f"{city}:{threshold}:{direction}"

    def _bucket_prob(self, prob: float) -> str:
        """Round probability to nearest 5% bucket for agreement detection."""
        return f"{round(prob * 20) * 5}"

    def _record_snapshot(self, city: str, threshold: float, direction: str, ensemble_prob: float):
        """Store a forecast snapshot for trend tracking."""
        key = self._trend_key(city, threshold, direction)
        snapshot = ForecastSnapshot(
            city=city,
            threshold=threshold,
            direction=direction,
            ensemble_prob=ensemble_prob,
            bucket=self._bucket_prob(ensemble_prob),
        )
        history = self._forecast_history[key]
        history.append(snapshot)
        if len(history) > self._max_history:
            self._forecast_history[key] = history[-self._max_history :]

    def _get_trend_multiplier(self, city: str, threshold: float, direction: str) -> float:
        """Calculate confidence multiplier based on consecutive ensemble agreement.

        If the ensemble has agreed on the same bucket for N consecutive snapshots,
        boost confidence: 1.0 (no trend), up to 1.5 (5+ consecutive agreements).
        """
        if not self.enable_trend:
            return 1.0

        key = self._trend_key(city, threshold, direction)
        history = self._forecast_history.get(key, [])

        if len(history) < 2:
            return 1.0

        # Count consecutive agreements from most recent backward
        current_bucket = history[-1].bucket
        streak = 1
        for snap in reversed(history[:-1]):
            if snap.bucket == current_bucket:
                streak += 1
            else:
                break

        if streak >= 5:
            multiplier = 1.5
        elif streak >= 3:
            multiplier = 1.25
        elif streak >= 2:
            multiplier = 1.1
        else:
            multiplier = 1.0

        if multiplier > 1.0:
            logger.info(
                f"Trend detected: {city} {direction} {threshold}°F | "
                f"Bucket {current_bucket}% agreed {streak}x | "
                f"Confidence multiplier: {multiplier:.2f}"
            )

        return multiplier

    @property
    def name(self) -> str:
        return "weather"

    async def evaluate(self, markets: list[Market]) -> list[Signal]:
        """Find mispriced weather markets using NOAA ensemble data."""
        # 1. Filter to weather markets only
        weather_markets = [m for m in markets if m.category == MarketCategory.WEATHER]
        logger.info(f"Found {len(weather_markets)} weather markets")

        if not weather_markets:
            return []

        # 2. Fetch ensemble forecasts for all relevant cities
        cities_needed = set()
        parsed_markets = []

        for market in weather_markets:
            city_raw, threshold, direction = extract_temperature_threshold(market.question)
            if city_raw and threshold and direction:
                city = resolve_city(city_raw)
                if city:
                    cities_needed.add(city)
                    parsed_markets.append((market, city, threshold, direction))
                else:
                    logger.debug(f"Unknown city '{city_raw}' in: {market.question}")
            else:
                logger.debug(f"Could not parse weather question: {market.question}")

        if not cities_needed:
            return []

        logger.info(f"Fetching ensemble forecasts for {len(cities_needed)} cities...")
        forecasts = await self.weather.get_forecasts_for_cities(list(cities_needed))

        # 3. Compare ensemble probabilities vs market prices
        signals = []
        for market, city, threshold, direction in parsed_markets:
            forecast = forecasts.get(city)
            if not forecast:
                continue

            # Calculate ensemble probability
            if direction == "above":
                ensemble_prob = forecast.prob_above(threshold)
            else:
                ensemble_prob = forecast.prob_below(threshold)

            market_prob = market.yes_price
            edge = ensemble_prob - market_prob

            # Record snapshot for trend tracking
            self._record_snapshot(city, threshold, direction, ensemble_prob)
            trend_multiplier = self._get_trend_multiplier(city, threshold, direction)

            # Build ensemble context for Claude
            bucket_dist = forecast.prob_per_bucket()
            ensemble_summary = (
                f"City: {city}\n"
                f"Ensemble members: {forecast.num_members}\n"
                f"Mean temp: {forecast.mean_temp:.1f}°F\n"
                f"Range: [{forecast.min_temp:.1f}, {forecast.max_temp:.1f}]°F\n"
                f"Ensemble P(temp {direction} {threshold}°F): {ensemble_prob:.2%}\n"
                f"Distribution: {bucket_dist}\n"
            )

            # Use Claude to refine the estimate (adds model uncertainty, checks for edge cases)
            claude_estimate = self.claude.estimate_weather_market(
                question=market.question,
                description=market.description,
                current_yes_price=market_prob,
                ensemble_summary=ensemble_summary,
            )

            # Weight ensemble data heavily (it's the real edge), Claude for refinement
            # 70% ensemble, 30% Claude
            blended_prob = 0.7 * ensemble_prob + 0.3 * claude_estimate.probability
            final_edge = blended_prob - market_prob

            # Determine trade direction
            if final_edge > 0:
                # Market underprices YES → buy YES
                outcome = Outcome.YES
                side = Side.BUY
                signal_price = market_prob
                signal_edge = final_edge
                signal_fair = blended_prob
            else:
                # Market overprices YES → buy NO
                outcome = Outcome.NO
                side = Side.BUY
                # Edge for NO = our NO probability - market NO price
                signal_price = market.no_price
                signal_fair = 1.0 - blended_prob
                signal_edge = signal_fair - signal_price

            signal = Signal(
                market_id=market.id,
                market_question=market.question,
                outcome=outcome,
                side=side,
                source=SignalSource.WEATHER,
                fair_value=signal_fair,
                market_price=signal_price,
                edge=signal_edge,
                confidence=min(claude_estimate.confidence * trend_multiplier, forecast.num_members / 21),
                reasoning=(
                    f"Ensemble: {ensemble_prob:.2%} | Claude: {claude_estimate.probability:.2%} | "
                    f"Blended: {blended_prob:.2%} | Market: {market_prob:.2%} | "
                    f"Edge: {final_edge:.2%}\n"
                    f"{claude_estimate.reasoning}"
                ),
            )

            logger.info(
                f"Weather signal: {city} {direction} {threshold}°F | "
                f"Ensemble: {ensemble_prob:.2%} vs Market: {market_prob:.2%} | "
                f"Edge: {final_edge:+.2%} | Trade: {side.value} {outcome.value}"
            )
            signals.append(signal)

        return signals
