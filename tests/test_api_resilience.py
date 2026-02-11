"""Tests for API error handling and resilience."""

from __future__ import annotations

from datetime import datetime
from unittest.mock import AsyncMock, MagicMock, patch

import httpx
import pytest

from clients.polymarket import PolymarketClient
from clients.weather import WeatherClient


class TestWeatherRetryOn429:
    @pytest.mark.asyncio
    async def test_retries_on_rate_limit(self) -> None:
        """429 should trigger retry with backoff, eventually succeed."""
        client = WeatherClient()

        # First call: 429, second call: success
        rate_limit_resp = httpx.Response(429, request=httpx.Request("GET", "http://test"))
        success_resp = MagicMock()
        success_resp.status_code = 200
        success_resp.raise_for_status = lambda: None
        success_resp.json.return_value = {
            "hourly": {
                "temperature_2m_member01": [70.0, 72.0, 75.0],
                "temperature_2m_member02": [68.0, 71.0, 73.0],
            }
        }

        call_count = 0

        async def mock_get(*args, **kwargs):
            nonlocal call_count
            call_count += 1
            if call_count == 1:
                raise httpx.HTTPStatusError("429", request=httpx.Request("GET", "http://test"), response=rate_limit_resp)
            return success_resp

        client._http = AsyncMock()
        client._http.get = mock_get

        with patch("asyncio.sleep", new_callable=AsyncMock):
            result = await client.get_ensemble_forecast("New York", datetime(2026, 2, 12))

        assert result is not None
        assert result.num_members == 2
        assert call_count == 2

    @pytest.mark.asyncio
    async def test_all_retries_exhausted(self) -> None:
        """All 3 retries fail → returns None gracefully."""
        client = WeatherClient()
        rate_limit_resp = httpx.Response(429, request=httpx.Request("GET", "http://test"))

        async def mock_get(*args, **kwargs):
            raise httpx.HTTPStatusError("429", request=httpx.Request("GET", "http://test"), response=rate_limit_resp)

        client._http = AsyncMock()
        client._http.get = mock_get

        with patch("asyncio.sleep", new_callable=AsyncMock):
            result = await client.get_ensemble_forecast("New York", datetime(2026, 2, 12))

        assert result is None


class TestServerError:
    @pytest.mark.asyncio
    async def test_500_returns_none(self) -> None:
        """500 error should not crash, returns None."""
        client = WeatherClient()
        resp_500 = httpx.Response(500, request=httpx.Request("GET", "http://test"))

        async def mock_get(*args, **kwargs):
            raise httpx.HTTPStatusError("500", request=httpx.Request("GET", "http://test"), response=resp_500)

        client._http = AsyncMock()
        client._http.get = mock_get

        result = await client.get_ensemble_forecast("New York", datetime(2026, 2, 12))
        assert result is None


class TestTimeout:
    @pytest.mark.asyncio
    async def test_timeout_returns_none(self) -> None:
        client = WeatherClient()

        async def mock_get(*args, **kwargs):
            raise httpx.TimeoutException("timeout")

        client._http = AsyncMock()
        client._http.get = mock_get

        result = await client.get_ensemble_forecast("New York", datetime(2026, 2, 12))
        assert result is None


class TestMalformedResponse:
    @pytest.mark.asyncio
    async def test_missing_hourly_key(self) -> None:
        """Malformed response without hourly data → None or empty forecast."""
        client = WeatherClient()

        mock_resp = MagicMock()
        mock_resp.raise_for_status = lambda: None
        mock_resp.json.return_value = {"not_hourly": {}}

        client._http = AsyncMock()
        client._http.get = AsyncMock(return_value=mock_resp)

        result = await client.get_ensemble_forecast("New York", datetime(2026, 2, 12))
        assert result is None  # No member temps → returns None


class TestGammaLeaderboard405:
    @pytest.mark.asyncio
    async def test_405_graceful_fallback(self) -> None:
        """Gamma leaderboard returning 405 should not crash copy trading."""
        from strategies.copy_trading import CopyTradingStrategy

        strategy = CopyTradingStrategy()
        resp_405 = httpx.Response(405, request=httpx.Request("GET", "http://test"))

        async def mock_get(*args, **kwargs):
            raise httpx.HTTPStatusError("405", request=httpx.Request("GET", "http://test"), response=resp_405)

        strategy._http = AsyncMock()
        strategy._http.get = mock_get

        # Should not raise
        await strategy.discover_whales()
        assert len(strategy.tracked_wallets) == 0  # No wallets discovered, but no crash


class TestPolymarketResolutionErrors:
    @pytest.mark.asyncio
    async def test_resolution_check_network_error(self) -> None:
        """Network error during resolution check should return None."""
        with patch.object(PolymarketClient, "__init__", lambda self, cfg: None):
            client = PolymarketClient.__new__(PolymarketClient)
            client._http = AsyncMock()
            client._http.get = AsyncMock(side_effect=Exception("network error"))
            client.gamma_url = "https://gamma-api.polymarket.com"

            result = await client.check_market_resolution("market-001")
            assert result is None
