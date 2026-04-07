from __future__ import annotations

from models.weather import TemperatureBand
from strategies.weather import WeatherStrategyParams, evaluate_weather_trade


def test_temperature_band_handles_fahrenheit_conversion() -> None:
    band = TemperatureBand.from_unit(68, 70, "f")
    assert band.contains(20.0)
    assert not band.contains(10.0)


def test_weather_strategy_takes_yes_when_edge_is_positive() -> None:
    row = {
        "market_id": "m1",
        "lower_temp_c": 20.0,
        "upper_temp_c": 22.0,
        "lower_inclusive": 1,
        "upper_inclusive": 1,
        "yes_price": 0.35,
        "no_price": 0.68,
        "probability_yes": 0.75,
        "confidence": 0.75,
        "observed_temp_c": 21.0,
    }
    trade = evaluate_weather_trade(row, WeatherStrategyParams(min_edge=0.08, max_entry=0.55))
    assert trade is not None
    assert trade.side == "YES"
    assert trade.won is True
    assert trade.pnl > 0


def test_weather_strategy_can_take_no_side() -> None:
    row = {
        "market_id": "m2",
        "lower_temp_c": 20.0,
        "upper_temp_c": 22.0,
        "lower_inclusive": 1,
        "upper_inclusive": 1,
        "yes_price": 0.72,
        "no_price": 0.22,
        "probability_yes": 0.10,
        "confidence": 0.90,
        "observed_temp_c": 25.0,
    }
    trade = evaluate_weather_trade(row, WeatherStrategyParams(min_edge=0.08, max_entry=0.55))
    assert trade is not None
    assert trade.side == "NO"
    assert trade.won is True
