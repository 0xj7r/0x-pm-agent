"""Tests for clients.ctf_redeemer.CTFRedeemer.

Mocks web3.Web3 and eth_account.Account so no real RPC or signing happens.
Covers calldata encoding, balanceOf(), and the three terminal states of
redeem(): success, failed (status=0), and timeout (receipt wait raises).
"""

from __future__ import annotations

from typing import Any
from unittest.mock import MagicMock, patch

import pytest

from web3 import Web3

from clients import ctf_redeemer as ctf_mod
from clients.ctf_redeemer import CTFRedeemer, _normalize_bytes32


USDC_E = "0x2791Bca1F2de4661ED88A30C99A7a9449Aa84174"
CTF_ADDR = "0x4D97DCd97eC945f40cF65F87097ACe5EA0476045"
FAKE_WALLET = "0x000000000000000000000000000000000000dEaD"
FAKE_KEY = "0x" + "11" * 32
FAKE_CONDITION = "0x" + "ab" * 32


def _make_real_redeemer() -> CTFRedeemer:
    """Build a real CTFRedeemer (uses genuine web3 ABI encoder, no network).

    The HTTPProvider is not contacted unless we make an RPC call. The
    Account.from_key call is real and cheap.
    """
    return CTFRedeemer(
        web3_provider_url="https://polygon-rpc.invalid",
        private_key=FAKE_KEY,
        ctf_address=CTF_ADDR,
        collateral_token_address=USDC_E,
        chain_id=137,
    )


def test_normalize_bytes32_accepts_0x_prefixed_hex():
    out = _normalize_bytes32("0x" + "ab" * 32)
    assert isinstance(out, bytes)
    assert len(out) == 32


def test_normalize_bytes32_rejects_wrong_length():
    with pytest.raises(ValueError):
        _normalize_bytes32("0xabcd")


def test_normalize_bytes32_passes_raw_bytes():
    raw = b"\x01" * 32
    assert _normalize_bytes32(raw) == raw


def test_init_requires_private_key():
    with pytest.raises(ValueError):
        CTFRedeemer(
            web3_provider_url="https://polygon-rpc.invalid",
            private_key="",
            ctf_address=CTF_ADDR,
            collateral_token_address=USDC_E,
        )


def test_init_requires_provider_url():
    with pytest.raises(ValueError):
        CTFRedeemer(
            web3_provider_url="",
            private_key=FAKE_KEY,
            ctf_address=CTF_ADDR,
            collateral_token_address=USDC_E,
        )


def test_encode_redeem_calldata_has_correct_selector():
    r = _make_real_redeemer()
    data = r.encode_redeem_calldata(FAKE_CONDITION, [1, 2])
    # redeemPositions(address,bytes32,bytes32,uint256[]) selector = 0x01b7037c
    assert data.startswith("0x01b7037c"), f"unexpected selector: {data[:10]}"


def test_encode_redeem_calldata_contains_args():
    r = _make_real_redeemer()
    data = r.encode_redeem_calldata(FAKE_CONDITION, [1, 2]).lower()
    # collateral token address (right-padded in a 32-byte word)
    assert "2791bca1f2de4661ed88a30c99a7a9449aa84174" in data
    # parentCollectionId = bytes32(0): 64 consecutive zero chars after selector
    assert "0" * 64 in data
    # conditionId
    assert "ab" * 32 in data


def test_encode_redeem_calldata_with_only_yes_partition():
    r = _make_real_redeemer()
    data = r.encode_redeem_calldata(FAKE_CONDITION, [1])
    assert data.startswith("0x01b7037c")


@pytest.mark.asyncio
async def test_get_position_balance_calls_balanceOf_with_wallet_and_token_id():
    r = _make_real_redeemer()
    token_id_dec = "12345678901234567890"

    fake_fn = MagicMock()
    fake_fn.call.return_value = 500_000_000
    fake_functions = MagicMock()
    fake_functions.balanceOf.return_value = fake_fn

    with patch.object(r, "_contract") as mock_contract:
        mock_contract.functions = fake_functions
        bal = await r.get_position_balance(token_id_dec)

    assert bal == 500_000_000
    fake_functions.balanceOf.assert_called_once_with(r.address, int(token_id_dec))


@pytest.mark.asyncio
async def test_get_position_balance_accepts_hex_token_id():
    r = _make_real_redeemer()

    fake_fn = MagicMock()
    fake_fn.call.return_value = 7
    fake_functions = MagicMock()
    fake_functions.balanceOf.return_value = fake_fn

    with patch.object(r, "_contract") as mock_contract:
        mock_contract.functions = fake_functions
        bal = await r.get_position_balance("0x10")

    assert bal == 7
    fake_functions.balanceOf.assert_called_once_with(r.address, 16)


@pytest.mark.asyncio
async def test_estimate_gas_returns_estimate_from_contract():
    r = _make_real_redeemer()

    fake_call = MagicMock()
    fake_call.estimate_gas.return_value = 123_456
    fake_functions = MagicMock()
    fake_functions.redeemPositions.return_value = fake_call

    with patch.object(r, "_contract") as mock_contract:
        mock_contract.functions = fake_functions
        out = await r.estimate_gas(FAKE_CONDITION, [1, 2])

    assert out == 123_456
    fake_call.estimate_gas.assert_called_once_with({"from": r.address})


def _patch_redeem_internals(
    r: CTFRedeemer,
    *,
    estimate_gas: int = 200_000,
    send_tx_hash: bytes = b"\x00" * 31 + b"\x01",
    receipt_status: int = 1,
    receipt_gas: int = 150_000,
    wait_raises: Exception | None = None,
):
    """Return a patched (contract, w3, account) trio for redeem() tests."""
    fake_call = MagicMock()
    fake_call.estimate_gas.return_value = estimate_gas
    fake_call.build_transaction.return_value = {
        "from": r.address,
        "to": r.ctf_address,
        "data": "0x01b7037c",
        "nonce": 42,
        "gas": 300_000,
        "chainId": 137,
    }
    fake_functions = MagicMock()
    fake_functions.redeemPositions.return_value = fake_call

    mock_contract = MagicMock()
    mock_contract.functions = fake_functions

    mock_w3 = MagicMock()
    mock_w3.eth.get_transaction_count.return_value = 42
    mock_w3.eth.send_raw_transaction.return_value = send_tx_hash
    if wait_raises is not None:
        mock_w3.eth.wait_for_transaction_receipt.side_effect = wait_raises
    else:
        mock_w3.eth.wait_for_transaction_receipt.return_value = {
            "status": receipt_status,
            "gasUsed": receipt_gas,
        }

    mock_signed = MagicMock()
    mock_signed.raw_transaction = b"\xde\xad\xbe\xef"
    mock_account = MagicMock()
    mock_account.sign_transaction.return_value = mock_signed

    return mock_contract, mock_w3, mock_account, fake_call


@pytest.mark.asyncio
async def test_redeem_success_returns_tx_hash_and_gas():
    r = _make_real_redeemer()
    mock_contract, mock_w3, mock_account, fake_call = _patch_redeem_internals(r)
    with patch.object(r, "_contract", mock_contract), \
         patch.object(r, "_w3", mock_w3), \
         patch.object(r, "_account", mock_account):
        result = await r.redeem(FAKE_CONDITION, [1, 2])

    assert result["status"] == "success"
    assert result["gas_used"] == 150_000
    assert result["tx_hash"].startswith("0x")
    assert len(result["tx_hash"]) == 66

    fake_call.build_transaction.assert_called_once()
    tx_kwargs = fake_call.build_transaction.call_args.args[0]
    assert tx_kwargs["from"] == r.address
    assert tx_kwargs["nonce"] == 42
    assert tx_kwargs["chainId"] == 137
    assert tx_kwargs["gas"] == 240_000  # 200_000 * 1.2

    mock_account.sign_transaction.assert_called_once()
    mock_w3.eth.send_raw_transaction.assert_called_once_with(b"\xde\xad\xbe\xef")


@pytest.mark.asyncio
async def test_redeem_failed_when_receipt_status_zero():
    r = _make_real_redeemer()
    mock_contract, mock_w3, mock_account, _ = _patch_redeem_internals(
        r, receipt_status=0
    )
    with patch.object(r, "_contract", mock_contract), \
         patch.object(r, "_w3", mock_w3), \
         patch.object(r, "_account", mock_account):
        result = await r.redeem(FAKE_CONDITION, [1, 2])

    assert result["status"] == "failed"
    assert result["tx_hash"].startswith("0x")
    assert result["gas_used"] == 150_000


@pytest.mark.asyncio
async def test_redeem_timeout_when_wait_raises():
    r = _make_real_redeemer()
    mock_contract, mock_w3, mock_account, _ = _patch_redeem_internals(
        r, wait_raises=TimeoutError("receipt timeout")
    )
    with patch.object(r, "_contract", mock_contract), \
         patch.object(r, "_w3", mock_w3), \
         patch.object(r, "_account", mock_account):
        result = await r.redeem(FAKE_CONDITION, [1, 2])

    assert result["status"] == "timeout"
    assert result["tx_hash"].startswith("0x")
    assert result["gas_used"] == 0


@pytest.mark.asyncio
async def test_redeem_failed_when_send_raises():
    r = _make_real_redeemer()
    mock_contract, mock_w3, mock_account, _ = _patch_redeem_internals(r)
    mock_w3.eth.send_raw_transaction.side_effect = RuntimeError("rpc down")
    with patch.object(r, "_contract", mock_contract), \
         patch.object(r, "_w3", mock_w3), \
         patch.object(r, "_account", mock_account):
        result = await r.redeem(FAKE_CONDITION, [1, 2])

    assert result["status"] == "failed"
    assert result["gas_used"] == 0


@pytest.mark.asyncio
async def test_redeem_falls_back_to_300k_gas_when_estimate_raises():
    r = _make_real_redeemer()
    mock_contract, mock_w3, mock_account, fake_call = _patch_redeem_internals(r)
    fake_call.estimate_gas.side_effect = RuntimeError("execution reverted on estimate")
    with patch.object(r, "_contract", mock_contract), \
         patch.object(r, "_w3", mock_w3), \
         patch.object(r, "_account", mock_account):
        result = await r.redeem(FAKE_CONDITION, [1, 2])

    # Should still attempt send + wait after fallback.
    assert result["status"] == "success"
    tx_kwargs = fake_call.build_transaction.call_args.args[0]
    assert tx_kwargs["gas"] == 300_000


# Proxy-delegated mode (signature_type=1 / POLY_PROXY)

FAKE_FUNDER = "0xa57189d5b2285A5E64083d3925687bDFCE01fC83"


def _make_proxy_redeemer(
    funder: str = FAKE_FUNDER, sig_type: int = 1
) -> CTFRedeemer:
    return CTFRedeemer(
        web3_provider_url="https://polygon-rpc.invalid",
        private_key=FAKE_KEY,
        ctf_address=CTF_ADDR,
        collateral_token_address=USDC_E,
        chain_id=137,
        signature_type=sig_type,
        funder_address=funder,
    )


def test_init_proxy_mode_requires_funder():
    with pytest.raises(ValueError, match="funder_address"):
        CTFRedeemer(
            web3_provider_url="https://polygon-rpc.invalid",
            private_key=FAKE_KEY,
            ctf_address=CTF_ADDR,
            collateral_token_address=USDC_E,
            signature_type=1,
            funder_address=None,
        )


def test_init_proxy_mode_sets_position_owner_to_funder():
    r = _make_proxy_redeemer()
    assert r.funder_address == Web3.to_checksum_address(FAKE_FUNDER)
    assert r.position_owner == Web3.to_checksum_address(FAKE_FUNDER)
    # EOA signer address is unchanged and separate.
    assert r.address != r.position_owner


def test_init_eoa_mode_position_owner_is_signer():
    r = _make_real_redeemer()
    assert r.funder_address is None
    assert r.position_owner == r.address


def test_encode_proxy_redeem_calldata_wraps_ctf_selector():
    """The outer calldata selector is ProxyWallet.proxy(...); the inner
    bytes payload contains the CTF redeemPositions selector 0x01b7037c."""
    # Need real Web3 ABI encoder; Web3 imported at top of ctf_redeemer is
    # available transitively.
    from web3 import Web3 as _Web3  # noqa: F401 (ensures module loaded)

    r = _make_proxy_redeemer()
    outer = r.encode_proxy_redeem_calldata(FAKE_CONDITION, [1, 2]).lower()

    # proxy((uint8,address,uint256,bytes)[]) selector. Compute it instead
    # of hardcoding so the test stays honest if ABI encoding lib changes.
    from eth_utils import function_signature_to_4byte_selector

    proxy_sel = function_signature_to_4byte_selector(
        "proxy((uint8,address,uint256,bytes)[])"
    ).hex()
    assert outer.startswith("0x" + proxy_sel), (
        f"expected proxy selector 0x{proxy_sel} at head, got {outer[:10]}"
    )

    # Inner CTF selector 0x01b7037c (redeemPositions) appears somewhere
    # inside the wrapped `data` bytes field.
    assert "01b7037c" in outer, "inner CTF redeemPositions selector missing"

    # Funder/proxy address does NOT appear in outer calldata (it's the
    # `to` of the tx, not an arg). CTF address DOES appear as the
    # per-ProxyCall target.
    assert CTF_ADDR.lower()[2:] in outer
    # Collateral (USDC.e) is inside the nested CTF calldata.
    assert USDC_E.lower()[2:] in outer


@pytest.mark.asyncio
async def test_redeem_proxy_mode_sends_to_funder_from_eoa():
    r = _make_proxy_redeemer()

    # Mock out proxy_contract.functions.proxy(...) so we can assert the
    # ProxyCall args and inspect the built tx.
    fake_call = MagicMock()
    fake_call.estimate_gas.return_value = 250_000
    fake_call.build_transaction.return_value = {
        "from": r.address,
        "to": r.funder_address,
        "data": "0xabc123",
        "nonce": 7,
        "gas": 300_000,
        "chainId": 137,
    }
    fake_proxy_functions = MagicMock()
    fake_proxy_functions.proxy.return_value = fake_call
    mock_proxy_contract = MagicMock()
    mock_proxy_contract.functions = fake_proxy_functions

    mock_w3 = MagicMock()
    mock_w3.eth.get_transaction_count.return_value = 7
    mock_w3.eth.send_raw_transaction.return_value = b"\x00" * 31 + b"\x02"
    mock_w3.eth.wait_for_transaction_receipt.return_value = {
        "status": 1,
        "gasUsed": 220_000,
    }

    mock_signed = MagicMock()
    mock_signed.raw_transaction = b"\xca\xfe\xba\xbe"
    mock_account = MagicMock()
    mock_account.sign_transaction.return_value = mock_signed

    with patch.object(r, "_proxy_contract", mock_proxy_contract), \
         patch.object(r, "_w3", mock_w3), \
         patch.object(r, "_account", mock_account):
        result = await r.redeem(FAKE_CONDITION, [1, 2])

    assert result["status"] == "success"
    assert result["gas_used"] == 220_000

    # The proxy(...) call received exactly one ProxyCall tuple pointing
    # at the CTF with the redeemPositions calldata as its `data`.
    fake_proxy_functions.proxy.assert_called_once()
    (calls_arg,), _ = fake_proxy_functions.proxy.call_args
    assert len(calls_arg) == 1
    type_code, to_addr, value, data_bytes = calls_arg[0]
    assert type_code == 1  # CALL
    assert to_addr == r.ctf_address
    assert value == 0
    assert isinstance(data_bytes, bytes)
    # Inner CTF selector
    assert data_bytes[:4].hex() == "01b7037c"

    # Tx was built from the EOA signer, not the proxy.
    tx_kwargs = fake_call.build_transaction.call_args.args[0]
    assert tx_kwargs["from"] == r.address  # EOA
    assert tx_kwargs["nonce"] == 7
    assert tx_kwargs["chainId"] == 137
    assert tx_kwargs["gas"] == 300_000  # 250_000 * 1.2

    # The outgoing tx signs the EOA, raw_tx goes to RPC.
    mock_account.sign_transaction.assert_called_once()
    mock_w3.eth.send_raw_transaction.assert_called_once_with(b"\xca\xfe\xba\xbe")


@pytest.mark.asyncio
async def test_get_position_balance_queries_funder_in_proxy_mode():
    r = _make_proxy_redeemer()
    token_id_dec = "99999999999999999999"

    fake_fn = MagicMock()
    fake_fn.call.return_value = 1_234_567
    fake_functions = MagicMock()
    fake_functions.balanceOf.return_value = fake_fn

    with patch.object(r, "_contract") as mock_contract:
        mock_contract.functions = fake_functions
        bal = await r.get_position_balance(token_id_dec)

    assert bal == 1_234_567
    # Called with the FUNDER (proxy) address, not the EOA signer.
    fake_functions.balanceOf.assert_called_once_with(
        r.funder_address, int(token_id_dec)
    )
    assert fake_functions.balanceOf.call_args.args[0] != r.address


@pytest.mark.asyncio
async def test_get_position_balance_queries_signer_in_eoa_mode():
    """Regression: EOA-mode balance queries still use the signer."""
    r = _make_real_redeemer()
    fake_fn = MagicMock()
    fake_fn.call.return_value = 42
    fake_functions = MagicMock()
    fake_functions.balanceOf.return_value = fake_fn

    with patch.object(r, "_contract") as mock_contract:
        mock_contract.functions = fake_functions
        bal = await r.get_position_balance("0x10")

    assert bal == 42
    fake_functions.balanceOf.assert_called_once_with(r.address, 16)


@pytest.mark.asyncio
async def test_redeem_gnosis_safe_raises_not_implemented():
    r = _make_proxy_redeemer(sig_type=2)  # Safe mode stores funder but no impl.
    with pytest.raises(NotImplementedError, match="GNOSIS_SAFE"):
        await r.redeem(FAKE_CONDITION, [1, 2])


@pytest.mark.asyncio
async def test_redeem_via_proxy_requires_proxy_mode():
    """Calling redeem_via_proxy on an EOA-mode redeemer is a config error."""
    r = _make_real_redeemer()
    with pytest.raises(RuntimeError, match="signature_type=1"):
        await r.redeem_via_proxy(FAKE_CONDITION, [1, 2])
