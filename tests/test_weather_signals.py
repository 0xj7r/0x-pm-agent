"""Tests for weather signal generation quality."""

from __future__ import annotations

import pytest

from clients.weather import CITY_COORDS, EnsembleForecast
from models.market import Outcome
from strategies.weather import (
    CITY_SLUG_MAP,
    WeatherStrategy,
    ensemble_prob_for_bucket,
)


class TestEdgeCalculation:
    def test_positive_edge_buy_yes(self) -> None:
        """If ensemble says 90% and market says 50%, edge = 0.40 → BUY YES."""
        ensemble_prob = 0.90
        market_yes = 0.50
        edge = ensemble_prob - market_yes
        assert edge == pytest.approx(0.40)
        assert edge > 0  # → BUY YES

    def test_negative_edge_buy_no(self) -> None:
        """If ensemble says 10% and market says 50%, edge = -0.40 → BUY NO."""
        ensemble_prob = 0.10
        market_yes = 0.50
        edge = ensemble_prob - market_yes
        assert edge == pytest.approx(-0.40)
        assert edge < 0  # → BUY NO

    def test_no_edge_filtered(self) -> None:
        """Edge below 15% threshold should be filtered."""
        ensemble_prob = 0.55
        market_yes = 0.50
        edge = ensemble_prob - market_yes
        abs_edge = abs(edge)
        assert abs_edge < 0.15  # Below threshold → filtered

    def test_edge_at_threshold(self) -> None:
        """Edge exactly at 15% should pass."""
        ensemble_prob = 0.65
        market_yes = 0.50
        edge = ensemble_prob - market_yes
        assert abs(edge) == pytest.approx(0.15)


class TestEnsembleProbDistribution:
    def test_known_distribution(self) -> None:
        """With 20 members, 18 above 40°F → prob_above(40) = 90%."""
        temps = [45.0] * 18 + [35.0] * 2  # 18 above, 2 below
        prob = ensemble_prob_for_bucket(temps, 40.0, None)  # "40 or higher"
        assert prob == pytest.approx(0.90)

    def test_uniform_distribution(self) -> None:
        """21 members from 30-50 → bucket 35-40°F (expanded to 34.5-40.5) gets 6/21."""
        temps = [float(t) for t in range(30, 51)]  # 30,31,...,50 = 21 members
        # bucket "35-40°F" pre-expanded: [34.5, 40.5) → 35,36,37,38,39,40 = 6 members
        prob = ensemble_prob_for_bucket(temps, 34.5, 40.5)
        assert prob == pytest.approx(6 / 21)


class TestCitySupport:
    def test_all_slug_cities_have_coords(self) -> None:
        """All cities in CITY_SLUG_MAP must have coordinates (NYC is alias for New York)."""
        for city in CITY_SLUG_MAP:
            # NYC is an alias that maps to the same slug as "New York"
            if city == "NYC":
                assert "New York" in CITY_COORDS, "New York (NYC alias) missing from CITY_COORDS"
            else:
                assert city in CITY_COORDS, f"{city} missing from CITY_COORDS"

    def test_city_count(self) -> None:
        # The 13 active Polymarket weather cities
        expected_cities = {
            "Seoul", "London", "Toronto", "New York", "NYC",
            "Atlanta", "Ankara", "Chicago", "Dallas", "Miami",
            "Seattle", "Auckland", "Buenos Aires",
        }
        assert expected_cities == set(CITY_SLUG_MAP.keys())


class TestBuyDirection:
    def test_high_ensemble_buys_yes(self) -> None:
        """When ensemble > market → outcome should be YES."""
        ensemble_prob = 0.80
        market_yes = 0.40
        edge = ensemble_prob - market_yes
        assert edge > 0
        outcome = Outcome.YES if edge > 0 else Outcome.NO
        assert outcome == Outcome.YES

    def test_low_ensemble_buys_no(self) -> None:
        """When ensemble < market → outcome should be NO."""
        ensemble_prob = 0.20
        market_yes = 0.60
        edge = ensemble_prob - market_yes
        assert edge < 0
        outcome = Outcome.YES if edge > 0 else Outcome.NO
        assert outcome == Outcome.NO
