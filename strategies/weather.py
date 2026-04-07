"""Weather strategy scoring and trade selection."""

from __future__ import annotations

from dataclasses import dataclass

from models.weather import TemperatureBand, WeatherTrade


@dataclass(frozen=True)
class WeatherStrategyParams:
    min_edge: float = 0.08
    max_entry: float = 0.55
    min_consensus: float = 0.67
    kelly_fraction: float = 0.25
    max_bankroll_pct: float = 0.05


def binary_kelly_fraction(probability: float, price: float) -> float:
    if price <= 0 or price >= 1:
        return 0.0
    b = (1.0 - price) / price
    q = 1.0 - probability
    edge = b * probability - q
    if edge <= 0:
        return 0.0
    return edge / b


def settle_yes(price: float, won: bool) -> float:
    return (1.0 - price) if won else -price


def settle_no(price: float, yes_won: bool) -> float:
    return (1.0 - price) if not yes_won else -price


def actual_in_band(row: dict) -> bool:
    band = TemperatureBand(
        low_c=row["lower_temp_c"],
        high_c=row["upper_temp_c"],
        low_inclusive=bool(row["lower_inclusive"]),
        high_inclusive=bool(row["upper_inclusive"]),
    )
    return band.contains(float(row["observed_temp_c"]))


def evaluate_weather_trade(row: dict, params: WeatherStrategyParams) -> WeatherTrade | None:
    probability_yes = float(row["probability_yes"])
    confidence = float(row["confidence"])
    yes_price = float(row["yes_price"])
    no_price = float(row["no_price"])

    if confidence < params.min_consensus:
        return None

    yes_edge = probability_yes - yes_price
    no_probability = 1.0 - probability_yes
    no_edge = no_probability - no_price

    side = ""
    entry_price = 0.0
    model_probability = 0.0
    edge = 0.0
    pnl = 0.0
    won = False

    if yes_edge >= params.min_edge and yes_price <= params.max_entry:
        side = "YES"
        entry_price = yes_price
        model_probability = probability_yes
        edge = yes_edge
        won = actual_in_band(row)
        pnl = settle_yes(yes_price, won)
    elif no_edge >= params.min_edge and no_price <= params.max_entry:
        side = "NO"
        entry_price = no_price
        model_probability = no_probability
        edge = no_edge
        yes_won = actual_in_band(row)
        won = not yes_won
        pnl = settle_no(no_price, yes_won)
    else:
        return None

    kelly = binary_kelly_fraction(model_probability, entry_price)
    stake_fraction = min(params.max_bankroll_pct, kelly * params.kelly_fraction)
    if stake_fraction <= 0:
        return None

    return WeatherTrade(
        market_id=str(row["market_id"]),
        side=side,
        entry_price=entry_price,
        model_probability=model_probability,
        edge=edge,
        stake_fraction=stake_fraction,
        won=won,
        pnl=pnl * stake_fraction,
    )
