"""Tests for BTC Up/Down market window scanner."""
from __future__ import annotations

import json
from datetime import datetime, timezone
from unittest.mock import AsyncMock, patch

import pytest

from clients.market_scanner import MarketWindowScanner, parse_btc_market


def test_parse_btc_market_valid():
    raw = {
        "id": "market_123",
        "question": "Bitcoin Up or Down - March 31, 3:20AM-3:25AM ET",
        "description": "Will BTC go up between 3:20AM and 3:25AM ET?",
        "active": True,
        "closed": False,
        "endDate": "2026-03-31T07:25:00Z",
        "tokens": [
            {"outcome": "Up", "token_id": "tok_up_1", "price": "0.48"},
            {"outcome": "Down", "token_id": "tok_down_1", "price": "0.52"},
        ],
    }
    window = parse_btc_market(raw)
    assert window is not None
    assert window.market_id == "market_123"
    assert window.up_token_id == "tok_up_1"
    assert window.down_token_id == "tok_down_1"
    assert window.up_price == 0.48
    assert window.down_price == 0.52


def test_parse_btc_market_with_yes_no_outcomes():
    raw = {
        "id": "market_456",
        "question": "Bitcoin Up or Down - March 31, 3:20AM-3:25AM ET",
        "active": True,
        "closed": False,
        "endDate": "2026-03-31T07:25:00Z",
        "tokens": [
            {"outcome": "Yes", "token_id": "tok_yes", "price": "0.50"},
            {"outcome": "No", "token_id": "tok_no", "price": "0.50"},
        ],
    }
    window = parse_btc_market(raw)
    assert window is not None
    assert window.up_token_id == "tok_yes"
    assert window.down_token_id == "tok_no"


def test_parse_btc_market_not_btc():
    raw = {
        "id": "m1",
        "question": "Will it rain in NYC tomorrow?",
        "active": True,
        "closed": False,
        "endDate": "2026-04-01T00:00:00Z",
        "tokens": [],
    }
    assert parse_btc_market(raw) is None


def test_parse_btc_market_closed():
    raw = {
        "id": "m2",
        "question": "Bitcoin Up or Down - March 30",
        "active": False,
        "closed": True,
        "endDate": "2026-03-30T07:25:00Z",
        "tokens": [
            {"outcome": "Up", "token_id": "t1", "price": "1.0"},
            {"outcome": "Down", "token_id": "t2", "price": "0.0"},
        ],
    }
    assert parse_btc_market(raw) is None


@pytest.mark.asyncio
async def test_scanner_find_active_windows():
    mock_markets = [
        {
            "id": "m_active",
            "question": "Bitcoin Up or Down - March 31, 3:20AM-3:25AM ET",
            "active": True,
            "closed": False,
            "endDate": "2099-12-31T23:59:00Z",
            "tokens": [
                {"outcome": "Up", "token_id": "up1", "price": "0.50"},
                {"outcome": "Down", "token_id": "dn1", "price": "0.50"},
            ],
        },
        {
            "id": "m_closed",
            "question": "Bitcoin Up or Down - old",
            "active": False,
            "closed": True,
            "endDate": "2020-01-01T00:00:00Z",
            "tokens": [
                {"outcome": "Up", "token_id": "up2", "price": "1.0"},
                {"outcome": "Down", "token_id": "dn2", "price": "0.0"},
            ],
        },
    ]

    scanner = MarketWindowScanner(gamma_url="https://gamma-api.polymarket.com")

    with patch.object(scanner, "_fetch_btc_markets", return_value=mock_markets):
        windows = await scanner.find_active_windows()

    assert len(windows) == 1
    assert windows[0].market_id == "m_active"
