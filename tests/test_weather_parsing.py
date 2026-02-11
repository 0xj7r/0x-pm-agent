"""Tests for weather bucket parsing and ensemble probability calculation."""

from __future__ import annotations

import pytest

from strategies.weather import ensemble_prob_for_bucket, parse_bucket_label


class TestParseBucketLabel:
    def test_or_below_f(self):
        assert parse_bucket_label("35°F or below") == (None, 35.0, "F")

    def test_range_f(self):
        assert parse_bucket_label("36-37°F") == (36.0, 37.0, "F")

    def test_or_higher_f(self):
        assert parse_bucket_label("46°F or higher") == (46.0, None, "F")

    def test_or_below_c(self):
        assert parse_bucket_label("6°C or below") == (None, 6.0, "C")

    def test_single_c(self):
        assert parse_bucket_label("10°C") == (10.0, 10.0, "C")

    def test_or_higher_c(self):
        assert parse_bucket_label("12°C or higher") == (12.0, None, "C")

    def test_negative_or_below_c(self):
        assert parse_bucket_label("-4°C or below") == (None, -4.0, "C")

    def test_negative_single_c(self):
        assert parse_bucket_label("-3°C") == (-3.0, -3.0, "C")

    def test_range_f_2(self):
        assert parse_bucket_label("38-39°F") == (38.0, 39.0, "F")


class TestEnsembleProbForBucket:
    def test_empty_members(self):
        assert ensemble_prob_for_bucket([], 30.0, 40.0) == 0.0

    def test_all_in_range(self):
        # "35-39°F": member temps in °F, round to int, check 35 <= t <= 39
        temps = [35.0, 36.0, 37.0, 38.0, 39.0]
        assert ensemble_prob_for_bucket(temps, 35, 39, native_unit="F") == 1.0

    def test_none_in_range(self):
        temps = [50.0, 51.0, 52.0]
        assert ensemble_prob_for_bucket(temps, 30, 40, native_unit="F") == 0.0

    def test_or_below(self):
        temps = [30.0, 35.0, 40.0, 45.0]
        # "35 or below": round(30)=30 ✓, round(35)=35 ✓ → 2/4
        assert ensemble_prob_for_bucket(temps, None, 35, native_unit="F") == 0.5

    def test_or_higher(self):
        temps = [30.0, 35.0, 40.0, 45.0]
        # "40 or higher": round(40)=40 ✓, round(45)=45 ✓ → 2/4
        assert ensemble_prob_for_bucket(temps, 40, None, native_unit="F") == 0.5

    def test_rounding(self):
        # 35.4°F rounds to 35, 35.6°F rounds to 36
        temps = [35.4, 35.6]
        # bucket "35°F": only 35.4 rounds to 35 → 1/2
        assert ensemble_prob_for_bucket(temps, 35, 35, native_unit="F") == 0.5

    def test_partial_range(self):
        temps = [36.0, 37.0, 38.0, 39.0, 40.0]
        # "37-38°F": 37 and 38 → 2/5
        assert ensemble_prob_for_bucket(temps, 37, 38, native_unit="F") == pytest.approx(0.4)

    def test_celsius_bucket(self):
        # Seoul "4°C": member temps in °F [38.0, 39.5, 40.0]
        # Convert to °C: [3.3, 3.9, 4.4] → round to [3, 4, 4]
        # Bucket "4°C": 2 of 3 match
        temps = [38.0, 39.5, 40.0]
        assert ensemble_prob_for_bucket(temps, 4, 4, native_unit="C") == pytest.approx(2/3)
