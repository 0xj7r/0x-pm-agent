"""On-chain merging of paired Up+Down outcome tokens via Polymarket's CTF.

The whale-pair strategy posts GTC BUY limit orders on both Up and Down
5-min BTC outcomes at prices summing below $1.00. Once both legs fill,
we hold `N` Up shares and `N` Down shares, which the Gnosis Conditional
Tokens Framework (CTF) allows us to atomically collapse back into `N`
USDC via `mergePositions`. This is the symmetric inverse of
`splitPosition`.

Contract: 0x4D97DCd97eC945f40cF65F87097ACe5EA0476045 (Polymarket CTF on
Polygon, same contract the redeemer uses).
Function: mergePositions(address,bytes32,bytes32,uint256[],uint256).

Proxy-delegated mode
--------------------
When trading via a Polymarket proxy wallet (POLYMARKET_SIGNATURE_TYPE=1,
POLYMARKET_FUNDER=<proxy addr>), filled outcome tokens land on the
proxy contract, not the EOA. So the merge tx must be submitted with
the proxy as msg.sender to the CTF. We achieve this by signing with
the EOA but wrapping the merge calldata inside a call to the proxy's
`proxy((uint8,address,uint256,bytes)[])` entrypoint. The CTF burns
the ERC-1155 pair held by the proxy and transfers USDC back to
msg.sender = proxy, which is where trading capital lives.

See `clients/ctf_redeemer.py` for the sibling implementation that
redeems winning tokens after a market resolves. Both clients share the
same proxy wrapper ABI and the same send-and-wait flow.
"""

from __future__ import annotations

import asyncio
import logging
from typing import Any

from eth_account import Account
from web3 import Web3
from web3.types import TxReceipt

logger = logging.getLogger(__name__)

SIG_TYPE_EOA = 0
SIG_TYPE_POLY_PROXY = 1
SIG_TYPE_GNOSIS_SAFE = 2

PROXY_CALL_TYPE_CALL = 1


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


_PROXY_WALLET_ABI: list[dict[str, Any]] = [
    {
        "inputs": [
            {
                "components": [
                    {"internalType": "uint8", "name": "typeCode", "type": "uint8"},
                    {"internalType": "address", "name": "to", "type": "address"},
                    {"internalType": "uint256", "name": "value", "type": "uint256"},
                    {"internalType": "bytes", "name": "data", "type": "bytes"},
                ],
                "internalType": "struct ProxyWalletLib.ProxyCall[]",
                "name": "calls",
                "type": "tuple[]",
            }
        ],
        "name": "proxy",
        "outputs": [{"internalType": "bytes[]", "name": "returnValues", "type": "bytes[]"}],
        "stateMutability": "payable",
        "type": "function",
    }
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


def _to_base_units(shares: float, decimals: int = 6) -> int:
    """Convert fractional share count to CTF base units.

    Outcome tokens share USDC's 6 decimal precision on Polymarket's CTF.
    We round down to avoid attempting to merge more than we actually
    hold (`mergePositions` reverts if the caller's balance on either
    leg is short).
    """
    if shares <= 0:
        return 0
    return int(shares * (10**decimals))


class CTFMerger:
    """Calls `mergePositions` on Polymarket's CTF to collapse a paired
    Up+Down ERC-1155 pair into USDC.

    Instantiate only in live mode. Methods are async so they interleave
    with the engine's event loop, but the underlying web3.py calls are
    sync and are dispatched to a thread via `asyncio.to_thread`.
    """

    def __init__(
        self,
        web3_provider_url: str,
        private_key: str,
        ctf_address: str,
        collateral_token_address: str,
        chain_id: int = 137,
        signature_type: int | None = None,
        funder_address: str | None = None,
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
        self._contract = self._w3.eth.contract(
            address=self._ctf_address, abi=_MERGE_POSITIONS_ABI
        )

        self._signature_type = signature_type
        self._funder_address: str | None = None
        self._proxy_contract = None
        if signature_type == SIG_TYPE_POLY_PROXY:
            if not funder_address:
                raise ValueError(
                    "funder_address is required when signature_type=1 (POLY_PROXY)"
                )
            self._funder_address = Web3.to_checksum_address(funder_address)
            self._proxy_contract = self._w3.eth.contract(
                address=self._funder_address, abi=_PROXY_WALLET_ABI
            )
        elif signature_type == SIG_TYPE_GNOSIS_SAFE:
            if not funder_address:
                raise ValueError(
                    "funder_address is required when signature_type=2 (GNOSIS_SAFE)"
                )
            self._funder_address = Web3.to_checksum_address(funder_address)

        logger.info(
            "CTFMerger initialized: ctf=%s eoa=%s collateral=%s chain_id=%s "
            "sig_type=%s funder=%s",
            self._ctf_address,
            self._address,
            self._collateral,
            self._chain_id,
            self._signature_type if self._signature_type is not None else "default(EOA)",
            self._funder_address if self._funder_address else "n/a",
        )

    @property
    def address(self) -> str:
        """EOA signer address (always, regardless of mode)."""
        return self._address

    @property
    def ctf_address(self) -> str:
        return self._ctf_address

    @property
    def funder_address(self) -> str | None:
        return self._funder_address

    @property
    def signature_type(self) -> int | None:
        return self._signature_type

    @property
    def position_owner(self) -> str:
        """Address that actually holds ERC-1155 outcome tokens."""
        return self._funder_address or self._address

    def _build_call(
        self,
        condition_id: str,
        partition: list[int],
        amount_base_units: int,
    ):
        cid = _normalize_bytes32(condition_id)
        return self._contract.functions.mergePositions(
            self._collateral,
            b"\x00" * 32,  # parentCollectionId: top-level
            cid,
            list(partition),
            int(amount_base_units),
        )

    def _build_merge_calldata(
        self,
        condition_id: str,
        partition: list[int],
        amount_base_units: int,
    ) -> str:
        return self._contract.encode_abi(
            abi_element_identifier="mergePositions",
            args=[
                self._collateral,
                b"\x00" * 32,
                _normalize_bytes32(condition_id),
                list(partition),
                int(amount_base_units),
            ],
        )

    def _build_proxy_call(
        self,
        condition_id: str,
        partition: list[int],
        amount_base_units: int,
    ):
        if self._proxy_contract is None:
            raise RuntimeError(
                "proxy contract not configured; signature_type must be 1 with "
                "a funder_address to use proxy-delegated merge"
            )
        merge_data = self._build_merge_calldata(
            condition_id, partition, amount_base_units
        )
        merge_bytes = bytes.fromhex(
            merge_data[2:] if merge_data.startswith("0x") else merge_data
        )
        calls = [
            (PROXY_CALL_TYPE_CALL, self._ctf_address, 0, merge_bytes),
        ]
        return self._proxy_contract.functions.proxy(calls)

    def encode_merge_calldata(
        self,
        condition_id: str,
        amount_base_units: int,
        partition: list[int] = [1, 2],
    ) -> str:
        """ABI-encoded calldata for CTF.mergePositions (inspection helper)."""
        return self._build_merge_calldata(
            condition_id, partition, amount_base_units
        )

    def encode_proxy_merge_calldata(
        self,
        condition_id: str,
        amount_base_units: int,
        partition: list[int] = [1, 2],
    ) -> str:
        call = self._build_proxy_call(condition_id, partition, amount_base_units)
        return call._encode_transaction_data()

    async def estimate_gas(
        self,
        condition_id: str,
        amount_base_units: int,
        partition: list[int] = [1, 2],
    ) -> int:
        call = self._build_call(condition_id, partition, amount_base_units)
        return await asyncio.to_thread(
            call.estimate_gas, {"from": self._address}
        )

    async def get_position_balance(self, token_id: str | int) -> int:
        """ERC-1155 balanceOf(owner, token_id) on the CTF.

        In proxy mode the owner is the funder (proxy wallet); in EOA
        mode the owner is the signer.
        """
        if isinstance(token_id, str):
            s = token_id.strip()
            tid = int(s, 16) if s.lower().startswith("0x") else int(s)
        else:
            tid = int(token_id)

        owner = self.position_owner

        def _call() -> int:
            return self._contract.functions.balanceOf(owner, tid).call()

        return await asyncio.to_thread(_call)

    async def merge_pair(
        self,
        condition_id: str,
        amount_shares: float,
        partition: list[int] = [1, 2],
        timeout_seconds: int = 180,
        decimals: int = 6,
    ) -> dict[str, Any]:
        """Burn `amount_shares` Up+Down pairs back into USDC.

        `amount_shares` is a fractional share count matching how the
        strategy tracks inventory. We scale it to the CTF's 6-decimal
        base units internally and round down so we never try to merge
        more than we hold.

        Returns {"tx_hash": str, "status": "success"|"failed"|"timeout"|"skipped",
                 "gas_used": int, "amount_base_units": int}.
        """
        amount_base_units = _to_base_units(amount_shares, decimals=decimals)
        if amount_base_units <= 0:
            return {
                "tx_hash": "",
                "status": "skipped",
                "gas_used": 0,
                "amount_base_units": 0,
                "reason": "amount_base_units <= 0",
            }

        if self._signature_type == SIG_TYPE_GNOSIS_SAFE:
            raise NotImplementedError(
                "GNOSIS_SAFE (signature_type=2) merge is not implemented."
            )
        if self._signature_type == SIG_TYPE_POLY_PROXY:
            result = await self.merge_via_proxy(
                condition_id,
                amount_base_units,
                partition=partition,
                timeout_seconds=timeout_seconds,
            )
        else:
            result = await self._merge_direct(
                condition_id,
                amount_base_units,
                partition=partition,
                timeout_seconds=timeout_seconds,
            )
        result["amount_base_units"] = amount_base_units
        return result

    async def _merge_direct(
        self,
        condition_id: str,
        amount_base_units: int,
        partition: list[int],
        timeout_seconds: int = 180,
    ) -> dict[str, Any]:
        call = self._build_call(condition_id, partition, amount_base_units)
        return await self._send_and_wait(
            call,
            to_address=self._ctf_address,
            timeout_seconds=timeout_seconds,
            label="mergePositions",
            condition_id=condition_id,
        )

    async def merge_via_proxy(
        self,
        condition_id: str,
        amount_base_units: int,
        partition: list[int] = [1, 2],
        timeout_seconds: int = 180,
    ) -> dict[str, Any]:
        if self._signature_type != SIG_TYPE_POLY_PROXY:
            raise RuntimeError(
                "merge_via_proxy requires signature_type=1 (POLY_PROXY); "
                f"got {self._signature_type}"
            )
        if self._proxy_contract is None or self._funder_address is None:
            raise RuntimeError("proxy contract not configured")
        call = self._build_proxy_call(condition_id, partition, amount_base_units)
        return await self._send_and_wait(
            call,
            to_address=self._funder_address,
            timeout_seconds=timeout_seconds,
            label="ProxyWallet.proxy(mergePositions)",
            condition_id=condition_id,
        )

    async def _send_and_wait(
        self,
        call,
        to_address: str,
        timeout_seconds: int,
        label: str,
        condition_id: str,
    ) -> dict[str, Any]:
        def _send() -> str:
            try:
                gas_est = call.estimate_gas({"from": self._address})
                gas_limit = int(gas_est * 12 // 10)
            except Exception as e:
                logger.warning(
                    "%s gas estimate failed, falling back to 300k: %s", label, e
                )
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
            return tx_hash

        def _wait(tx_hash: str) -> TxReceipt:
            return self._w3.eth.wait_for_transaction_receipt(
                tx_hash, timeout=timeout_seconds
            )

        tx_hash: str = ""
        try:
            tx_hash = await asyncio.to_thread(_send)
            logger.info(
                "%s sent: tx=%s to=%s from=%s condition=%s",
                label, tx_hash, to_address, self._address, condition_id,
            )
        except Exception as e:
            logger.error("%s send failed for %s: %s", label, condition_id, e)
            return {"tx_hash": tx_hash, "status": "failed", "gas_used": 0}

        try:
            receipt = await asyncio.to_thread(_wait, tx_hash)
        except Exception as e:
            logger.error("%s receipt wait timed out for %s: %s", label, tx_hash, e)
            return {"tx_hash": tx_hash, "status": "timeout", "gas_used": 0}

        status_code = (
            int(receipt.get("status", 0))
            if isinstance(receipt, dict)
            else int(getattr(receipt, "status", 0))
        )
        gas_used = (
            int(receipt.get("gasUsed", 0))
            if isinstance(receipt, dict)
            else int(getattr(receipt, "gasUsed", 0))
        )
        status = "success" if status_code == 1 else "failed"
        logger.info(
            "%s receipt: tx=%s status=%s gas_used=%s",
            label, tx_hash, status, gas_used,
        )
        return {"tx_hash": tx_hash, "status": status, "gas_used": gas_used}
