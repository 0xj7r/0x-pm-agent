from __future__ import annotations

from clients.ctf_merger import CTFMerger


USDC_E = "0x2791Bca1F2de4661ED88A30C99A7a9449Aa84174"
CTF_ADDR = "0x4D97DCd97eC945f40cF65F87097ACe5EA0476045"
FAKE_KEY = "0x" + "11" * 32
FAKE_CONDITION = "0x" + "ab" * 32


def _make_real_merger() -> CTFMerger:
    return CTFMerger(
        web3_provider_url="https://polygon-rpc.invalid",
        private_key=FAKE_KEY,
        ctf_address=CTF_ADDR,
        collateral_token_address=USDC_E,
        chain_id=137,
    )


def test_encode_merge_calldata_contains_condition_and_amount():
    merger = _make_real_merger()
    data = merger.encode_merge_calldata(FAKE_CONDITION, amount=7, partition=[1, 2]).lower()
    assert data.startswith("0x")
    assert "2791bca1f2de4661ed88a30c99a7a9449aa84174" in data
    assert "ab" * 32 in data
    assert "0000000000000000000000000000000000000000000000000000000000000007" in data


def test_shares_to_amount_uses_usdc_base_units_and_rounds_down():
    merger = _make_real_merger()
    assert merger.shares_to_amount(1.0) == 1_000_000
    assert merger.shares_to_amount("1.234567") == 1_234_567
    assert merger.shares_to_amount(1.2345678) == 1_234_567
