"""Tests for persistence reliability: startup health check, write-ahead log, reconciliation.

These ensure we never silently lose trade data.
"""
from __future__ import annotations

import json
import time
from unittest.mock import MagicMock, patch

import pytest


class TestStartupHealthCheck:
    """Supabase connection and schema must be verified before trading starts."""

    def test_health_check_passes_with_valid_schema(self):
        from shared.supabase_client import SupabaseClient

        with patch.object(SupabaseClient, "__init__", lambda self, *a, **kw: None):
            client = SupabaseClient.__new__(SupabaseClient)
            client._http = MagicMock()
            mock_resp = MagicMock()
            mock_resp.status_code = 201
            mock_resp.raise_for_status = MagicMock()
            client._http.post.return_value = mock_resp

            delete_resp = MagicMock()
            delete_resp.status_code = 200
            client._http.delete.return_value = delete_resp

            result = client.health_check()
            assert result is True

    def test_health_check_fails_on_schema_mismatch(self):
        from shared.supabase_client import SupabaseClient

        with patch.object(SupabaseClient, "__init__", lambda self, *a, **kw: None):
            client = SupabaseClient.__new__(SupabaseClient)
            client._http = MagicMock()
            mock_resp = MagicMock()
            mock_resp.status_code = 400
            mock_resp.text = '{"message":"Could not find column btc_price"}'
            mock_resp.raise_for_status.side_effect = Exception("400 Bad Request")
            client._http.post.return_value = mock_resp

            result = client.health_check()
            assert result is False

    def test_health_check_fails_on_network_error(self):
        from shared.supabase_client import SupabaseClient

        with patch.object(SupabaseClient, "__init__", lambda self, *a, **kw: None):
            client = SupabaseClient.__new__(SupabaseClient)
            client._http = MagicMock()
            client._http.post.side_effect = ConnectionError("refused")

            result = client.health_check()
            assert result is False


class TestWriteAheadQueue:
    """Failed Supabase writes should be queued for retry."""

    def test_failed_write_is_queued(self):
        from shared.supabase_client import SupabaseClient

        with patch.object(SupabaseClient, "__init__", lambda self, *a, **kw: None):
            client = SupabaseClient.__new__(SupabaseClient)
            client._http = MagicMock()
            client._dlq = []
            mock_resp = MagicMock()
            mock_resp.status_code = 500
            mock_resp.raise_for_status.side_effect = Exception("500 Server Error")
            client._http.post.return_value = mock_resp

            row = {"id": "test-1", "coin": "btc", "direction": "UP"}
            client.upsert_trade_safe(row)

            assert len(client._dlq) == 1
            assert client._dlq[0]["id"] == "test-1"

    def test_successful_write_not_queued(self):
        from shared.supabase_client import SupabaseClient

        with patch.object(SupabaseClient, "__init__", lambda self, *a, **kw: None):
            client = SupabaseClient.__new__(SupabaseClient)
            client._http = MagicMock()
            client._dlq = []
            mock_resp = MagicMock()
            mock_resp.status_code = 201
            mock_resp.raise_for_status = MagicMock()
            client._http.post.return_value = mock_resp

            row = {"id": "test-1", "coin": "btc", "direction": "UP"}
            client.upsert_trade_safe(row)

            assert len(client._dlq) == 0

    def test_flush_dlq_retries_queued_writes(self):
        from shared.supabase_client import SupabaseClient

        with patch.object(SupabaseClient, "__init__", lambda self, *a, **kw: None):
            client = SupabaseClient.__new__(SupabaseClient)
            client._http = MagicMock()
            client._dlq = [
                {"id": "test-1", "coin": "btc"},
                {"id": "test-2", "coin": "eth"},
            ]
            mock_resp = MagicMock()
            mock_resp.status_code = 201
            mock_resp.raise_for_status = MagicMock()
            client._http.post.return_value = mock_resp

            flushed = client.flush_dlq()

            assert flushed == 2
            assert len(client._dlq) == 0

    def test_flush_dlq_keeps_still_failing(self):
        from shared.supabase_client import SupabaseClient

        with patch.object(SupabaseClient, "__init__", lambda self, *a, **kw: None):
            client = SupabaseClient.__new__(SupabaseClient)
            client._http = MagicMock()
            client._dlq = [
                {"id": "test-1", "coin": "btc"},
                {"id": "test-2", "coin": "eth"},
            ]
            mock_resp = MagicMock()
            mock_resp.status_code = 500
            mock_resp.raise_for_status.side_effect = Exception("still broken")
            client._http.post.return_value = mock_resp

            flushed = client.flush_dlq()

            assert flushed == 0
            assert len(client._dlq) == 2

    def test_dlq_has_max_size(self):
        """DLQ should not grow unbounded."""
        from shared.supabase_client import SupabaseClient

        with patch.object(SupabaseClient, "__init__", lambda self, *a, **kw: None):
            client = SupabaseClient.__new__(SupabaseClient)
            client._http = MagicMock()
            client._dlq = [{"id": f"test-{i}"} for i in range(1000)]
            mock_resp = MagicMock()
            mock_resp.status_code = 500
            mock_resp.raise_for_status.side_effect = Exception("broken")
            client._http.post.return_value = mock_resp

            client.upsert_trade_safe({"id": "test-overflow"})

            assert len(client._dlq) <= 500
