"""Tests for the critical failures observed in production.

Each test reproduces a real failure mode and verifies the fix.
"""
from __future__ import annotations

import time
from datetime import UTC, datetime, timedelta
from unittest.mock import AsyncMock, MagicMock, patch

import pytest

from config import Config
from core.risk import RiskManager


def _make_risk() -> RiskManager:
    cfg = Config()
    cfg.MAX_POSITION_PCT = 0.06
    cfg.MAX_POSITION_USD = 50.0
    cfg.MIN_EDGE_THRESHOLD = 0.15
    cfg.KILL_BALANCE_USD = 5.0
    cfg.DAILY_LOSS_LIMIT_PCT = 0.20
    cfg.MAX_CONCURRENT_POSITIONS = 10
    cfg.LOSS_COOLDOWN_TRADES = 3
    cfg.LOSS_COOLDOWN_SECONDS = 1800
    rm = RiskManager(cfg)
    rm.set_bankroll(100.0)
    return rm


class TestCircuitBreakerDailyReset:
    """The drawdown breaker should reset daily, not permanently halt the bot.

    Root cause observed: 5 of 6 containers halted because peak balance from yesterday
    kept triggering the breaker even after market conditions changed.
    """

    def test_breaker_resets_on_new_day(self):
        rm = _make_risk()
        rm.set_peak_balance(200.0)
        assert rm.is_drawdown_breaker_tripped(100.0) is True

        rm._peak_reset_date = (datetime.now(UTC) - timedelta(days=1)).strftime("%Y-%m-%d")
        rm.update_peak_balance(100.0)

        assert rm._peak_balance == 100.0, "Peak should reset to current balance on new day"
        assert rm.is_drawdown_breaker_tripped(100.0) is False, "Breaker should not trip right after daily reset"

    def test_breaker_threshold_is_looser(self):
        """25% is too tight. Real quants use 30-40%."""
        rm = _make_risk()
        rm.set_peak_balance(200.0)

        assert rm.is_drawdown_breaker_tripped(155.0) is False, (
            "22.5% drawdown should NOT trip with relaxed threshold"
        )
        assert rm.is_drawdown_breaker_tripped(130.0) is False, (
            "35% drawdown should NOT trip (below new 40% threshold)"
        )
        assert rm.is_drawdown_breaker_tripped(115.0) is True, (
            "42.5% drawdown should trip"
        )

    def test_breaker_auto_reset_after_hours(self):
        """After N hours of being tripped, reset automatically.

        This is the escape hatch for when the breaker trips on noise.
        """
        rm = _make_risk()
        rm.set_peak_balance(200.0)
        assert rm.is_drawdown_breaker_tripped(110.0) is True

        rm._breaker_tripped_at = time.time() - 7 * 3600

        assert rm.is_drawdown_breaker_tripped(110.0) is False, (
            "Breaker should auto-reset after 6+ hours"
        )


class TestNotifierHandlesNone:
    """notify_trade and notify_status crash when p_win or p_up is None.

    Root cause observed: every hour this error fires:
      'unsupported format string passed to NoneType.__format__'
    """

    @pytest.mark.asyncio
    async def test_notify_trade_accepts_none_p_win(self):
        from core.notifier import SlackNotifier

        notifier = SlackNotifier()
        notifier._http = AsyncMock()
        notifier._url = "https://hooks.slack.com/services/test"
        mock_resp = MagicMock()
        mock_resp.status_code = 200
        notifier._http.post = AsyncMock(return_value=mock_resp)

        await notifier.notify_trade(
            direction="UP",
            token_price=0.50,
            size_usd=10.0,
            shares=20.0,
            p_win=None,
            btc_price=67000.0,
            balance=100.0,
        )

        notifier._http.post.assert_called_once()

    @pytest.mark.asyncio
    async def test_notify_status_accepts_none_p_up(self):
        from core.notifier import SlackNotifier

        notifier = SlackNotifier()
        notifier._http = AsyncMock()
        notifier._url = "https://hooks.slack.com/services/test"
        mock_resp = MagicMock()
        mock_resp.status_code = 200
        notifier._http.post = AsyncMock(return_value=mock_resp)

        await notifier.notify_status(
            balance=100.0,
            trades=10,
            resolved=5,
            p_up=None,
            uptime_hours=3.5,
        )

        notifier._http.post.assert_called_once()


class TestPIDLockWorksInContainers:
    """The PID lock should allow restart when the only 'running' process is PID 1 (self).

    Root cause observed: in Docker containers, the Python process is PID 1.
    On restart, the new process is also PID 1, and the old .pid file has '1'.
    os.kill(1, 0) always succeeds, so the lock refused the restart.
    """

    def test_stale_pid_from_same_pid_is_cleared(self, tmp_path, monkeypatch):
        import os
        monkeypatch.chdir(tmp_path)

        pid_file = tmp_path / "btc_agent.pid"
        pid_file.write_text(str(os.getpid()))

        import main
        main.write_pid()

        assert pid_file.exists()
        assert pid_file.read_text().strip() == str(os.getpid())

    def test_pid_lock_blocks_different_live_pid(self, tmp_path, monkeypatch):
        import os
        monkeypatch.chdir(tmp_path)

        parent_pid = os.getppid()
        if parent_pid == os.getpid():
            pytest.skip("No distinct parent pid")

        pid_file = tmp_path / "btc_agent.pid"
        pid_file.write_text(str(parent_pid))

        import main
        with pytest.raises(SystemExit):
            main.write_pid()
