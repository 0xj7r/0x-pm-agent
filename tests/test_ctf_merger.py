"""Tests for clients.ctf_merger.CTFMerger.

Mocks web3.Web3 and eth_account.Account so no real RPC or signing
happens. Covers calldata encoding, balanceOf(), base-unit scaling, and
the terminal states of merge_pair(): success, failed, timeout, skipped
(zero amount), and the proxy-delegated path.
"""

from __future__ import annotations

from unittest.mock import MagicMock, patch

import pytest
from web3 import Web3

from clients.ctf_merger import CTFMerger, _normalize_bytes32, _to_base_units


USDC_E = "0x2791Bca1F2de4661ED88A30C99A7a9449Aa84174"
CTF_ADDR = "0x4D97DCd97eC945f40cF65F87097ACe5EA0476045"
FAKE_KEY = "0x" + "22" * 32
FAKE_CONDITION = "0x" + "cd" * 32
FAKE_FUNDER = "0xa57189d5b2285A5E64083d3925687bDFCE01fC83"


def _make_eoa_merger() -> CTFMerger:
    return CTFMerger(
        web3_provider_url="https://polygon-rpc.invalid",
        private_key=FAKE_KEY,
        ctf_address=CTF_ADDR,
        collateral_token_address=USDC_E,
        chain_id=137,
    )


def _make_proxy_merger(funder: str = FAKE_FUNDER, sig_type: int = 1) -> CTFMerger:
    return CTFMerger(
        web3_provider_url="https://polygon-rpc.invalid",
        private_key=FAKE_KEY,
        ctf_address=CTF_ADDR,
        collateral_token_address=USDC_E,
        chain_id=137,
        signature_type=sig_type,
        funder_address=funder,
    )


def test_to_base_units_rounds_down():
    assert _to_base_units(0.0) == 0
    assert _to_base_units(-1.5) == 0
    # 6-decimal USDC scaling
    assert _to_base_units(1.0) == 1_000_000
    assert _to_base_units(2.5) == 2_500_000
    # Fractional that should round down, never up
    assert _to_base_units(0.9999999) == 999_999


def test_normalize_bytes32_accepts_hex_and_bytes():
    assert len(_normalize_bytes32("0x" + "11" * 32)) == 32
    raw = b"\x05" * 32
    assert _normalize_bytes32(raw) == raw
    with pytest.raises(ValueError):
        _normalize_bytes32("0xabcd")


def test_init_requires_private_key():
    with pytest.raises(ValueError):
        CTFMerger(
            web3_provider_url="https://polygon-rpc.invalid",
            private_key="",
            ctf_address=CTF_ADDR,
            collateral_token_address=USDC_E,
        )


def test_init_requires_provider_url():
    with pytest.raises(ValueError):
        CTFMerger(
            web3_provider_url="",
            private_key=FAKE_KEY,
            ctf_address=CTF_ADDR,
            collateral_token_address=USDC_E,
        )


def test_init_proxy_mode_requires_funder():
    with pytest.raises(ValueError, match="funder_address"):
        CTFMerger(
            web3_provider_url="https://polygon-rpc.invalid",
            private_key=FAKE_KEY,
            ctf_address=CTF_ADDR,
            collateral_token_address=USDC_E,
            signature_type=1,
            funder_address=None,
        )


def test_init_proxy_mode_position_owner_is_funder():
    m = _make_proxy_merger()
    assert m.funder_address == Web3.to_checksum_address(FAKE_FUNDER)
    assert m.position_owner == Web3.to_checksum_address(FAKE_FUNDER)
    assert m.address != m.position_owner


def test_init_eoa_mode_position_owner_is_signer():
    m = _make_eoa_merger()
    assert m.funder_address is None
    assert m.position_owner == m.address


def test_encode_merge_calldata_has_correct_selector():
    m = _make_eoa_merger()
    # mergePositions(address,bytes32,bytes32,uint256[],uint256). Selector
    # derived at test time so we don't lock in a stale hex value.
    from eth_utils import function_signature_to_4byte_selector
    expected_sel = function_signature_to_4byte_selector(
        "mergePositions(address,bytes32,bytes32,uint256[],uint256)"
    ).hex()
    data = m.encode_merge_calldata(
        FAKE_CONDITION, amount_base_units=1_000_000, partition=[1, 2]
    )
    assert data.lower().startswith("0x" + expected_sel), (
        f"unexpected selector: {data[:10]}"
    )


def test_encode_merge_calldata_contains_args():
    m = _make_eoa_merger()
    data = m.encode_merge_calldata(
        FAKE_CONDITION, amount_base_units=2_500_000, partition=[1, 2]
    ).lower()
    # collateral token address
    assert "2791bca1f2de4661ed88a30c99a7a9449aa84174" in data
    # conditionId
    assert "cd" * 32 in data
    # amount = 2_500_000 = 0x2625a0
    assert "2625a0" in data


def test_encode_proxy_merge_calldata_wraps_ctf_selector():
    m = _make_proxy_merger()
    outer = m.encode_proxy_merge_calldata(
        FAKE_CONDITION, amount_base_units=1_000_000, partition=[1, 2]
    ).lower()

    from eth_utils import function_signature_to_4byte_selector

    proxy_sel = function_signature_to_4byte_selector(
        "proxy((uint8,address,uint256,bytes)[])"
    ).hex()
    assert outer.startswith("0x" + proxy_sel)

    merge_sel = function_signature_to_4byte_selector(
        "mergePositions(address,bytes32,bytes32,uint256[],uint256)"
    ).hex()
    assert merge_sel in outer
    # CTF address is the per-call target; funder/proxy address does
    # NOT appear in the outer data (it's the tx.to).
    assert CTF_ADDR.lower()[2:] in outer


@pytest.mark.asyncio
async def test_get_position_balance_uses_funder_in_proxy_mode():
    m = _make_proxy_merger()
    fake_fn = MagicMock()
    fake_fn.call.return_value = 7_500_000
    fake_functions = MagicMock()
    fake_functions.balanceOf.return_value = fake_fn

    with patch.object(m, "_contract") as mock_contract:
        mock_contract.functions = fake_functions
        bal = await m.get_position_balance("12345")

    assert bal == 7_500_000
    fake_functions.balanceOf.assert_called_once_with(m.funder_address, 12345)
    # Not the signer EOA
    assert fake_functions.balanceOf.call_args.args[0] != m.address


@pytest.mark.asyncio
async def test_get_position_balance_uses_signer_in_eoa_mode():
    m = _make_eoa_merger()
    fake_fn = MagicMock()
    fake_fn.call.return_value = 10
    fake_functions = MagicMock()
    fake_functions.balanceOf.return_value = fake_fn

    with patch.object(m, "_contract") as mock_contract:
        mock_contract.functions = fake_functions
        bal = await m.get_position_balance("0x10")

    assert bal == 10
    fake_functions.balanceOf.assert_called_once_with(m.address, 16)


def _patch_merge_internals(
    m: CTFMerger,
    *,
    estimate_gas: int = 200_000,
    send_tx_hash: bytes = b"\x00" * 31 + b"\x03",
    receipt_status: int = 1,
    receipt_gas: int = 120_000,
    wait_raises: Exception | None = None,
):
    fake_call = MagicMock()
    fake_call.estimate_gas.return_value = estimate_gas
    fake_call.build_transaction.return_value = {
        "from": m.address,
        "to": m.ctf_address,
        "data": "0x",
        "nonce": 9,
        "gas": 300_000,
        "chainId": 137,
    }
    fake_functions = MagicMock()
    fake_functions.mergePositions.return_value = fake_call

    mock_contract = MagicMock()
    mock_contract.functions = fake_functions

    mock_w3 = MagicMock()
    mock_w3.eth.get_transaction_count.return_value = 9
    mock_w3.eth.send_raw_transaction.return_value = send_tx_hash
    if wait_raises is not None:
        mock_w3.eth.wait_for_transaction_receipt.side_effect = wait_raises
    else:
        mock_w3.eth.wait_for_transaction_receipt.return_value = {
            "status": receipt_status,
            "gasUsed": receipt_gas,
        }

    mock_signed = MagicMock()
    mock_signed.raw_transaction = b"\xfe\xed\xfa\xce"
    mock_account = MagicMock()
    mock_account.sign_transaction.return_value = mock_signed

    return mock_contract, mock_w3, mock_account, fake_call


@pytest.mark.asyncio
async def test_merge_pair_skips_on_zero_shares():
    m = _make_eoa_merger()
    # No patches: we must not touch web3 if we skip early.
    result = await m.merge_pair(FAKE_CONDITION, amount_shares=0.0)
    assert result["status"] == "skipped"
    assert result["amount_base_units"] == 0
    assert result["tx_hash"] == ""


@pytest.mark.asyncio
async def test_merge_pair_skips_when_rounding_drops_to_zero():
    m = _make_eoa_merger()
    # 1e-9 shares rounds to zero 6-decimal base units
    result = await m.merge_pair(FAKE_CONDITION, amount_shares=1e-9)
    assert result["status"] == "skipped"
    assert result["amount_base_units"] == 0


@pytest.mark.asyncio
async def test_merge_pair_eoa_success():
    m = _make_eoa_merger()
    mock_contract, mock_w3, mock_account, fake_call = _patch_merge_internals(m)
    with patch.object(m, "_contract", mock_contract), \
         patch.object(m, "_w3", mock_w3), \
         patch.object(m, "_account", mock_account):
        result = await m.merge_pair(FAKE_CONDITION, amount_shares=1.5)

    assert result["status"] == "success"
    assert result["gas_used"] == 120_000
    assert result["amount_base_units"] == 1_500_000
    assert result["tx_hash"].startswith("0x")

    # mergePositions(collateral, parent, conditionId, partition, amount)
    args, _ = mock_contract.functions.mergePositions.call_args
    assert args[0] == m._collateral
    assert args[1] == b"\x00" * 32
    assert args[2] == _normalize_bytes32(FAKE_CONDITION)
    assert args[3] == [1, 2]
    assert args[4] == 1_500_000

    # Gas buffered 20%
    tx_kwargs = fake_call.build_transaction.call_args.args[0]
    assert tx_kwargs["gas"] == 240_000
    assert tx_kwargs["from"] == m.address
    assert tx_kwargs["chainId"] == 137


@pytest.mark.asyncio
async def test_merge_pair_failed_when_receipt_status_zero():
    m = _make_eoa_merger()
    mock_contract, mock_w3, mock_account, _ = _patch_merge_internals(
        m, receipt_status=0
    )
    with patch.object(m, "_contract", mock_contract), \
         patch.object(m, "_w3", mock_w3), \
         patch.object(m, "_account", mock_account):
        result = await m.merge_pair(FAKE_CONDITION, amount_shares=1.0)
    assert result["status"] == "failed"
    assert result["tx_hash"].startswith("0x")


@pytest.mark.asyncio
async def test_merge_pair_timeout_when_wait_raises():
    m = _make_eoa_merger()
    mock_contract, mock_w3, mock_account, _ = _patch_merge_internals(
        m, wait_raises=TimeoutError("boom")
    )
    with patch.object(m, "_contract", mock_contract), \
         patch.object(m, "_w3", mock_w3), \
         patch.object(m, "_account", mock_account):
        result = await m.merge_pair(FAKE_CONDITION, amount_shares=1.0)
    assert result["status"] == "timeout"
    assert result["gas_used"] == 0


@pytest.mark.asyncio
async def test_merge_pair_failed_when_send_raises():
    m = _make_eoa_merger()
    mock_contract, mock_w3, mock_account, _ = _patch_merge_internals(m)
    mock_w3.eth.send_raw_transaction.side_effect = RuntimeError("rpc down")
    with patch.object(m, "_contract", mock_contract), \
         patch.object(m, "_w3", mock_w3), \
         patch.object(m, "_account", mock_account):
        result = await m.merge_pair(FAKE_CONDITION, amount_shares=1.0)
    assert result["status"] == "failed"


@pytest.mark.asyncio
async def test_merge_pair_falls_back_to_300k_gas_on_estimate_failure():
    m = _make_eoa_merger()
    mock_contract, mock_w3, mock_account, fake_call = _patch_merge_internals(m)
    fake_call.estimate_gas.side_effect = RuntimeError("revert on estimate")
    with patch.object(m, "_contract", mock_contract), \
         patch.object(m, "_w3", mock_w3), \
         patch.object(m, "_account", mock_account):
        result = await m.merge_pair(FAKE_CONDITION, amount_shares=1.0)
    assert result["status"] == "success"
    tx_kwargs = fake_call.build_transaction.call_args.args[0]
    assert tx_kwargs["gas"] == 300_000


@pytest.mark.asyncio
async def test_merge_pair_proxy_mode_sends_to_funder_from_eoa():
    m = _make_proxy_merger()

    fake_call = MagicMock()
    fake_call.estimate_gas.return_value = 250_000
    fake_call.build_transaction.return_value = {
        "from": m.address,
        "to": m.funder_address,
        "data": "0x",
        "nonce": 11,
        "gas": 300_000,
        "chainId": 137,
    }
    fake_proxy_functions = MagicMock()
    fake_proxy_functions.proxy.return_value = fake_call
    mock_proxy_contract = MagicMock()
    mock_proxy_contract.functions = fake_proxy_functions

    mock_w3 = MagicMock()
    mock_w3.eth.get_transaction_count.return_value = 11
    mock_w3.eth.send_raw_transaction.return_value = b"\x00" * 31 + b"\x07"
    mock_w3.eth.wait_for_transaction_receipt.return_value = {
        "status": 1,
        "gasUsed": 180_000,
    }

    mock_signed = MagicMock()
    mock_signed.raw_transaction = b"\xab\xcd\xef\x01"
    mock_account = MagicMock()
    mock_account.sign_transaction.return_value = mock_signed

    with patch.object(m, "_proxy_contract", mock_proxy_contract), \
         patch.object(m, "_w3", mock_w3), \
         patch.object(m, "_account", mock_account):
        result = await m.merge_pair(FAKE_CONDITION, amount_shares=2.0)

    assert result["status"] == "success"
    assert result["gas_used"] == 180_000
    assert result["amount_base_units"] == 2_000_000

    fake_proxy_functions.proxy.assert_called_once()
    (calls_arg,), _ = fake_proxy_functions.proxy.call_args
    assert len(calls_arg) == 1
    type_code, to_addr, value, data_bytes = calls_arg[0]
    assert type_code == 1
    assert to_addr == m.ctf_address
    assert value == 0
    assert isinstance(data_bytes, bytes)
    # Inner mergePositions selector
    from eth_utils import function_signature_to_4byte_selector
    expected = function_signature_to_4byte_selector(
        "mergePositions(address,bytes32,bytes32,uint256[],uint256)"
    )
    assert data_bytes[:4] == expected

    tx_kwargs = fake_call.build_transaction.call_args.args[0]
    assert tx_kwargs["from"] == m.address
    assert tx_kwargs["chainId"] == 137


@pytest.mark.asyncio
async def test_merge_gnosis_safe_raises_not_implemented():
    m = _make_proxy_merger(sig_type=2)
    with pytest.raises(NotImplementedError, match="GNOSIS_SAFE"):
        await m.merge_pair(FAKE_CONDITION, amount_shares=1.0)


@pytest.mark.asyncio
async def test_merge_via_proxy_requires_proxy_mode():
    m = _make_eoa_merger()
    with pytest.raises(RuntimeError, match="signature_type=1"):
        await m.merge_via_proxy(FAKE_CONDITION, amount_base_units=1_000_000)
