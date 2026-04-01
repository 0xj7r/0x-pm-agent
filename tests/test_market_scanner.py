"""Tests for BTC Up/Down 5-minute market window scanner."""
from __future__ import annotations

from datetime import datetime, timezone
from unittest.mock import AsyncMock, MagicMock, patch

import pytest

from clients.market_scanner import MarketWindowScanner, parse_btc_event, SLUG_PREFIX_5M


def _make_event(ts: int, active: bool = True, closed: bool = False) -> dict:
    return {
        "id": "330618",
        "slug": f"btc-updown-5m-{ts}",
        "title": f"Bitcoin Up or Down - Test {ts}",
        "active": active,
        "closed": closed,
        "markets": [{
            "id": "m1",
            "tokens": [
                {"outcome": "Up", "token_id": "tok_up", "price": "0.505"},
                {"outcome": "Down", "token_id": "tok_down", "price": "0.495"},
            ],
        }],
    }


def test_parse_valid_event():
    ts = int(datetime(2026, 4, 1, 12, 30, tzinfo=timezone.utc).timestamp())
    window = parse_btc_event(_make_event(ts))
    assert window is not None
    assert window.up_token_id == "tok_up"
    assert window.down_token_id == "tok_down"
    assert window.up_price == 0.505
    assert (window.end_time - window.start_time).total_seconds() == 300


def test_parse_rejects_non_5m_slug():
    event = {"id": "1", "slug": "bitcoin-up-or-down-april-1-8am-et",
             "title": "Bitcoin Up or Down", "active": True, "closed": False, "markets": []}
    assert parse_btc_event(event) is None


def test_parse_rejects_closed():
    ts = int(datetime(2026, 4, 1, 12, 30, tzinfo=timezone.utc).timestamp())
    assert parse_btc_event(_make_event(ts, active=False, closed=True)) is None


def test_start_time_from_slug():
    ts = 1775089800
    window = parse_btc_event(_make_event(ts))
    assert window is not None
    assert window.start_time == datetime.fromtimestamp(ts, tz=timezone.utc)


def test_generates_candidate_slugs():
    scanner = MarketWindowScanner()
    slugs = scanner._generate_candidate_slugs(count=6)
    assert len(slugs) == 7
    for s in slugs:
        assert s.startswith(SLUG_PREFIX_5M)


@pytest.mark.asyncio
async def test_find_active_windows():
    ts = int(datetime(2026, 4, 1, 12, 30, tzinfo=timezone.utc).timestamp())
    event = _make_event(ts)

    scanner = MarketWindowScanner()
    mock_resp = MagicMock()
    mock_resp.status_code = 200
    mock_resp.json.return_value = [event]
    scanner._http.get = AsyncMock(return_value=mock_resp)

    with patch.object(scanner, "_generate_candidate_slugs", return_value=[f"btc-updown-5m-{ts}"]):
        windows = await scanner.find_active_windows()

    assert len(windows) == 1
    assert windows[0].up_token_id == "tok_up"
