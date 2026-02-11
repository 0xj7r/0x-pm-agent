"""Shared fixtures for the test suite."""

from __future__ import annotations

from datetime import datetime

import pytest

from config import Config
from models.market import Market, MarketCategory, OrderBook, PricePoint
from models.trade import Outcome, Side, Signal, SignalSource, Trade


@pytest.fixture
def config() -> Config:
    """A Config instance with defaults (no env overrides)."""
    return Config()


@pytest.fixture
def sample_market() -> Market:
    return Market(
        id="market-001",
        question="Will it rain tomorrow?",
        description="Resolves YES if rain is observed.",
        category=MarketCategory.WEATHER,
        end_date=datetime(2026, 3, 1),
        active=True,
        yes_token_id="yes-token-001",
        no_token_id="no-token-001",
        yes_price=0.65,
        no_price=0.35,
    )


@pytest.fixture
def sample_signal() -> Signal:
    return Signal(
        market_id="market-001",
        market_question="Will it rain tomorrow?",
        outcome=Outcome.YES,
        side=Side.BUY,
        source=SignalSource.WEATHER,
        fair_value=0.80,
        market_price=0.65,
        edge=0.15,
        confidence=0.7,
        reasoning="Test signal",
        yes_token_id="yes-token-001",
        no_token_id="no-token-001",
    )


@pytest.fixture
def sample_trade(sample_signal: Signal) -> Trade:
    return Trade(
        id="trade-001",
        signal=sample_signal,
        size_usd=5.0,
        price=0.65,
        outcome=Outcome.YES,
        side=Side.BUY,
        token_id="yes-token-001",
        executed=True,
        paper=True,
    )
