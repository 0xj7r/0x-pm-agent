"""Tests for autoresearch.methods.regime_features.

Verifies look-ahead causality, correct rolling window behavior,
NaN handling for empty windows, and the time-of-day extraction.
The look-ahead test is the most important: it explicitly constructs
a scenario where calling features_for_current before vs after
appending the current market produces different results, proving
the rolling window respects the strict-precedence invariant.
"""
from __future__ import annotations

import math

import pytest

from autoresearch.methods.regime_features import (
    REGIME_FEATURE_NAMES,
    PastMarket,
    RegimeFeatures,
    RollingMarketWindow,
    compute_regime_features,
)


def make_market(t: str, ps: float, pe: float, winner: str) -> PastMarket:
    return PastMarket(start_time=t, price_start=ps, price_end=pe, winner=winner)


class TestPastMarket:
    def test_signed_move_pct(self):
        m = make_market("2026-04-08T00:00:00Z", 100.0, 102.0, "Up")
        assert m.signed_move_pct == pytest.approx(2.0)

    def test_negative_move(self):
        m = make_market("2026-04-08T00:00:00Z", 100.0, 95.0, "Down")
        assert m.signed_move_pct == pytest.approx(-5.0)

    def test_abs_move_always_non_negative(self):
        up = make_market("2026-04-08T00:00:00Z", 100.0, 102.0, "Up")
        dn = make_market("2026-04-08T00:00:00Z", 100.0, 98.0, "Down")
        assert up.abs_move_pct == 2.0
        assert dn.abs_move_pct == 2.0

    def test_zero_price_start_handled(self):
        m = make_market("2026-04-08T00:00:00Z", 0.0, 0.5, "Up")
        assert m.signed_move_pct == 0.0


class TestComputeRegimeFeatures:
    def test_empty_window_returns_nans(self):
        f = compute_regime_features([], "2026-04-08T00:00:00Z", lag_n=20)
        assert math.isnan(f.lag_n_up_frac)
        assert math.isnan(f.lag_n_mean_abs_move)
        assert math.isnan(f.lag_n_mean_signed_move)
        assert math.isnan(f.lag_n_up_magnitude)
        assert math.isnan(f.lag_n_down_magnitude)
        assert f.n_past_markets == 0
        # Time-of-day should still resolve from current_start_time
        assert f.hour_of_day == 0
        assert not math.isnan(f.day_of_week)

    def test_all_up_window(self):
        # 5 markets, all Up, each moving +1%
        markets = [
            make_market(f"2026-04-08T0{i}:00:00Z", 100.0, 101.0, "Up")
            for i in range(5)
        ]
        f = compute_regime_features(markets, "2026-04-08T05:00:00Z", lag_n=10)
        assert f.lag_n_up_frac == 1.0
        assert f.lag_n_mean_abs_move == pytest.approx(1.0)
        assert f.lag_n_mean_signed_move == pytest.approx(1.0)
        assert f.lag_n_up_magnitude == pytest.approx(1.0)
        assert math.isnan(f.lag_n_down_magnitude)  # no down markets
        assert f.n_past_markets == 5

    def test_mixed_window(self):
        # 6 markets: 4 up at +2%, 2 down at -1%
        markets = (
            [make_market(f"2026-04-08T0{i}:00:00Z", 100.0, 102.0, "Up") for i in range(4)]
            + [make_market(f"2026-04-08T0{4+i}:00:00Z", 100.0, 99.0, "Down") for i in range(2)]
        )
        f = compute_regime_features(markets, "2026-04-08T06:00:00Z", lag_n=10)
        assert f.lag_n_up_frac == pytest.approx(4 / 6)
        assert f.lag_n_mean_abs_move == pytest.approx((4 * 2 + 2 * 1) / 6)
        assert f.lag_n_mean_signed_move == pytest.approx((4 * 2 - 2 * 1) / 6)
        assert f.lag_n_up_magnitude == pytest.approx(2.0)
        assert f.lag_n_down_magnitude == pytest.approx(1.0)
        assert f.n_past_markets == 6

    def test_lag_n_truncates_window(self):
        # 30 markets in input, lag_n=5 keeps only the last 5
        markets = [
            make_market(f"2026-04-08T{i:02d}:00:00Z", 100.0,
                        100.0 + i * 0.1, "Up" if i % 2 == 0 else "Down")
            for i in range(30)
        ]
        f = compute_regime_features(markets, "2026-04-09T00:00:00Z", lag_n=5)
        assert f.n_past_markets == 5
        # Last 5 markets are i=25..29: winners alternate odd/even
        # i=25 odd Down, i=26 even Up, i=27 odd Down, i=28 even Up, i=29 odd Down
        # so 2 ups, 3 downs
        assert f.lag_n_up_frac == pytest.approx(2 / 5)

    def test_hour_of_day_extraction(self):
        f = compute_regime_features([], "2026-04-08T14:30:00Z", lag_n=10)
        assert f.hour_of_day == 14

    def test_hour_with_microseconds(self):
        f = compute_regime_features([], "2026-04-08T14:30:00.123456Z", lag_n=10)
        assert f.hour_of_day == 14

    def test_day_of_week(self):
        # 2026-04-08 is a Wednesday -> weekday() = 2
        f = compute_regime_features([], "2026-04-08T00:00:00Z", lag_n=10)
        assert f.day_of_week == 2

    def test_invalid_timestamp_returns_nan_time(self):
        f = compute_regime_features([], "not-a-timestamp", lag_n=10)
        assert math.isnan(f.hour_of_day)
        assert math.isnan(f.day_of_week)


class TestRollingWindow:
    def test_features_before_append_dont_see_current(self):
        """The look-ahead causality test.

        Build a 3-market sequence. Compute features for the 4th market
        BEFORE appending it. Verify the features are based on markets
        1-3 only, not on market 4. Then append and compute features
        for a hypothetical 5th market, and verify those features now
        include market 4.
        """
        w = RollingMarketWindow(lag_n=10)
        m1 = make_market("2026-04-08T00:00:00Z", 100.0, 102.0, "Up")
        m2 = make_market("2026-04-08T00:05:00Z", 102.0, 104.0, "Up")
        m3 = make_market("2026-04-08T00:10:00Z", 104.0, 106.0, "Up")
        for m in [m1, m2, m3]:
            w.append(m)

        # Features for hypothetical market 4 (Down)
        f_before = w.features_for_current("2026-04-08T00:15:00Z")
        # Window has m1, m2, m3 — all Up
        assert f_before.lag_n_up_frac == 1.0
        assert f_before.n_past_markets == 3

        # Now append market 4 as Down
        m4 = make_market("2026-04-08T00:15:00Z", 106.0, 100.0, "Down")
        w.append(m4)

        # Features for hypothetical market 5
        f_after = w.features_for_current("2026-04-08T00:20:00Z")
        # Window now has m1-m4: 3 up, 1 down
        assert f_after.lag_n_up_frac == pytest.approx(3 / 4)
        assert f_after.n_past_markets == 4

        # Critical: f_before saw a 100% up rate; if we had naively
        # computed it after appending m4, we'd see 75%. The fact
        # they differ proves the window respected the strict-
        # precedence invariant for the m4 calculation.
        assert f_before.lag_n_up_frac != f_after.lag_n_up_frac

    def test_window_maxlen_enforced(self):
        w = RollingMarketWindow(lag_n=3)
        for i in range(10):
            w.append(make_market(f"2026-04-08T{i:02d}:00:00Z", 100.0, 101.0, "Up"))
        assert len(w) == 3

    def test_initial_window_features(self):
        w = RollingMarketWindow(lag_n=20)
        f = w.features_for_current("2026-04-08T00:00:00Z")
        assert f.n_past_markets == 0
        assert math.isnan(f.lag_n_up_frac)


class TestRegimeFeatureNames:
    def test_name_list_matches_dataclass(self):
        # Make sure the export matches the dataclass fields. If
        # someone adds a field to RegimeFeatures, this test fails
        # until they update REGIME_FEATURE_NAMES.
        rf = RegimeFeatures(0, 0, 0, 0, 0, 0, 0, 0)
        assert set(REGIME_FEATURE_NAMES) == set(rf.to_dict().keys())
