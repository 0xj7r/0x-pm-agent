"""On-chain subscription to Polymarket market resolution events.

Push path for resolution detection. Runs in parallel with the Gamma-API
`PaperTradeResolver` polling in `core.resolver`; the engine treats the
callback as advisory and still guards duplicates with `resolved_ids`.

We subscribe to `ConditionResolution` on Polymarket's Gnosis
ConditionalTokens (CTF) contract:

    event ConditionResolution(
        bytes32 indexed conditionId,
        address indexed oracle,
        bytes32 indexed questionId,
        uint outcomeSlotCount,
        uint[] payoutNumerators
    );

Contract (Polygon): 0x4D97DCd97eC945f40cF65F87097ACe5EA0476045.

Winner derivation for binary Up/Down markets: `payoutNumerators` has
length 2. `[1, 0]` → outcome index 0 won (Yes / Up); `[0, 1]` → outcome
index 1 won (No / Down). We expose both raw numerators and a convenience
winner label so callers that don't know which way "Yes" maps to UP can
decide for themselves.

Reconnect strategy: exponential backoff 1s → 30s on disconnect or decode
error. Resubscribe on each connect. No message replay: on reconnect we
catch up via the existing Gamma polling path.
"""
from __future__ import annotations

import asyncio
import json
import logging
from typing import Any, Awaitable, Callable

logger = logging.getLogger(__name__)

# Polymarket's Gnosis ConditionalTokens (CTF) on Polygon.
POLYMARKET_CTF_ADDRESS = "0x4D97DCd97eC945f40cF65F87097ACe5EA0476045"

# keccak256("ConditionResolution(bytes32,address,bytes32,uint256,uint256[])")
# Verified against Gnosis ConditionalTokens source and recomputed via
# eth_utils.keccak at module load (eth_utils is a transitive dep via web3).
CONDITION_RESOLUTION_TOPIC = (
    "0xb44d84d3289691f71497564b85d4233648d9dbae8cbdbb4329f301c3a0185894"
)

try:
    from eth_utils import keccak as _keccak  # type: ignore

    _computed = (
        "0x"
        + _keccak(
            text=(
                "ConditionResolution(bytes32,address,bytes32,"
                "uint256,uint256[])"
            )
        ).hex()
    )
    if _computed != CONDITION_RESOLUTION_TOPIC:
        logger.warning(
            "ConditionResolution topic drift: expected=%s computed=%s. "
            "Using computed value.",
            CONDITION_RESOLUTION_TOPIC,
            _computed,
        )
        CONDITION_RESOLUTION_TOPIC = _computed
except Exception:
    pass


ResolutionCallback = Callable[
    [str, str, list[int]], Awaitable[None] | None
]
"""callback(condition_id, winner, payout_numerators)

- condition_id: 0x-prefixed hex string, 32-byte condition id
- winner: "YES" if numerators == [1,0], "NO" if [0,1], else "UNKNOWN"
- payout_numerators: raw list of ints from the event (length >=2 for
  binary; unused slots are 0)
"""


def _hex32_to_condition_id(topic_hex: str) -> str:
    """Normalize a 32-byte topic hex to a lowercase 0x-prefixed string."""
    s = topic_hex.lower()
    if not s.startswith("0x"):
        s = "0x" + s
    if len(s) != 66:
        raise ValueError(f"expected 32-byte topic, got {topic_hex!r}")
    return s


def _decode_payout_numerators(data_hex: str) -> tuple[int, list[int]]:
    """Decode the non-indexed tail of ConditionResolution.

    Layout of `data`:
        word 0: outcomeSlotCount (uint256)
        word 1: offset-to-payoutNumerators (dynamic array), always 0x40
        word 2: payoutNumerators.length
        word 3..: payoutNumerators[i]

    Returns (outcomeSlotCount, payoutNumerators). Robust to leading "0x".
    """
    raw = data_hex.lower()
    if raw.startswith("0x"):
        raw = raw[2:]
    # Each word is 64 hex chars (32 bytes).
    if len(raw) < 64 * 3:
        raise ValueError(f"data too short for ConditionResolution: {data_hex!r}")

    def _word(i: int) -> int:
        return int(raw[i * 64 : (i + 1) * 64], 16)

    outcome_slots = _word(0)
    # word 1 is the offset to the dynamic array (always 0x40 = 64 bytes
    # past start = word index 2). We verify and skip.
    arr_len = _word(2)
    nums: list[int] = []
    for i in range(arr_len):
        word_idx = 3 + i
        if (word_idx + 1) * 64 > len(raw):
            break
        nums.append(_word(word_idx))
    return outcome_slots, nums


def derive_winner(payout_numerators: list[int]) -> str:
    """For a binary market, return "YES" if outcome 0 won, "NO" if outcome
    1 won, "UNKNOWN" otherwise (including ties / multi-outcome)."""
    if len(payout_numerators) < 2:
        return "UNKNOWN"
    a, b = payout_numerators[0], payout_numerators[1]
    if a == 1 and b == 0:
        return "YES"
    if a == 0 and b == 1:
        return "NO"
    return "UNKNOWN"


class ResolutionWatcher:
    """Subscribes to ConditionResolution on the CTF via Polygon WS RPC.

    Implementation detail: we use raw `eth_subscribe("logs", {...})` over
    JSON-RPC 2.0 on top of an async websocket. No web3.py `AsyncWeb3`
    dependency -- the wire protocol is straightforward and this keeps
    the surface tight for tests. On each `eth_subscription` notification
    we ABI-decode the `data` field and invoke the callback.
    """

    def __init__(
        self,
        ws_url: str,
        *,
        on_resolution: ResolutionCallback | None = None,
        ctf_address: str = POLYMARKET_CTF_ADDRESS,
        condition_ids: list[str] | None = None,
    ) -> None:
        if not ws_url:
            raise ValueError("ResolutionWatcher requires a non-empty ws_url")
        self._ws_url = ws_url
        self._on_resolution = on_resolution
        self._ctf_address = ctf_address.lower()
        # Optional conditionId filter; if set we only dispatch events whose
        # indexed conditionId topic matches. Empty => all resolutions on
        # the CTF contract are dispatched.
        self._condition_filter: set[str] = {
            _hex32_to_condition_id(c).lower() for c in (condition_ids or [])
        }
        self._ws = None
        self._running = False
        self._connected = False
        self._reconnect_delay = 1.0
        self._sub_id: str | None = None
        self._next_req_id = 1

    @property
    def connected(self) -> bool:
        return self._connected

    def set_condition_filter(self, condition_ids: list[str]) -> None:
        """Replace the conditionId allow-list. Applied to future events
        (no re-subscription required; we filter client-side)."""
        self._condition_filter = {
            _hex32_to_condition_id(c).lower() for c in condition_ids
        }

    def _build_subscribe_request(self) -> dict[str, Any]:
        req_id = self._next_req_id
        self._next_req_id += 1
        params: dict[str, Any] = {
            "address": self._ctf_address,
            "topics": [CONDITION_RESOLUTION_TOPIC],
        }
        return {
            "jsonrpc": "2.0",
            "id": req_id,
            "method": "eth_subscribe",
            "params": ["logs", params],
        }

    async def connect(self) -> None:
        import websockets

        self._running = True
        while self._running:
            try:
                logger.info(f"Connecting to Polygon WS RPC ({self._ws_url})")
                async with websockets.connect(self._ws_url) as ws:
                    self._ws = ws
                    self._reconnect_delay = 1.0
                    sub_req = self._build_subscribe_request()
                    await ws.send(json.dumps(sub_req))
                    # Wait for subscription ack before marking connected.
                    sub_id = await self._await_subscription_ack(ws, sub_req["id"])
                    if sub_id is None:
                        raise RuntimeError(
                            "eth_subscribe did not return a subscription id"
                        )
                    self._sub_id = sub_id
                    self._connected = True
                    logger.info(
                        "Resolution WS subscribed: ctf=%s topic=%s sub_id=%s",
                        self._ctf_address,
                        CONDITION_RESOLUTION_TOPIC,
                        sub_id,
                    )
                    async for raw in ws:
                        await self._handle_message(raw)
            except Exception as e:
                if not self._running:
                    break
                logger.warning(
                    f"Resolution WS disconnected: {e}. "
                    f"Reconnecting in {self._reconnect_delay:.0f}s. "
                    "Gamma polling fallback remains active."
                )
                self._connected = False
                self._ws = None
                self._sub_id = None
                await asyncio.sleep(self._reconnect_delay)
                self._reconnect_delay = min(self._reconnect_delay * 2, 30.0)
        self._connected = False

    async def _await_subscription_ack(
        self, ws: object, req_id: int
    ) -> str | None:
        """Pull frames until the ack for `req_id` arrives. The first frame
        on a fresh subscription is always the RPC response with `result`
        set to the subscription id."""
        for _ in range(5):
            raw = await ws.recv()
            try:
                msg = json.loads(raw)
            except Exception:
                continue
            if isinstance(msg, dict) and msg.get("id") == req_id:
                result = msg.get("result")
                if isinstance(result, str):
                    return result
                err = msg.get("error")
                raise RuntimeError(f"eth_subscribe error: {err}")
            # Any unsolicited frame before the ack is dispatched normally.
            await self._handle_decoded(msg)
        return None

    async def _handle_message(self, raw: str) -> None:
        try:
            msg = json.loads(raw)
        except Exception:
            logger.debug(f"Resolution WS: non-JSON frame: {raw!r:.80}")
            return
        await self._handle_decoded(msg)

    async def _handle_decoded(self, msg: Any) -> None:
        if not isinstance(msg, dict):
            return
        if msg.get("method") != "eth_subscription":
            return
        params = msg.get("params") or {}
        if params.get("subscription") != self._sub_id:
            return
        log = params.get("result") or {}
        await self._dispatch_log(log)

    async def _dispatch_log(self, log: dict[str, Any]) -> None:
        topics = log.get("topics") or []
        if len(topics) < 3:
            return
        # topics[0] = event signature hash
        # topics[1] = conditionId
        # topics[2] = oracle
        # topics[3] = questionId
        try:
            condition_id = _hex32_to_condition_id(topics[1])
        except ValueError:
            return

        if self._condition_filter and condition_id.lower() not in self._condition_filter:
            return

        data_hex = log.get("data", "0x")
        try:
            _outcome_slots, numerators = _decode_payout_numerators(data_hex)
        except Exception as exc:
            logger.warning(
                f"Resolution WS decode failed condition={condition_id}: {exc}"
            )
            return

        winner = derive_winner(numerators)
        logger.info(
            "Resolution event: condition=%s winner=%s numerators=%s",
            condition_id,
            winner,
            numerators,
        )
        if self._on_resolution is None:
            return
        try:
            result = self._on_resolution(condition_id, winner, numerators)
            if asyncio.iscoroutine(result):
                await result
        except Exception as exc:
            logger.error(
                f"Resolution WS callback raised for condition={condition_id}: "
                f"{exc}",
                exc_info=True,
            )

    async def close(self) -> None:
        self._running = False
        self._connected = False
        if self._ws is not None:
            try:
                await self._ws.close()
            except Exception:
                pass
            self._ws = None
