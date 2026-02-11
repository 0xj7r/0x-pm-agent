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
        # Bounds are pre-expanded by ±0.5 from polymarket client
        # So "35-39°F" becomes low=34.5, high=39.5
        temps = [35.0, 36.0, 37.0, 38.0, 39.0]
        assert ensemble_prob_for_bucket(temps, 34.5, 39.5) == 1.0

    def test_none_in_range(self):
        temps = [50.0, 51.0, 52.0]
        assert ensemble_prob_for_bucket(temps, 29.5, 40.5) == 0.0

    def test_or_below(self):
        temps = [30.0, 35.0, 40.0, 45.0]
        # "35 or below" → high=35.5: 30 and 35 match → 2/4
        assert ensemble_prob_for_bucket(temps, None, 35.5) == 0.5

    def test_or_higher(self):
        temps = [30.0, 35.0, 40.0, 45.0]
        # "40 or higher" → low=39.5: 40 and 45 match → 2/4
        assert ensemble_prob_for_bucket(temps, 39.5, None) == 0.5

    def test_continuous_comparison(self):
        # With expanded bounds, 35.4 and 35.6 both fall in [34.5, 35.5)
        # 35.4 is in range, 35.6 is NOT (35.6 >= 35.5)
        temps = [35.4, 35.6]
        assert ensemble_prob_for_bucket(temps, 34.5, 35.5) == 0.5

    def test_partial_range(self):
        temps = [36.0, 37.0, 38.0, 39.0, 40.0]
        # "37-38°F" → low=36.5, high=38.5: 37 and 38 → 2/5
        assert ensemble_prob_for_bucket(temps, 36.5, 38.5) == pytest.approx(0.4)
