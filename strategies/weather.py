"""Weather market strategy.

Uses NOAA GEFS ensemble forecasts to build probability distributions,
then finds mispriced weather markets on Polymarket.

The edge: government satellites + 21 independent model runs vs. retail gut feeling.
"""

from __future__ import annotations

import logging
import re

from clients.claude_client import ClaudeClient
from clients.weather import CITY_COORDS, WeatherClient
from config import Config
from models.market import Market, MarketCategory, Outcome
from models.trade import Side, Signal, SignalSource
from strategies.base import Strategy

logger = logging.getLogger(__name__)

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
                confidence=min(claude_estimate.confidence, forecast.num_members / 21),
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
