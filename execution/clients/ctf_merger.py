"""On-chain merge of paired outcome tokens via Polymarket's CTF."""

from __future__ import annotations

import asyncio
from decimal import Decimal, ROUND_DOWN
import logging
from typing import Any

from eth_account import Account
from web3 import Web3
from web3.types import TxReceipt

from clients.ctf_redeemer import (
    PROXY_CALL_TYPE_CALL,
    SIG_TYPE_GNOSIS_SAFE,
    SIG_TYPE_POLY_PROXY,
    _PROXY_WALLET_ABI,
    _normalize_bytes32,
)

logger = logging.getLogger(__name__)

_MERGE_POSITIONS_ABI: list[dict[str, Any]] = [
    {
        "inputs": [
            {"internalType": "address", "name": "collateralToken", "type": "address"},
            {"internalType": "bytes32", "name": "parentCollectionId", "type": "bytes32"},
            {"internalType": "bytes32", "name": "conditionId", "type": "bytes32"},
            {"internalType": "uint256[]", "name": "partition", "type": "uint256[]"},
            {"internalType": "uint256", "name": "amount", "type": "uint256"},
        ],
        "name": "mergePositions",
        "outputs": [],
        "stateMutability": "nonpayable",
        "type": "function",
    },
]


class CTFMerger:
    """Calls `mergePositions` on Polymarket's Conditional Tokens contract."""

    def __init__(
        self,
        web3_provider_url: str,
        private_key: str,
        ctf_address: str,
        collateral_token_address: str,
        chain_id: int = 137,
        signature_type: int | None = None,
        funder_address: str | None = None,
        collateral_decimals: int = 6,
    ) -> None:
        if not private_key:
            raise ValueError("private_key is required for CTFMerger")
        if not web3_provider_url:
            raise ValueError("web3_provider_url is required for CTFMerger")

        self._w3 = Web3(Web3.HTTPProvider(web3_provider_url))
        try:
            from web3.middleware import ExtraDataToPOAMiddleware
            self._w3.middleware_onion.inject(ExtraDataToPOAMiddleware, layer=0)
        except ImportError:
            from web3.middleware import geth_poa_middleware
            self._w3.middleware_onion.inject(geth_poa_middleware, layer=0)

        self._account = Account.from_key(private_key)
        self._address = self._account.address
        self._chain_id = chain_id
        self._ctf_address = Web3.to_checksum_address(ctf_address)
        self._collateral = Web3.to_checksum_address(collateral_token_address)
        self._collateral_decimals = int(collateral_decimals)
        self._collateral_scale = Decimal(10) ** self._collateral_decimals
        self._contract = self._w3.eth.contract(
            address=self._ctf_address, abi=_MERGE_POSITIONS_ABI
        )

        self._signature_type = signature_type
        self._funder_address: str | None = None
        self._proxy_contract = None
        if signature_type == SIG_TYPE_POLY_PROXY:
            if not funder_address:
                raise ValueError("funder_address is required when signature_type=1")
            self._funder_address = Web3.to_checksum_address(funder_address)
            self._proxy_contract = self._w3.eth.contract(
                address=self._funder_address, abi=_PROXY_WALLET_ABI
            )
        elif signature_type == SIG_TYPE_GNOSIS_SAFE:
            if not funder_address:
                raise ValueError("funder_address is required when signature_type=2")
            self._funder_address = Web3.to_checksum_address(funder_address)

    @property
    def position_owner(self) -> str:
        return self._funder_address or self._address

    def _build_call(self, condition_id: str, amount: int, partition: list[int]) -> Any:
        return self._contract.functions.mergePositions(
            self._collateral,
            b"\x00" * 32,
            _normalize_bytes32(condition_id),
            list(partition),
            int(amount),
        )

    def _build_merge_calldata(self, condition_id: str, amount: int, partition: list[int]) -> str:
        return self._contract.encode_abi(
            abi_element_identifier="mergePositions",
            args=[
                self._collateral,
                b"\x00" * 32,
                _normalize_bytes32(condition_id),
                list(partition),
                int(amount),
            ],
        )

    def _build_proxy_call(self, condition_id: str, amount: int, partition: list[int]) -> Any:
        if self._proxy_contract is None:
            raise RuntimeError("proxy contract not configured")
        merge_data = self._build_merge_calldata(condition_id, amount, partition)
        merge_bytes = bytes.fromhex(merge_data[2:] if merge_data.startswith("0x") else merge_data)
        calls = [(PROXY_CALL_TYPE_CALL, self._ctf_address, 0, merge_bytes)]
        return self._proxy_contract.functions.proxy(calls)

    def encode_merge_calldata(self, condition_id: str, amount: int, partition: list[int] = [1, 2]) -> str:
        return self._build_merge_calldata(condition_id, amount, partition)

    def shares_to_amount(self, shares: float | str | Decimal) -> int:
        quantized = (Decimal(str(shares)) * self._collateral_scale).to_integral_value(
            rounding=ROUND_DOWN
        )
        return max(0, int(quantized))

    async def merge(
        self,
        condition_id: str,
        amount: int,
        partition: list[int] = [1, 2],
        timeout_seconds: int = 180,
    ) -> dict[str, Any]:
        if self._signature_type == SIG_TYPE_GNOSIS_SAFE:
            raise NotImplementedError("GNOSIS_SAFE merge is not implemented")
        if self._signature_type == SIG_TYPE_POLY_PROXY:
            call = self._build_proxy_call(condition_id, amount, partition)
            to_address = self._funder_address
            label = "ProxyWallet.proxy(mergePositions)"
        else:
            call = self._build_call(condition_id, amount, partition)
            to_address = self._ctf_address
            label = "mergePositions"
        return await self._send_and_wait(
            call,
            to_address=to_address or self._ctf_address,
            timeout_seconds=timeout_seconds,
            label=label,
            condition_id=condition_id,
            amount=amount,
        )

    async def _send_and_wait(
        self,
        call: Any,
        *,
        to_address: str,
        timeout_seconds: int,
        label: str,
        condition_id: str,
        amount: int,
    ) -> dict[str, Any]:
        def _send() -> str:
            try:
                gas_est = call.estimate_gas({"from": self._address})
                gas_limit = int(gas_est * 12 // 10)
            except Exception as exc:
                logger.warning("%s gas estimate failed, falling back to 300k: %s", label, exc)
                gas_limit = 300_000

            nonce = self._w3.eth.get_transaction_count(self._address)
            tx = call.build_transaction(
                {
                    "from": self._address,
                    "nonce": nonce,
                    "gas": gas_limit,
                    "chainId": self._chain_id,
                }
            )
            signed = self._account.sign_transaction(tx)
            raw = getattr(signed, "raw_transaction", None) or signed.rawTransaction
            tx_hash_bytes = self._w3.eth.send_raw_transaction(raw)
            tx_hash = tx_hash_bytes.hex()
            return tx_hash if tx_hash.startswith("0x") else f"0x{tx_hash}"

        def _wait(tx_hash: str) -> TxReceipt:
            return self._w3.eth.wait_for_transaction_receipt(tx_hash, timeout=timeout_seconds)

        tx_hash = ""
        try:
            tx_hash = await asyncio.to_thread(_send)
            logger.info("%s sent: tx=%s to=%s condition=%s amount=%s", label, tx_hash, to_address, condition_id, amount)
        except Exception as exc:
            logger.error("%s send failed for %s amount=%s: %s", label, condition_id, amount, exc)
            return {"tx_hash": tx_hash, "status": "failed", "gas_used": 0}

        try:
            receipt = await asyncio.to_thread(_wait, tx_hash)
        except Exception as exc:
            logger.error("%s receipt wait timed out for %s: %s", label, tx_hash, exc)
            return {"tx_hash": tx_hash, "status": "timeout", "gas_used": 0}

        status_code = int(receipt.get("status", 0)) if isinstance(receipt, dict) else int(getattr(receipt, "status", 0))
        gas_used = int(receipt.get("gasUsed", 0)) if isinstance(receipt, dict) else int(getattr(receipt, "gasUsed", 0))
        return {"tx_hash": tx_hash, "status": "success" if status_code == 1 else "failed", "gas_used": gas_used}
