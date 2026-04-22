"""Tests for BTCTradingEngine._reconcile_live_balance."""

from __future__ import annotations

import asyncio
from dataclasses import dataclass
from unittest.mock import AsyncMock, MagicMock

import pytest

from core.engine import BTCTradingEngine


@dataclass
class _PaperCfg:
    enabled: bool
    starting_balance: float = 100.0


@dataclass
class _RootCfg:
    paper: _PaperCfg


def _make_engine_stub(
    paper: bool,
    polymarket: object | None,
    balance: float,
    open_positions: int,
    threshold: float = 2.0,
) -> BTCTradingEngine:
    eng = BTCTradingEngine.__new__(BTCTradingEngine)
    eng.cfg = _RootCfg(paper=_PaperCfg(enabled=paper))
    eng._polymarket = polymarket
    eng.balance = balance
    eng._paper_trades = [{"placeholder": i} for i in range(open_positions)]
    eng._reconcile_halt = False
    eng._drift_threshold_usd = threshold
    eng._live_usdc_anchor = None
    return eng


def _poly_with_balance(bal: float) -> MagicMock:
    client = MagicMock()
    client.get_balance_allowance = AsyncMock(
        return_value={"balance_usdc": bal, "allowance_usdc": bal}
    )
    return client


@pytest.mark.asyncio
async def test_noop_in_paper_mode():
    eng = _make_engine_stub(
        paper=True, polymarket=_poly_with_balance(1.0), balance=100.0, open_positions=0
    )
    await eng._reconcile_live_balance()
    assert eng._reconcile_halt is False


@pytest.mark.asyncio
async def test_noop_when_no_polymarket_client():
    eng = _make_engine_stub(
        paper=False, polymarket=None, balance=100.0, open_positions=0
    )
    await eng._reconcile_live_balance()
    assert eng._reconcile_halt is False


@pytest.mark.asyncio
async def test_noop_when_open_positions_exist():
    poly = _poly_with_balance(0.0)  # massive drift
    eng = _make_engine_stub(
        paper=False, polymarket=poly, balance=100.0, open_positions=1
    )
    await eng._reconcile_live_balance()
    assert eng._reconcile_halt is False
    poly.get_balance_allowance.assert_not_called()


@pytest.mark.asyncio
async def test_no_halt_under_threshold():
    eng = _make_engine_stub(
        paper=False,
        polymarket=_poly_with_balance(99.5),
        balance=100.0,
        open_positions=0,
        threshold=2.0,
    )
    eng._live_usdc_anchor = 100.0
    await eng._reconcile_live_balance()
    assert eng._reconcile_halt is False


@pytest.mark.asyncio
async def test_halts_on_drift_over_threshold():
    eng = _make_engine_stub(
        paper=False,
        polymarket=_poly_with_balance(95.0),
        balance=100.0,
        open_positions=0,
        threshold=2.0,
    )
    eng._live_usdc_anchor = 100.0
    await eng._reconcile_live_balance()
    assert eng._reconcile_halt is True


@pytest.mark.asyncio
async def test_halts_on_positive_drift():
    eng = _make_engine_stub(
        paper=False,
        polymarket=_poly_with_balance(105.0),
        balance=100.0,
        open_positions=0,
        threshold=2.0,
    )
    eng._live_usdc_anchor = 100.0
    await eng._reconcile_live_balance()
    assert eng._reconcile_halt is True


@pytest.mark.asyncio
async def test_rpc_failure_does_not_halt():
    client = MagicMock()
    client.get_balance_allowance = AsyncMock(side_effect=RuntimeError("rpc down"))
    eng = _make_engine_stub(
        paper=False, polymarket=client, balance=100.0, open_positions=0
    )
    await eng._reconcile_live_balance()
    assert eng._reconcile_halt is False
