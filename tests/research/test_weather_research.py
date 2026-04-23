from __future__ import annotations

from strategies.weather import WeatherStrategyParams, evaluate_weather_trade


def _row(market_id: str, probability_yes: float, yes_price: float, observed_temp_c: float) -> dict:
    return {
        "market_id": market_id,
        "lower_temp_c": 20.0,
        "upper_temp_c": 22.0,
        "lower_inclusive": 1,
        "upper_inclusive": 1,
        "yes_price": yes_price,
        "no_price": 1.0 - yes_price,
        "probability_yes": probability_yes,
        "confidence": max(probability_yes, 1.0 - probability_yes),
        "observed_temp_c": observed_temp_c,
        "as_of": "2026-04-07T00:00:00+00:00",
        "target_date": "2026-04-08",
    }


def test_weather_rows_can_be_replayed_end_to_end() -> None:
    params = WeatherStrategyParams(min_edge=0.05, max_entry=0.55, min_consensus=0.6)
    rows = [
        _row("a", 0.75, 0.35, 21.0),
        _row("b", 0.20, 0.78, 26.0),
        _row("c", 0.52, 0.50, 18.0),
    ]
    trades = [trade for row in rows if (trade := evaluate_weather_trade(row, params)) is not None]
    assert len(trades) == 2
    assert sum(trade.pnl for trade in trades) > 0
