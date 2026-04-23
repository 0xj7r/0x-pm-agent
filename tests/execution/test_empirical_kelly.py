from __future__ import annotations

import pytest

from core.kelly import (
    PriceBucketEstimate,
    bucket_for_price,
    empirical_kelly_size,
    full_kelly_fraction,
    smoothed_bucket_estimate,
)
from strategies.strategy_config import RiskConfig


def test_bucket_for_price():
    assert bucket_for_price(0.02) == "0-10¢"
    assert bucket_for_price(0.10) == "10-25¢"
    assert bucket_for_price(0.40) == "40-55¢"


def test_full_kelly_zero_without_edge():
    assert full_kelly_fraction(p_win=0.02, token_price=0.03) == 0.0


def test_empirical_kelly_sizes_positive_edge_with_caps():
    risk_cfg = RiskConfig(
        kelly_multiplier=0.25,
        max_position_usd=5.0,
        max_position_pct=0.05,
    )
    size = empirical_kelly_size(
        p_win=0.65,
        token_price=0.40,
        bankroll=100.0,
        risk_cfg=risk_cfg,
    )
    assert 0.0 < size <= 5.0


def test_empirical_kelly_respects_min_size_floor():
    risk_cfg = RiskConfig(
        kelly_multiplier=0.25,
        max_position_usd=5.0,
        max_position_pct=0.05,
        empirical_kelly_min_size_usd=1.0,
    )
    size = empirical_kelly_size(
        p_win=0.031,
        token_price=0.03,
        bankroll=100.0,
        risk_cfg=risk_cfg,
    )
    assert size == pytest.approx(0.0)


def test_smoothed_prior_edge_lifts_cold_start():
    """With zero-sample bucket and prior_edge=0.08, prior lifts p_win
    above token_price so Kelly produces a positive fraction."""
    est = PriceBucketEstimate(bucket="0-10¢", wins=0, losses=0, p_win=0.0)
    smoothed = smoothed_bucket_estimate(
        est, token_price=0.02, prior_weight=10.0, prior_edge=0.08
    )
    assert smoothed.p_win == pytest.approx(0.10)

    est_losses = PriceBucketEstimate(bucket="0-10¢", wins=0, losses=2, p_win=0.0)
    smoothed_l = smoothed_bucket_estimate(
        est_losses, token_price=0.02, prior_weight=10.0, prior_edge=0.08
    )
    # Prior still dominates after 2 losses with weight=10
    assert smoothed_l.p_win > 0.02

    est_many = PriceBucketEstimate(bucket="0-10¢", wins=3, losses=17, p_win=0.15)
    smoothed_m = smoothed_bucket_estimate(
        est_many, token_price=0.02, prior_weight=10.0, prior_edge=0.08
    )
    # With 20 samples + weight=10, empirical (0.15) mostly dominates
    assert abs(smoothed_m.p_win - 0.15) < 0.05
