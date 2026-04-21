"""Tests for CTFRedeemer.sweep_wallet (2026-04 fix 3).

``sweep_wallet`` walks a list of ``{condition_id, token_id}`` dicts,
queries the CTF's ERC-1155 ``balanceOf(owner, id)`` for each, and calls
``redeemPositions`` for any position with non-zero balance. Zero-balance
positions are skipped (already redeemed). Errors on a single position
do not abort the sweep.

These tests mock the web3 contract + ``redeem()`` method so no RPC
traffic occurs.
"""

from __future__ import annotations

from unittest.mock import AsyncMock, MagicMock, patch

import pytest

from clients.ctf_redeemer import CTFRedeemer


USDC_E = "0x2791Bca1F2de4661ED88A30C99A7a9449Aa84174"
CTF_ADDR = "0x4D97DCd97eC945f40cF65F87097ACe5EA0476045"
FAKE_KEY = "0x" + "11" * 32
COND_A = "0x" + "aa" * 32
COND_B = "0x" + "bb" * 32
COND_C = "0x" + "cc" * 32
TOKEN_A = "11111111111111111111"
TOKEN_B = "22222222222222222222"
TOKEN_C = "33333333333333333333"


def _make_real_redeemer() -> CTFRedeemer:
    return CTFRedeemer(
        web3_provider_url="https://polygon-rpc.invalid",
        private_key=FAKE_KEY,
        ctf_address=CTF_ADDR,
        collateral_token_address=USDC_E,
        chain_id=137,
    )


def _patch_balances(r: CTFRedeemer, balances: dict[int, int]):
    """Wire ``_contract.functions.balanceOf(owner, id).call()`` to the
    given token_id -> balance map."""
    def _balanceOf(owner, token_id):
        fn = MagicMock()
        fn.call.return_value = balances.get(int(token_id), 0)
        return fn
    fake_functions = MagicMock()
    fake_functions.balanceOf.side_effect = _balanceOf
    mock_contract = MagicMock()
    mock_contract.functions = fake_functions
    return patch.object(r, "_contract", mock_contract)


@pytest.mark.asyncio
async def test_sweep_redeems_two_positions_with_nonzero_balance():
    """Core deliverable: two positions held, both are redeemed; one
    already-redeemed position is skipped without calling redeem()."""
    r = _make_real_redeemer()

    balances = {
        int(TOKEN_A): 500_000_000,   # non-zero → redeem
        int(TOKEN_B): 0,             # already redeemed → skip
        int(TOKEN_C): 1_000_000_000, # non-zero → redeem
    }

    fake_redeem = AsyncMock(side_effect=[
        {"tx_hash": "0xdead1", "status": "success", "gas_used": 120_000},
        {"tx_hash": "0xdead3", "status": "success", "gas_used": 140_000},
    ])

    with _patch_balances(r, balances), \
         patch.object(r, "redeem", fake_redeem):
        results = await r.sweep_wallet([
            {"condition_id": COND_A, "token_id": TOKEN_A},
            {"condition_id": COND_B, "token_id": TOKEN_B},
            {"condition_id": COND_C, "token_id": TOKEN_C},
        ])

    assert len(results) == 3
    # Two successes + one skipped (ordering matches input).
    assert results[0]["status"] == "success"
    assert results[0]["condition_id"] == COND_A
    assert results[0]["tx_hash"] == "0xdead1"
    assert results[0]["gas_used"] == 120_000

    assert results[1]["status"] == "skipped"
    assert results[1]["condition_id"] == COND_B
    assert "zero balance" in results[1]["reason"]

    assert results[2]["status"] == "success"
    assert results[2]["condition_id"] == COND_C
    assert results[2]["tx_hash"] == "0xdead3"
    assert results[2]["gas_used"] == 140_000

    # redeem called exactly twice, for A and C (not B).
    assert fake_redeem.await_count == 2
    called_conditions = [call.args[0] for call in fake_redeem.await_args_list]
    assert called_conditions == [COND_A, COND_C]


@pytest.mark.asyncio
async def test_sweep_deduplicates_same_condition_id():
    """Same condition_id twice (different token_ids for yes/no outcomes)
    should only redeem once — redeemPositions burns all winning
    partitions in a single call."""
    r = _make_real_redeemer()
    balances = {int(TOKEN_A): 100, int(TOKEN_B): 200}
    fake_redeem = AsyncMock(return_value={
        "tx_hash": "0xaaa", "status": "success", "gas_used": 100_000,
    })
    with _patch_balances(r, balances), \
         patch.object(r, "redeem", fake_redeem):
        results = await r.sweep_wallet([
            {"condition_id": COND_A, "token_id": TOKEN_A},
            {"condition_id": COND_A, "token_id": TOKEN_B},  # dup cond_id
        ])

    assert len(results) == 1
    assert results[0]["condition_id"] == COND_A
    assert fake_redeem.await_count == 1


@pytest.mark.asyncio
async def test_sweep_survives_balanceOf_error():
    """A balanceOf failure on one position is reported but does not
    prevent later positions from being probed."""
    r = _make_real_redeemer()

    def _balanceOf(owner, token_id):
        fn = MagicMock()
        if int(token_id) == int(TOKEN_A):
            fn.call.side_effect = RuntimeError("rpc down")
        else:
            fn.call.return_value = 42
        return fn

    fake_functions = MagicMock()
    fake_functions.balanceOf.side_effect = _balanceOf
    mock_contract = MagicMock()
    mock_contract.functions = fake_functions

    fake_redeem = AsyncMock(return_value={
        "tx_hash": "0xok", "status": "success", "gas_used": 100_000,
    })

    with patch.object(r, "_contract", mock_contract), \
         patch.object(r, "redeem", fake_redeem):
        results = await r.sweep_wallet([
            {"condition_id": COND_A, "token_id": TOKEN_A},
            {"condition_id": COND_B, "token_id": TOKEN_B},
        ])

    assert results[0]["status"] == "error"
    assert "balanceOf failed" in results[0]["reason"]
    assert results[1]["status"] == "success"
    fake_redeem.assert_awaited_once()


@pytest.mark.asyncio
async def test_sweep_redeem_failure_recorded_per_position():
    """If redeem() returns failed/timeout, the sweep records that status
    rather than aborting."""
    r = _make_real_redeemer()
    balances = {int(TOKEN_A): 100, int(TOKEN_B): 100}
    fake_redeem = AsyncMock(side_effect=[
        {"tx_hash": "0x1", "status": "failed", "gas_used": 50_000},
        {"tx_hash": "0x2", "status": "success", "gas_used": 80_000},
    ])
    with _patch_balances(r, balances), \
         patch.object(r, "redeem", fake_redeem):
        results = await r.sweep_wallet([
            {"condition_id": COND_A, "token_id": TOKEN_A},
            {"condition_id": COND_B, "token_id": TOKEN_B},
        ])

    statuses = [r["status"] for r in results]
    assert statuses == ["failed", "success"]


@pytest.mark.asyncio
async def test_sweep_blind_redeems_when_token_id_missing():
    """No token_id to probe: fire a blind redeemPositions (idempotent).

    Regression guard for issue #7: resolver recorded a win in event_log
    but token_id was not persisted, leaving the position unredeemed.
    redeemPositions is idempotent (empty position costs ~30k gas), so
    we prefer a blind call over skipping.
    """
    r = _make_real_redeemer()
    fake_redeem = AsyncMock(return_value={
        "tx_hash": "0xblind", "status": "success", "gas_used": 30_000,
    })
    balances = {}
    with _patch_balances(r, balances), \
         patch.object(r, "redeem", fake_redeem):
        results = await r.sweep_wallet([
            {"condition_id": COND_A},  # no token_id
        ])
    assert len(results) == 1
    assert results[0]["status"] == "success"
    assert results[0]["condition_id"] == COND_A
    assert results[0]["tx_hash"] == "0xblind"
    assert results[0]["gas_used"] == 30_000
    assert "blind redeem" in results[0]["reason"]
    fake_redeem.assert_awaited_once()
    assert fake_redeem.await_args.args[0] == COND_A


@pytest.mark.asyncio
async def test_sweep_blind_redeem_failure_is_recorded():
    """If blind redeem raises, record error and do not abort the sweep."""
    r = _make_real_redeemer()
    fake_redeem = AsyncMock(side_effect=RuntimeError("rpc down"))
    balances = {}
    with _patch_balances(r, balances), \
         patch.object(r, "redeem", fake_redeem):
        results = await r.sweep_wallet([
            {"condition_id": COND_A},  # no token_id
        ])
    assert len(results) == 1
    assert results[0]["status"] == "error"
    assert "blind redeem raised" in results[0]["reason"]


@pytest.mark.asyncio
async def test_sweep_empty_input_is_noop():
    r = _make_real_redeemer()
    out = await r.sweep_wallet([])
    assert out == []
