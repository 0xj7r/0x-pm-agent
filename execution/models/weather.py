"""Weather strategy domain models."""

from __future__ import annotations

from dataclasses import dataclass, field
from datetime import date


def fahrenheit_to_celsius(value: float) -> float:
    return (value - 32.0) * 5.0 / 9.0


@dataclass(frozen=True)
class WeatherLocation:
    key: str
    label: str
    latitude: float
    longitude: float
    timezone: str
    aliases: tuple[str, ...] = field(default_factory=tuple)


@dataclass(frozen=True)
class TemperatureBand:
    low_c: float | None
    high_c: float | None
    low_inclusive: bool = True
    high_inclusive: bool = True

    def contains(self, value_c: float) -> bool:
        if self.low_c is not None:
            if self.low_inclusive and value_c < self.low_c:
                return False
            if not self.low_inclusive and value_c <= self.low_c:
                return False
        if self.high_c is not None:
            if self.high_inclusive and value_c > self.high_c:
                return False
            if not self.high_inclusive and value_c >= self.high_c:
                return False
        return True

    @classmethod
    def from_unit(
        cls,
        low: float | None,
        high: float | None,
        unit: str,
        low_inclusive: bool = True,
        high_inclusive: bool = True,
    ) -> "TemperatureBand":
        unit_norm = unit.lower()

        def convert(value: float | None) -> float | None:
            if value is None:
                return None
            return fahrenheit_to_celsius(value) if unit_norm == "f" else value

        return cls(
            low_c=convert(low),
            high_c=convert(high),
            low_inclusive=low_inclusive,
            high_inclusive=high_inclusive,
        )


@dataclass(frozen=True)
class WeatherMarket:
    market_id: str
    slug: str
    question: str
    city_key: str
    target_date: date
    band: TemperatureBand
    unit: str
    yes_price: float
    no_price: float
    volume: float = 0.0
    liquidity: float = 0.0
    active: bool = True
    raw: dict = field(default_factory=dict)


@dataclass(frozen=True)
class ForecastSnapshot:
    market_id: str
    as_of: str
    source: str
    model: str
    target_date: date
    forecast_temp_c: float
    probability_yes: float
    confidence: float
    raw: dict = field(default_factory=dict)


@dataclass(frozen=True)
class WeatherTrade:
    market_id: str
    side: str
    entry_price: float
    model_probability: float
    edge: float
    stake_fraction: float
    won: bool
    pnl: float
