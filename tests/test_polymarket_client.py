"""Tests for clients.polymarket.PolymarketClient.

Mocks py_clob_client.client.ClobClient so no network is required. Focus is
on the CLOB-touching methods that were rewritten: credential derivation,
place_order, get_balance_allowance, get_order_status, set_allowances,
and verifying that the broken `_get_auth_headers` / `get_balance` helpers
are gone.
"""

from __future__ import annotations

from types import SimpleNamespace
from typing import Any
from unittest.mock import MagicMock, patch

import pytest

from clients.polymarket import PolymarketClient
from py_clob_client.clob_types import (
    ApiCreds,
    AssetType,
    BalanceAllowanceParams,
    OrderArgs,
    OrderType,
)


class _CfgLike:
    """Minimal config stand-in so we don't rely on env vars."""

    PRIVATE_KEY = "0x" + "a" * 64
    API_KEY = ""
    API_SECRET = ""
    API_PASSPHRASE = ""
    CLOB_URL = "https://clob.example"
    GAMMA_URL = "https://gamma.example"
    CHAIN_ID = 137


def _make_mock_clob() -> MagicMock:
    """A ClobClient mock with sensible defaults for the methods we call."""
    mock = MagicMock(name="ClobClient")
    mock.derive_api_key.return_value = ApiCreds(
        api_key="derived-key",
        api_secret="derived-secret",
        api_passphrase="derived-pass",
    )
    mock.create_api_key.return_value = ApiCreds(
        api_key="created-key",
        api_secret="created-secret",
        api_passphrase="created-pass",
    )
    mock.get_fee_rate_bps.return_value = 10
    mock.create_order.return_value = {"signed": True}
    mock.post_order.return_value = {"orderID": "abc123", "status": "matched"}
    mock.cancel.return_value = {"orderID": "abc123", "status": "cancelled"}
    mock.get_order.return_value = {"id": "abc123", "status": "live"}
    mock.get_balance_allowance.return_value = {
        "balance": "12500000",
        "allowance": "1000000000",
    }
    mock.update_balance_allowance.return_value = {"ok": True}
    return mock


@pytest.fixture
def mock_clob_class():
    """Patch ClobClient where clients.polymarket imports it.

    Note: the module imports ClobClient lazily inside _load_clob_dependencies,
    so we patch the symbol on py_clob_client.client itself — that's where
    the `from py_clob_client.client import ClobClient` resolves to.
    """
    mock_instance = _make_mock_clob()
    mock_class = MagicMock(return_value=mock_instance)
    with patch("py_clob_client.client.ClobClient", mock_class):
        yield mock_class, mock_instance


@pytest.fixture
def client(mock_clob_class) -> PolymarketClient:
    _, _ = mock_clob_class
    return PolymarketClient(_CfgLike())


# __init__ / credential derivation

def test_init_prefers_derive_api_key(mock_clob_class):
    mock_class, mock_instance = mock_clob_class
    PolymarketClient(_CfgLike())

    mock_instance.derive_api_key.assert_called_once()
    mock_instance.create_api_key.assert_not_called()
    mock_instance.set_api_creds.assert_called_once()
    set_arg = mock_instance.set_api_creds.call_args.args[0]
    assert set_arg.api_key == "derived-key"


def test_init_falls_back_to_create_when_derive_raises(mock_clob_class):
    _, mock_instance = mock_clob_class
    mock_instance.derive_api_key.side_effect = RuntimeError("no creds yet")

    PolymarketClient(_CfgLike())

    mock_instance.derive_api_key.assert_called_once()
    mock_instance.create_api_key.assert_called_once()
    set_arg = mock_instance.set_api_creds.call_args.args[0]
    assert set_arg.api_key == "created-key"


def test_init_falls_back_to_create_when_derive_returns_none(mock_clob_class):
    _, mock_instance = mock_clob_class
    mock_instance.derive_api_key.return_value = None

    PolymarketClient(_CfgLike())

    mock_instance.create_api_key.assert_called_once()


def test_init_readonly_when_no_private_key(mock_clob_class):
    mock_class, mock_instance = mock_clob_class

    class _NoKey(_CfgLike):
        PRIVATE_KEY = ""

    PolymarketClient(_NoKey())

    mock_instance.derive_api_key.assert_not_called()
    mock_instance.create_api_key.assert_not_called()
    mock_instance.set_api_creds.assert_not_called()


# place_order

@pytest.mark.asyncio
async def test_place_order_uses_market_fee_when_none_supplied(client, mock_clob_class):
    _, mock_instance = mock_clob_class

    result = await client.place_order(
        token_id="tok-1",
        side="BUY",
        price=0.42,
        size=10.0,
    )

    mock_instance.get_fee_rate_bps.assert_called_once_with("tok-1")
    mock_instance.create_order.assert_called_once()
    args = mock_instance.create_order.call_args.args[0]
    assert isinstance(args, OrderArgs)
    assert args.token_id == "tok-1"
    assert args.side == "BUY"
    assert args.price == 0.42
    assert args.size == 10.0
    assert args.fee_rate_bps == 10

    mock_instance.post_order.assert_called_once_with(
        {"signed": True}, OrderType.GTC
    )
    assert result == {"orderID": "abc123", "status": "matched"}


@pytest.mark.asyncio
async def test_place_order_respects_explicit_fee(client, mock_clob_class):
    _, mock_instance = mock_clob_class

    await client.place_order(
        token_id="tok-2",
        side="SELL",
        price=0.6,
        size=5.0,
        fee_rate_bps=0,
    )

    mock_instance.get_fee_rate_bps.assert_not_called()
    args = mock_instance.create_order.call_args.args[0]
    assert args.fee_rate_bps == 0


# cancel_order & get_order_status

@pytest.mark.asyncio
async def test_cancel_order_delegates_to_clob_cancel(client, mock_clob_class):
    _, mock_instance = mock_clob_class
    result = await client.cancel_order("abc123")
    mock_instance.cancel.assert_called_once_with("abc123")
    assert result == {"orderID": "abc123", "status": "cancelled"}


@pytest.mark.asyncio
async def test_get_order_status_wraps_get_order(client, mock_clob_class):
    _, mock_instance = mock_clob_class
    result = await client.get_order_status("abc123")
    mock_instance.get_order.assert_called_once_with("abc123")
    assert result == {"id": "abc123", "status": "live"}


# get_balance_allowance

@pytest.mark.asyncio
async def test_get_balance_allowance_parses_usdc_base_units(client, mock_clob_class):
    _, mock_instance = mock_clob_class

    result = await client.get_balance_allowance()

    mock_instance.get_balance_allowance.assert_called_once()
    params = mock_instance.get_balance_allowance.call_args.args[0]
    assert isinstance(params, BalanceAllowanceParams)
    assert params.asset_type == AssetType.COLLATERAL
    assert params.token_id is None

    # 12500000 / 1e6 = 12.5, 1000000000 / 1e6 = 1000.0
    assert result == {"balance_usdc": 12.5, "allowance_usdc": 1000.0}


@pytest.mark.asyncio
async def test_get_balance_allowance_conditional_requires_token(client, mock_clob_class):
    _, mock_instance = mock_clob_class
    mock_instance.get_balance_allowance.return_value = {
        "balance": "0",
        "allowance": "0",
    }

    await client.get_balance_allowance(
        asset_type=AssetType.CONDITIONAL, token_id="tok-xyz"
    )
    params = mock_instance.get_balance_allowance.call_args.args[0]
    assert params.asset_type == AssetType.CONDITIONAL
    assert params.token_id == "tok-xyz"


# set_allowances

@pytest.mark.asyncio
async def test_set_allowances_calls_update_balance_allowance(client, mock_clob_class):
    _, mock_instance = mock_clob_class

    result = await client.set_allowances()

    mock_instance.update_balance_allowance.assert_called_once()
    params = mock_instance.update_balance_allowance.call_args.args[0]
    assert isinstance(params, BalanceAllowanceParams)
    assert params.asset_type == AssetType.COLLATERAL
    assert result == {"ok": True}


# Deleted methods / attributes

def test_get_auth_headers_is_gone(client):
    assert not hasattr(client, "_get_auth_headers")


def test_get_balance_is_gone(client):
    assert not hasattr(client, "get_balance")
