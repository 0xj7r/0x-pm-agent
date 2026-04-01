"""Tests for the Bayesian signal engine."""
from __future__ import annotations

import pytest

from strategies.btc_sniper import BayesianSignalEngine
from strategies.strategy_config import SignalConfig


def make_engine(
    w1: float = 0.3,
    w2: float = 0.2,
    w3: float = 0.4,
    w4: float = 0.1,
    threshold: float = 0.85,
) -> BayesianSignalEngine:
    cfg = SignalConfig(
        w1_order_flow=w1,
        w2_microprice=w2,
        w3_price_delta=w3,
        w4_acceleration=w4,
        confidence_threshold=threshold,
    )
    return BayesianSignalEngine(cfg)


def test_initial_state():
    engine = make_engine()
    assert engine.p_up == 0.5
    assert engine.log_odds == 0.0
    assert engine.direction is None
    assert engine.confident is False


def test_strong_up_signal():
    engine = make_engine(w3=1.0, w1=0.0, w2=0.0, w4=0.0, threshold=0.80)
    engine.update(order_flow_imbalance=0.0, microprice_deviation=0.0,
                  price_delta=2.0, acceleration=0.0)
    assert engine.p_up > 0.80
    assert engine.direction == "UP"
    assert engine.confident is True


def test_strong_down_signal():
    engine = make_engine(w3=1.0, w1=0.0, w2=0.0, w4=0.0, threshold=0.80)
    engine.update(order_flow_imbalance=0.0, microprice_deviation=0.0,
                  price_delta=-2.0, acceleration=0.0)
    assert engine.p_up < 0.20
    assert engine.direction == "DOWN"
    assert engine.confident is True


def test_set_state_replaces_not_accumulates():
    engine = make_engine(w3=1.0, w1=0.0, w2=0.0, w4=0.0, threshold=0.90)
    engine.update(0.0, 0.0, 5.0, 0.0)  # p_up very high
    engine.update(0.0, 0.0, 0.1, 0.0)  # back to low delta
    # set_state replaces, so log_odds = 1.0 * 0.1 = 0.1, p_up ≈ 0.525
    assert engine.p_up < 0.60


def test_reset():
    engine = make_engine()
    engine.update(0.0, 0.0, 5.0, 0.0)
    assert engine.p_up != 0.5
    engine.reset()
    assert engine.p_up == 0.5
    assert engine.log_odds == 0.0


def test_order_flow_signal():
    engine = make_engine(w1=1.0, w2=0.0, w3=0.0, w4=0.0, threshold=0.70)
    engine.update(order_flow_imbalance=1.5, microprice_deviation=0.0,
                  price_delta=0.0, acceleration=0.0)
    assert engine.p_up > 0.70


def test_mixed_signals_cancel():
    engine = make_engine(w1=0.5, w3=0.5, w2=0.0, w4=0.0, threshold=0.90)
    engine.update(order_flow_imbalance=1.0, microprice_deviation=0.0,
                  price_delta=-1.0, acceleration=0.0)
    assert abs(engine.p_up - 0.5) < 0.01
    assert engine.confident is False
