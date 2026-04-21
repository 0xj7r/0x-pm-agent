"""Polymarket CLOB + Gamma API client.

Wraps py-clob-client for trading and uses Gamma API for market discovery.
All CLOB-authenticated calls go through py-clob-client (which handles HMAC
L2 header signing internally). Do NOT add custom httpx calls against CLOB.
"""

from __future__ import annotations

import json
import logging
from datetime import datetime
from typing import TYPE_CHECKING, Any

import httpx

from config import Config
from models.market import Market, MarketCategory, OrderBook, Outcome, PricePoint

logger = logging.getLogger(__name__)

if TYPE_CHECKING:
    from py_clob_client.client import ClobClient as _ClobClient
    from py_clob_client.clob_types import ApiCreds as _ApiCreds
    from py_clob_client.clob_types import AssetType as _AssetType


def _load_clob_dependencies() -> dict[str, Any]:
    try:
        from py_clob_client.client import ClobClient
        from py_clob_client.clob_types import (
            ApiCreds,
            AssetType,
            BalanceAllowanceParams,
            OrderArgs,
            OrderType,
        )
    except ModuleNotFoundError as exc:
        raise ModuleNotFoundError(
            "py_clob_client is required for Polymarket CLOB access. "
            "Install dependencies from requirements.txt to enable trading."
        ) from exc
    return {
        "ClobClient": ClobClient,
        "ApiCreds": ApiCreds,
        "AssetType": AssetType,
        "BalanceAllowanceParams": BalanceAllowanceParams,
        "OrderArgs": OrderArgs,
        "OrderType": OrderType,
    }


def categorize_market(question: str, description: str) -> MarketCategory:
    text = f"{question} {description}".lower()
    if any(kw in text for kw in ["bitcoin", "ethereum", "crypto", "btc", "eth"]):
        return MarketCategory.CRYPTO
    return MarketCategory.OTHER


def _safe_float(value: Any, default: float = 0.0) -> float:
    try:
        return float(value)
    except (TypeError, ValueError):
        return default


def _parse_iso_datetime(value: str | None) -> datetime | None:
    if not value:
        return None
    try:
        return datetime.fromisoformat(value.replace("Z", "+00:00"))
    except (ValueError, TypeError):
        return None


# USDC has 6 decimals on Polygon; CLOB balance/allowance responses are
# stringified integers in base units.
_USDC_DECIMALS: int = 6
_USDC_UNIT: int = 10**_USDC_DECIMALS


class PolymarketClient:
    def __init__(self, config: Config) -> None:
        self.config = config
        self.gamma_url = config.GAMMA_URL
        self._http = httpx.AsyncClient(timeout=30.0)

        deps = _load_clob_dependencies()
        ClobClient = deps["ClobClient"]

        if config.PRIVATE_KEY:
            self.clob = ClobClient(
                config.CLOB_URL,
                key=config.PRIVATE_KEY,
                chain_id=config.CHAIN_ID,
            )
            # Prefer deriving existing L2 creds for this wallet. Only create
            # fresh creds when derivation fails (raises or returns None),
            # otherwise each restart orphans a new API key.
            creds = self._resolve_api_creds()
            if creds is not None:
                self.clob.set_api_creds(creds)
                logger.info("CLOB client initialized with L2 credentials")
            else:
                logger.warning(
                    "CLOB client has L1 auth but no API credentials were "
                    "resolved; L2 endpoints (orders, balance) will fail"
                )
        else:
            self.clob = ClobClient(config.CLOB_URL, chain_id=config.CHAIN_ID)
            logger.warning(
                "CLOB client initialized in read-only mode (no private key)"
            )

    def _resolve_api_creds(self) -> "_ApiCreds | None":
        """Derive existing API creds; fall back to creating them on failure.

        Idempotent against wallet restarts: `derive_api_key` returns the
        existing creds bound to this wallet+nonce. Only when no creds exist
        yet (derive raises or returns None) do we create.
        """
        try:
            creds = self.clob.derive_api_key()
            if creds is not None:
                return creds
            logger.info(
                "derive_api_key returned None; creating new CLOB API creds"
            )
        except Exception as exc:
            logger.info(
                "derive_api_key failed (%s); creating new CLOB API creds",
                exc,
            )

        try:
            return self.clob.create_api_key()
        except Exception:
            logger.exception("Failed to create CLOB API credentials")
            return None

    async def get_markets(
        self,
        limit: int = 100,
        offset: int = 0,
        active: bool = True,
    ) -> list[Market]:
        """Fetch markets from Gamma API."""
        params = {
            "limit": limit,
            "offset": offset,
            "active": str(active).lower(),
            "closed": "false",
        }
        resp = await self._http.get(f"{self.gamma_url}/markets", params=params)
        resp.raise_for_status()
        raw_markets = resp.json()

        markets: list[Market] = []
        for raw in raw_markets:
            try:
                market = self._parse_market(raw)
                markets.append(market)
            except Exception as e:
                logger.debug(f"Skipping market {raw.get('id', '?')}: {e}")

        return markets

    async def get_all_active_markets(self, max_markets: int = 1000) -> list[Market]:
        """Fetch all active markets, paginating through results."""
        all_markets: list[Market] = []
        offset = 0
        batch_size = 100

        while len(all_markets) < max_markets:
            batch = await self.get_markets(limit=batch_size, offset=offset)
            if not batch:
                break
            all_markets.extend(batch)
            offset += batch_size
            logger.info(f"Fetched {len(all_markets)} markets so far...")

        logger.info(f"Total active markets fetched: {len(all_markets)}")
        return all_markets[:max_markets]

    async def get_order_book(self, token_id: str) -> OrderBook:
        """Fetch order book for a specific token from CLOB."""
        try:
            book = self.clob.get_order_book(token_id)
            raw_bids = getattr(book, "bids", None) or []
            raw_asks = getattr(book, "asks", None) or []
            return OrderBook(
                bids=[
                    PricePoint(price=float(b.price), size=float(b.size))
                    for b in raw_bids
                ],
                asks=[
                    PricePoint(price=float(a.price), size=float(a.size))
                    for a in raw_asks
                ],
            )
        except Exception as e:
            logger.warning(f"Failed to get order book for {token_id}: {e}")
            return OrderBook()

    async def get_price(self, token_id: str) -> float | None:
        """Get current midpoint price for a token."""
        try:
            price_data = self.clob.get_midpoint(token_id)
            return float(price_data.get("mid", 0))
        except Exception:
            return None

    async def place_order(
        self,
        token_id: str,
        side: str,
        price: float,
        size: float,
        fee_rate_bps: int | None = None,
    ) -> dict[str, Any]:
        """Place a GTC limit order on the CLOB.

        Args:
            token_id: The outcome token to trade.
            side: "BUY" or "SELL".
            price: Limit price (0-1).
            size: Number of shares.
            fee_rate_bps: Optional fee override. If None, the market's
                current fee rate is fetched via `get_fee_rate_bps` so the
                caller sees what will actually be charged.

        Returns:
            The raw post_order response dict from CLOB.
        """
        deps = _load_clob_dependencies()
        OrderArgs = deps["OrderArgs"]
        OrderType = deps["OrderType"]

        if fee_rate_bps is None:
            fee_rate_bps = self.clob.get_fee_rate_bps(token_id)
            logger.info(
                "Using market fee_rate_bps=%s for token %s",
                fee_rate_bps,
                token_id[:16],
            )

        order_args = OrderArgs(
            token_id=token_id,
            price=price,
            size=size,
            side=side,
            fee_rate_bps=fee_rate_bps,
        )
        signed_order = self.clob.create_order(order_args)
        result = self.clob.post_order(signed_order, OrderType.GTC)
        logger.info(
            "Order placed: %s %s @ %s for token %s... (fee_bps=%s) -> %s",
            side,
            size,
            price,
            token_id[:16],
            fee_rate_bps,
            result,
        )
        return result

    async def cancel_order(self, order_id: str) -> dict[str, Any]:
        """Cancel an open order."""
        result = self.clob.cancel(order_id)
        logger.info(f"Order cancelled: {order_id}")
        return result

    async def get_order_status(self, order_id: str) -> dict[str, Any]:
        """Fetch the current state of an order (fills, status, etc.)."""
        return self.clob.get_order(order_id)

    async def get_balance_allowance(
        self,
        asset_type: "_AssetType | str | None" = None,
        token_id: str | None = None,
    ) -> dict[str, float]:
        """Fetch USDC balance and Polymarket exchange allowance.

        Args:
            asset_type: COLLATERAL (USDC) or CONDITIONAL (outcome tokens).
                Defaults to COLLATERAL.
            token_id: Required when asset_type is CONDITIONAL.

        Returns:
            {"balance_usdc": float, "allowance_usdc": float} scaled from
            the 6-decimal USDC base units returned by the CLOB.
        """
        deps = _load_clob_dependencies()
        AssetType = deps["AssetType"]
        BalanceAllowanceParams = deps["BalanceAllowanceParams"]

        if asset_type is None:
            asset_type = AssetType.COLLATERAL

        params = BalanceAllowanceParams(
            asset_type=asset_type,
            token_id=token_id,
        )
        resp = self.clob.get_balance_allowance(params)
        balance_raw = resp.get("balance", "0")
        allowance_raw = resp.get("allowance", "0")
        return {
            "balance_usdc": int(balance_raw) / _USDC_UNIT,
            "allowance_usdc": int(allowance_raw) / _USDC_UNIT,
        }

    async def set_allowances(self) -> dict[str, Any]:
        """One-time setup: refresh the server-side view of USDC allowances.

        The CLOB's `update_balance_allowance` endpoint refreshes the
        server-side cache of on-chain allowances for this wallet. It does
        NOT set the allowance amount (that is an on-chain ERC20 `approve`
        that must be issued separately from this client).

        This method is safe to call repeatedly; it is effectively a no-op
        once the wallet already has the necessary approvals.
        """
        deps = _load_clob_dependencies()
        AssetType = deps["AssetType"]
        BalanceAllowanceParams = deps["BalanceAllowanceParams"]

        params = BalanceAllowanceParams(asset_type=AssetType.COLLATERAL)
        result = self.clob.update_balance_allowance(params)
        logger.info("Refreshed collateral balance/allowance: %s", result)
        return result if isinstance(result, dict) else {"result": result}

    def _parse_market(self, raw: dict) -> Market:
        """Parse a raw Gamma API market response into our Market model."""
        question = raw.get("question", "")
        description = raw.get("description", "")

        tokens = raw.get("tokens", [])
        yes_token = ""
        no_token = ""
        yes_price = 0.5
        no_price = 0.5

        for token in tokens:
            outcome = token.get("outcome", "").lower()
            if outcome == "yes":
                yes_token = token.get("token_id", "")
                yes_price = _safe_float(token.get("price"), 0.5)
            elif outcome == "no":
                no_token = token.get("token_id", "")
                no_price = _safe_float(token.get("price"), 0.5)

        return Market(
            id=str(raw.get("id", "")),
            question=question,
            description=description,
            category=categorize_market(question, description),
            end_date=_parse_iso_datetime(raw.get("endDate")),
            active=raw.get("active", True),
            yes_token_id=yes_token,
            no_token_id=no_token,
            yes_price=yes_price,
            no_price=no_price,
            volume=_safe_float(raw.get("volume"), 0.0),
            liquidity=_safe_float(raw.get("liquidity"), 0.0),
            raw=raw,
        )

    async def check_market_resolution(self, market_id: str) -> dict | None:
        """Check if a market has resolved via the Gamma API.

        Args:
            market_id: Gamma market ID (also accepts condition_id).

        Returns {"resolved": True, "winning_outcome": "Yes"|"No"} or None.
        """
        try:
            resp = await self._http.get(
                f"{self.gamma_url}/markets/{market_id}",
            )
            if resp.status_code == 404:
                resp = await self._http.get(
                    f"{self.gamma_url}/markets",
                    params={"condition_id": market_id},
                )
            resp.raise_for_status()
            data = resp.json()

            markets = data if isinstance(data, list) else [data]
            if not markets:
                return None

            market = markets[0]
            if not market.get("closed", False):
                return None

            outcome_prices_raw = market.get("outcomePrices", "[]")
            try:
                prices = (
                    json.loads(outcome_prices_raw)
                    if isinstance(outcome_prices_raw, str)
                    else outcome_prices_raw
                )
            except (json.JSONDecodeError, ValueError):
                return None

            if len(prices) < 2:
                return None

            yes_price = _safe_float(prices[0], 0.0)
            no_price = _safe_float(prices[1], 0.0)

            if yes_price >= 0.99:
                return {"resolved": True, "winning_outcome": "Yes"}
            elif no_price >= 0.99:
                return {"resolved": True, "winning_outcome": "No"}

            return None

        except Exception as e:
            logger.warning(f"Failed to check resolution for {market_id}: {e}")
            return None

    async def close(self) -> None:
        await self._http.aclose()
