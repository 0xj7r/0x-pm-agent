"""On-chain redemption of winning outcome tokens via Polymarket's CTF.

After a Polymarket market resolves, our winning ERC-1155 outcome tokens
sit in the wallet at $1 each but are not fungible USDC until someone
calls `redeemPositions` on the Gnosis ConditionalTokens Framework (CTF)
on Polygon. py-clob-client does not expose this, so we make the call
directly via web3.py.

Contract: 0x4D97DCd97eC945f40cF65F87097ACe5EA0476045 (Polymarket CTF).
Selector: redeemPositions(address,bytes32,bytes32,uint256[]) = 0x01b7037c.
"""

from __future__ import annotations

import asyncio
import logging
from typing import Any

from eth_account import Account
from web3 import Web3
from web3.types import TxReceipt

logger = logging.getLogger(__name__)


_REDEEM_POSITIONS_ABI: list[dict[str, Any]] = [
    {
        "inputs": [
            {"internalType": "address", "name": "collateralToken", "type": "address"},
            {"internalType": "bytes32", "name": "parentCollectionId", "type": "bytes32"},
            {"internalType": "bytes32", "name": "conditionId", "type": "bytes32"},
            {"internalType": "uint256[]", "name": "indexSets", "type": "uint256[]"},
        ],
        "name": "redeemPositions",
        "outputs": [],
        "stateMutability": "nonpayable",
        "type": "function",
    },
    {
        "inputs": [
            {"internalType": "address", "name": "owner", "type": "address"},
            {"internalType": "uint256", "name": "id", "type": "uint256"},
        ],
        "name": "balanceOf",
        "outputs": [{"internalType": "uint256", "name": "", "type": "uint256"}],
        "stateMutability": "view",
        "type": "function",
    },
]


def _normalize_bytes32(value: str | bytes) -> bytes:
    """Accept 0x-prefixed hex, unprefixed hex, or raw bytes; return 32 bytes."""
    if isinstance(value, bytes):
        if len(value) != 32:
            raise ValueError(f"bytes32 must be 32 bytes, got {len(value)}")
        return value
    s = value.lower()
    if s.startswith("0x"):
        s = s[2:]
    if len(s) != 64:
        raise ValueError(f"bytes32 hex must be 64 chars, got {len(s)}: {value!r}")
    return bytes.fromhex(s)


class CTFRedeemer:
    """Calls `redeemPositions` on Polymarket's CTF after market resolution.

    Instantiate only in live mode. Methods are async so they interleave with
    the engine's event loop, but the underlying web3.py calls are sync and
    are dispatched to a thread via `asyncio.to_thread`.
    """

    def __init__(
        self,
        web3_provider_url: str,
        private_key: str,
        ctf_address: str,
        collateral_token_address: str,
        chain_id: int = 137,
    ) -> None:
        if not private_key:
            raise ValueError("private_key is required for CTFRedeemer")
        if not web3_provider_url:
            raise ValueError("web3_provider_url is required for CTFRedeemer")

        self._w3 = Web3(Web3.HTTPProvider(web3_provider_url))
        self._account = Account.from_key(private_key)
        self._address = self._account.address
        self._chain_id = chain_id

        self._ctf_address = Web3.to_checksum_address(ctf_address)
        self._collateral = Web3.to_checksum_address(collateral_token_address)
        self._contract = self._w3.eth.contract(
            address=self._ctf_address, abi=_REDEEM_POSITIONS_ABI
        )
        logger.info(
            "CTFRedeemer initialized: ctf=%s wallet=%s collateral=%s chain_id=%s",
            self._ctf_address,
            self._address,
            self._collateral,
            self._chain_id,
        )

    @property
    def address(self) -> str:
        return self._address

    @property
    def ctf_address(self) -> str:
        return self._ctf_address

    def _build_call(self, condition_id: str, index_sets: list[int]):
        cid = _normalize_bytes32(condition_id)
        return self._contract.functions.redeemPositions(
            self._collateral,
            b"\x00" * 32,  # parentCollectionId: top-level
            cid,
            list(index_sets),
        )

    def encode_redeem_calldata(
        self, condition_id: str, index_sets: list[int] = [1, 2]
    ) -> str:
        """Return the ABI-encoded calldata for redeemPositions (for eyeballing)."""
        return self._contract.encode_abi(
            abi_element_identifier="redeemPositions",
            args=[
                self._collateral,
                b"\x00" * 32,
                _normalize_bytes32(condition_id),
                list(index_sets),
            ],
        )

    async def estimate_gas(
        self, condition_id: str, index_sets: list[int] = [1, 2]
    ) -> int:
        call = self._build_call(condition_id, index_sets)
        return await asyncio.to_thread(
            call.estimate_gas, {"from": self._address}
        )

    async def get_position_balance(self, token_id: str | int) -> int:
        """ERC-1155 `balanceOf(address, id)` on the CTF for our wallet.

        token_id: Polymarket outcome token id. Accepts decimal-string,
        0x-prefixed hex string, or int.
        """
        if isinstance(token_id, str):
            s = token_id.strip()
            tid = int(s, 16) if s.lower().startswith("0x") else int(s)
        else:
            tid = int(token_id)

        def _call() -> int:
            return self._contract.functions.balanceOf(self._address, tid).call()

        return await asyncio.to_thread(_call)

    async def redeem(
        self,
        condition_id: str,
        index_sets: list[int] = [1, 2],
        timeout_seconds: int = 180,
    ) -> dict[str, Any]:
        """Build, sign, and send the redeemPositions tx. Waits for receipt.

        Returns {"tx_hash": str, "status": "success"|"failed"|"timeout", "gas_used": int}.
        """
        call = self._build_call(condition_id, index_sets)

        def _send() -> tuple[str, int | None, str]:
            try:
                gas_est = call.estimate_gas({"from": self._address})
                gas_limit = int(gas_est * 12 // 10)  # 20% buffer
            except Exception as e:
                logger.warning("redeem gas estimate failed, falling back to 300k: %s", e)
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
            if not tx_hash.startswith("0x"):
                tx_hash = "0x" + tx_hash
            return tx_hash, gas_limit, tx_hash  # dummy placeholder

        def _wait(tx_hash: str) -> TxReceipt:
            return self._w3.eth.wait_for_transaction_receipt(
                tx_hash, timeout=timeout_seconds
            )

        tx_hash: str = ""
        try:
            tx_hash, _, _ = await asyncio.to_thread(_send)
            logger.info("redeemPositions sent: tx=%s condition=%s", tx_hash, condition_id)
        except Exception as e:
            logger.error("redeemPositions send failed for %s: %s", condition_id, e)
            return {"tx_hash": tx_hash, "status": "failed", "gas_used": 0}

        try:
            receipt = await asyncio.to_thread(_wait, tx_hash)
        except Exception as e:
            logger.error("redeemPositions receipt wait timed out for %s: %s", tx_hash, e)
            return {"tx_hash": tx_hash, "status": "timeout", "gas_used": 0}

        status_code = int(receipt.get("status", 0)) if isinstance(receipt, dict) else int(getattr(receipt, "status", 0))
        gas_used = int(receipt.get("gasUsed", 0)) if isinstance(receipt, dict) else int(getattr(receipt, "gasUsed", 0))
        status = "success" if status_code == 1 else "failed"
        logger.info(
            "redeemPositions receipt: tx=%s status=%s gas_used=%s",
            tx_hash,
            status,
            gas_used,
        )
        return {"tx_hash": tx_hash, "status": status, "gas_used": gas_used}
