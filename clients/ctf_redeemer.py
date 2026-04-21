"""On-chain redemption of winning outcome tokens via Polymarket's CTF.

After a Polymarket market resolves, our winning ERC-1155 outcome tokens
sit in the wallet at $1 each but are not fungible USDC until someone
calls `redeemPositions` on the Gnosis ConditionalTokens Framework (CTF)
on Polygon. py-clob-client does not expose this, so we make the call
directly via web3.py.

Contract: 0x4D97DCd97eC945f40cF65F87097ACe5EA0476045 (Polymarket CTF).
Selector: redeemPositions(address,bytes32,bytes32,uint256[]) = 0x01b7037c.

Proxy-delegated mode
--------------------
When trading via a Polymarket proxy wallet (POLYMARKET_SIGNATURE_TYPE=1,
POLYMARKET_FUNDER=<proxy addr>), winning ERC-1155 tokens land on the
proxy contract, not the EOA. The EOA owns the proxy, so to redeem we
submit a transaction signed by the EOA that calls the proxy's
`proxy((uint8,address,uint256,bytes)[])` entrypoint with a single
CALL tuple wrapping the CTF redeemPositions calldata. CTF then
transfers USDC to msg.sender = proxy, which is where trading capital
lives. See Polymarket ProxyWalletFactory at
0xaB45c5A4B0c941a2F231C04C3f49182e1A254052 (verified on Polygonscan)
and the poly-web3 reference implementation
(github.com/tosmart01/poly-web3, poly_web3/const.py).
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


# Polymarket ProxyWallet entrypoint (verified on Polygonscan at
# 0xaB45c5A4B0c941a2F231C04C3f49182e1A254052, the ProxyWalletFactory; the
# cloned ProxyWallet inherits the same `proxy` function).
# Struct ProxyCall { uint8 typeCode; address to; uint256 value; bytes data; }
# typeCode: 1 = CALL, 2 = DELEGATECALL. We always use CALL for redemption.
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
        signature_type: int | None = None,
        funder_address: str | None = None,
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

        # Proxy-delegated mode: winning ERC-1155 tokens land on the proxy,
        # not the EOA. We still sign with the EOA but wrap the redeem
        # calldata in a proxy.proxy([...]) call so the proxy is msg.sender
        # to CTF.redeemPositions and therefore the USDC recipient.
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
        # sig_type 0 / None: direct EOA path; funder_address is ignored.

        logger.info(
            "CTFRedeemer initialized: ctf=%s eoa=%s collateral=%s chain_id=%s "
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
        """Proxy/Safe address holding the outcome tokens, if proxy-delegated."""
        return self._funder_address

    @property
    def signature_type(self) -> int | None:
        return self._signature_type

    @property
    def position_owner(self) -> str:
        """Address that actually holds ERC-1155 outcome tokens.

        In EOA mode this is the signer; in proxy mode it's the funder.
        """
        return self._funder_address or self._address

    def _build_call(self, condition_id: str, index_sets: list[int]):
        cid = _normalize_bytes32(condition_id)
        return self._contract.functions.redeemPositions(
            self._collateral,
            b"\x00" * 32,  # parentCollectionId: top-level
            cid,
            list(index_sets),
        )

    def _build_redeem_calldata(
        self, condition_id: str, index_sets: list[int]
    ) -> str:
        """Raw hex calldata for CTF.redeemPositions(...). Used by both
        direct-EOA and proxy-delegated send paths."""
        return self._contract.encode_abi(
            abi_element_identifier="redeemPositions",
            args=[
                self._collateral,
                b"\x00" * 32,
                _normalize_bytes32(condition_id),
                list(index_sets),
            ],
        )

    def _build_proxy_call(self, condition_id: str, index_sets: list[int]):
        """Wrap redeem calldata in a single-item ProxyCall[] for
        ProxyWallet.proxy(). typeCode=1 (CALL), value=0, to=CTF."""
        if self._proxy_contract is None:
            raise RuntimeError(
                "proxy contract not configured; signature_type must be 1 with "
                "a funder_address to use proxy-delegated redemption"
            )
        redeem_data = self._build_redeem_calldata(condition_id, index_sets)
        redeem_bytes = bytes.fromhex(redeem_data[2:] if redeem_data.startswith("0x") else redeem_data)
        calls = [
            (PROXY_CALL_TYPE_CALL, self._ctf_address, 0, redeem_bytes),
        ]
        return self._proxy_contract.functions.proxy(calls)

    def encode_redeem_calldata(
        self, condition_id: str, index_sets: list[int] = [1, 2]
    ) -> str:
        """Return the ABI-encoded calldata for redeemPositions (for eyeballing)."""
        return self._build_redeem_calldata(condition_id, index_sets)

    def encode_proxy_redeem_calldata(
        self, condition_id: str, index_sets: list[int] = [1, 2]
    ) -> str:
        """Return the ABI-encoded calldata for ProxyWallet.proxy([...])
        wrapping the CTF redeemPositions call. Useful for inspection."""
        call = self._build_proxy_call(condition_id, index_sets)
        return call._encode_transaction_data()

    async def estimate_gas(
        self, condition_id: str, index_sets: list[int] = [1, 2]
    ) -> int:
        call = self._build_call(condition_id, index_sets)
        return await asyncio.to_thread(
            call.estimate_gas, {"from": self._address}
        )

    async def get_position_balance(self, token_id: str | int) -> int:
        """ERC-1155 `balanceOf(owner, id)` on the CTF.

        In proxy-delegated mode the owner is the funder (proxy wallet),
        which is where outcome tokens actually land. In direct-EOA mode
        the owner is the signer.

        token_id: Polymarket outcome token id. Accepts decimal-string,
        0x-prefixed hex string, or int.
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

    async def sweep_wallet(
        self,
        positions: list[dict[str, Any]],
        index_sets: list[int] = [1, 2],
    ) -> list[dict[str, Any]]:
        """Redeem every winning position with a non-zero ERC-1155 balance.

        `positions` is a list of {"condition_id": str, "token_id": str|int}
        dicts, typically derived from event_log resolution events where
        `won=True`. For each, we check `balanceOf(owner, token_id)` on the
        CTF; if > 0 we call `redeemPositions`. Each `condition_id` is
        redeemed at most once per sweep even if both outcome tokens are
        held (redeemPositions burns all winning positions in one call).

        Returns a list of per-condition result dicts:
          {
            "condition_id": str,
            "tx_hash": str,          # empty string if skipped/failed-precheck
            "status": "success" | "failed" | "timeout" | "skipped" | "error",
            "gas_used": int,
            "reason": str,           # optional: why skipped/errored
          }

        A position is "skipped" when balanceOf returns 0 (already redeemed
        or never held). An "error" status means the balanceOf precheck
        itself raised; we do NOT attempt a blind redeem in that case
        because it would waste gas on an empty position.
        """
        results: list[dict[str, Any]] = []
        seen_conditions: set[str] = set()
        for pos in positions:
            condition_id = pos.get("condition_id")
            token_id = pos.get("token_id")
            if not condition_id:
                continue
            cid_norm = condition_id.lower()
            if cid_norm in seen_conditions:
                continue
            seen_conditions.add(cid_norm)

            if token_id is None:
                # No token to probe: skip rather than redeem blindly.
                results.append({
                    "condition_id": condition_id,
                    "tx_hash": "",
                    "status": "skipped",
                    "gas_used": 0,
                    "reason": "no token_id to probe balance",
                })
                continue

            try:
                bal = await self.get_position_balance(token_id)
            except Exception as e:
                logger.warning(
                    "[SWEEP] balanceOf failed for condition=%s token=%s: %s",
                    condition_id,
                    str(token_id)[:16],
                    e,
                )
                results.append({
                    "condition_id": condition_id,
                    "tx_hash": "",
                    "status": "error",
                    "gas_used": 0,
                    "reason": f"balanceOf failed: {e}",
                })
                continue

            if bal == 0:
                results.append({
                    "condition_id": condition_id,
                    "tx_hash": "",
                    "status": "skipped",
                    "gas_used": 0,
                    "reason": "zero balance (already redeemed)",
                })
                continue

            try:
                res = await self.redeem(condition_id, index_sets=index_sets)
            except Exception as e:
                logger.error(
                    "[SWEEP] redeem threw for condition=%s: %s",
                    condition_id,
                    e,
                )
                results.append({
                    "condition_id": condition_id,
                    "tx_hash": "",
                    "status": "error",
                    "gas_used": 0,
                    "reason": f"redeem raised: {e}",
                })
                continue
            results.append({
                "condition_id": condition_id,
                "tx_hash": res.get("tx_hash", ""),
                "status": res.get("status", "failed"),
                "gas_used": int(res.get("gas_used", 0)),
            })
        return results

    async def redeem(
        self,
        condition_id: str,
        index_sets: list[int] = [1, 2],
        timeout_seconds: int = 180,
    ) -> dict[str, Any]:
        """Build, sign, and send redemption. Dispatches by signature_type.

        Returns {"tx_hash": str, "status": "success"|"failed"|"timeout", "gas_used": int}.
        """
        if self._signature_type == SIG_TYPE_GNOSIS_SAFE:
            raise NotImplementedError(
                "GNOSIS_SAFE (signature_type=2) redemption is not implemented. "
                "Safe execution requires execTransaction() with owner signatures; "
                "redeem via the Polymarket UI or a Safe client until this path "
                "is added."
            )
        if self._signature_type == SIG_TYPE_POLY_PROXY:
            return await self.redeem_via_proxy(
                condition_id, index_sets, timeout_seconds=timeout_seconds
            )
        return await self._redeem_direct(
            condition_id, index_sets, timeout_seconds=timeout_seconds
        )

    async def _redeem_direct(
        self,
        condition_id: str,
        index_sets: list[int],
        timeout_seconds: int = 180,
    ) -> dict[str, Any]:
        """Direct EOA path: signer calls CTF.redeemPositions. USDC lands
        on the EOA (same as msg.sender)."""
        call = self._build_call(condition_id, index_sets)
        return await self._send_and_wait(
            call,
            to_address=self._ctf_address,
            timeout_seconds=timeout_seconds,
            label="redeemPositions",
            condition_id=condition_id,
        )

    async def redeem_via_proxy(
        self,
        condition_id: str,
        index_sets: list[int] = [1, 2],
        timeout_seconds: int = 180,
    ) -> dict[str, Any]:
        """Proxy-delegated path: EOA signs a tx to ProxyWallet.proxy([...]).

        The proxy becomes msg.sender to CTF.redeemPositions, so the
        outcome tokens it holds are burned and USDC is transferred back
        to the proxy (where trading capital lives)."""
        if self._signature_type != SIG_TYPE_POLY_PROXY:
            raise RuntimeError(
                "redeem_via_proxy requires signature_type=1 (POLY_PROXY); "
                f"got {self._signature_type}"
            )
        if self._proxy_contract is None or self._funder_address is None:
            raise RuntimeError("proxy contract not configured")
        call = self._build_proxy_call(condition_id, index_sets)
        return await self._send_and_wait(
            call,
            to_address=self._funder_address,
            timeout_seconds=timeout_seconds,
            label="ProxyWallet.proxy(redeemPositions)",
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
        """Shared send/sign/wait loop. `call` is a web3 ContractFunction
        already bound to its args; `to_address` is the tx recipient used
        only for logging (build_transaction derives it from `call`)."""

        def _send() -> str:
            try:
                gas_est = call.estimate_gas({"from": self._address})
                gas_limit = int(gas_est * 12 // 10)  # 20% buffer
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

        status_code = int(receipt.get("status", 0)) if isinstance(receipt, dict) else int(getattr(receipt, "status", 0))
        gas_used = int(receipt.get("gasUsed", 0)) if isinstance(receipt, dict) else int(getattr(receipt, "gasUsed", 0))
        status = "success" if status_code == 1 else "failed"
        logger.info(
            "%s receipt: tx=%s status=%s gas_used=%s",
            label, tx_hash, status, gas_used,
        )
        return {"tx_hash": tx_hash, "status": status, "gas_used": gas_used}
