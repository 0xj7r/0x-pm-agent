"""Tests for BTC Up/Down 5-minute market window scanner."""
from __future__ import annotations

from datetime import datetime, timedelta, timezone
from unittest.mock import AsyncMock, MagicMock, patch

import pytest

from clients.market_scanner import MarketWindowScanner, parse_btc_event, SLUG_PREFIX_5M
from models.market import MarketWindow


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
    assert window.price_to_beat is None


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


def test_parse_price_to_beat_from_page_html():
    scanner = MarketWindowScanner()
    window = MarketWindow(
        market_id="m-current",
        question="Current",
        start_time=datetime(2026, 4, 22, 8, 30, 0, tzinfo=timezone.utc),
        end_time=datetime(2026, 4, 22, 8, 35, 0, tzinfo=timezone.utc),
        up_token_id="tok_up",
        down_token_id="tok_down",
        slug="btc-updown-5m-1776846600",
    )
    html = (
        '<html><body><script id="__NEXT_DATA__" type="application/json">'
        '{"props":{"pageProps":{"dehydratedState":{"queries":['
        '{"queryKey":["crypto-prices","price","BTC","2026-04-22T08:30:00Z","fiveminute","2026-04-22T08:35:00Z"],'
        '"state":{"data":{"openPrice":77986.83150999999,"closePrice":null}}},'
        '{"state":{"data":[{"id":"402307","ticker":"btc-updown-5m-1776846600",'
        '"slug":"btc-updown-5m-1776846600","title":"Bitcoin Up or Down",'
        '"eventMetadata":{"priceToBeat":77986.83150999999}}]}}]}}}}'
        "</script></body></html>"
    )

    price = scanner._parse_price_to_beat_from_html(html, window)

    assert price == pytest.approx(77986.83150999999)


@pytest.mark.asyncio
async def test_get_current_window_prefetches_current_and_next_price_to_beat():
    now = datetime.now(timezone.utc)
    current_window = MarketWindow(
        market_id="m-current",
        question="Current",
        start_time=now.replace(second=0, microsecond=0) - timedelta(minutes=1),
        end_time=now + timedelta(minutes=4),
        up_token_id="tok_up",
        down_token_id="tok_down",
        slug="btc-updown-5m-1776846600",
    )
    next_window = MarketWindow(
        market_id="m-next",
        question="Next",
        start_time=now + timedelta(minutes=4),
        end_time=now + timedelta(minutes=9),
        up_token_id="tok_up_next",
        down_token_id="tok_down_next",
        slug="btc-updown-5m-1776846900",
    )

    scanner = MarketWindowScanner()
    scanner.find_active_windows = AsyncMock(return_value=[current_window, next_window])
    scanner._fetch_price_to_beat = AsyncMock(side_effect=[77986.83, 78010.12])

    out = await scanner.get_current_window()

    assert out is current_window
    assert current_window.price_to_beat == pytest.approx(77986.83)
    assert next_window.price_to_beat == pytest.approx(78010.12)
    assert scanner._fetch_price_to_beat.await_count == 2


@pytest.mark.asyncio
async def test_fetch_price_to_beat_from_past_results_uses_previous_close():
    scanner = MarketWindowScanner()
    window = MarketWindow(
        market_id="m-current",
        question="Current",
        start_time=datetime(2026, 4, 22, 9, 0, 0, tzinfo=timezone.utc),
        end_time=datetime(2026, 4, 22, 9, 5, 0, tzinfo=timezone.utc),
        up_token_id="tok_up",
        down_token_id="tok_down",
        slug="btc-updown-5m-1776848400",
    )
    mock_resp = MagicMock()
    mock_resp.status_code = 200
    mock_resp.json.return_value = {
        "status": "success",
        "data": {
            "results": [
                {
                    "startTime": "2026-04-22T08:55:00.000Z",
                    "endTime": "2026-04-22T09:00:00Z",
                    "openPrice": 78060.37719961446,
                    "closePrice": 78024.5748753997,
                    "outcome": "down",
                }
            ]
        },
    }
    scanner._http.get = AsyncMock(return_value=mock_resp)

    price = await scanner._fetch_price_to_beat_from_past_results(window)

    assert price == pytest.approx(78024.5748753997)


@pytest.mark.asyncio
async def test_fetch_price_to_beat_prefers_past_results_before_other_sources():
    scanner = MarketWindowScanner()
    window = MarketWindow(
        market_id="m-current",
        question="Current",
        start_time=datetime(2026, 4, 22, 9, 0, 0, tzinfo=timezone.utc),
        end_time=datetime(2026, 4, 22, 9, 5, 0, tzinfo=timezone.utc),
        up_token_id="tok_up",
        down_token_id="tok_down",
        slug="btc-updown-5m-1776848400",
    )
    scanner._fetch_price_to_beat_from_past_results = AsyncMock(return_value=78024.57)
    scanner._fetch_price_to_beat_from_chainlink = AsyncMock(return_value=99999.99)
    scanner._http.get = AsyncMock()

    price = await scanner._fetch_price_to_beat(window)

    assert price == pytest.approx(78024.57)
    scanner._fetch_price_to_beat_from_past_results.assert_awaited_once_with(window)
    scanner._fetch_price_to_beat_from_chainlink.assert_not_awaited()
    scanner._http.get.assert_not_called()
