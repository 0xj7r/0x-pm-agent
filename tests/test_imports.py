"""Test that every module imports without errors."""

from __future__ import annotations


def test_import_models_market():
    import models.market  # noqa: F401


def test_import_models_trade():
    import models.trade  # noqa: F401


def test_import_config():
    import config  # noqa: F401


def test_import_core_engine():
    import core.engine  # noqa: F401


def test_import_core_portfolio():
    import core.portfolio  # noqa: F401


def test_import_core_risk():
    import core.risk  # noqa: F401


def test_import_core_memory():
    import core.memory  # noqa: F401


def test_import_core_analysis():
    import core.analysis  # noqa: F401


def test_import_clients_polymarket():
    import clients.polymarket  # noqa: F401


def test_import_clients_weather():
    import clients.weather  # noqa: F401


def test_import_clients_claude_client():
    import clients.claude_client  # noqa: F401


def test_import_strategies_weather():
    import strategies.weather  # noqa: F401


def test_import_strategies_arbitrage():
    import strategies.arbitrage  # noqa: F401


def test_import_strategies_copy_trading():
    import strategies.copy_trading  # noqa: F401


def test_import_main():
    import main  # noqa: F401
