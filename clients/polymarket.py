"""Polymarket CLOB + Gamma API client.

Wraps py-clob-client for trading and uses Gamma API for market discovery.
"""

from __future__ import annotations

import json
import logging
from datetime import datetime

import httpx
from py_clob_client.client import ClobClient
from py_clob_client.clob_types import OrderArgs, OrderType

from config import Config
from models.market import Market, MarketCategory, OrderBook, Outcome, PricePoint

logger = logging.getLogger(__name__)

def categorize_market(question: str, description: str) -> MarketCategory:
    text = f"{question} {description}".lower()
    if any(kw in text for kw in ["bitcoin", "ethereum", "crypto", "btc", "eth"]):
        return MarketCategory.CRYPTO
    return MarketCategory.OTHER


def _safe_float(value, default: float = 0.0) -> float:
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


class PolymarketClient:
    def __init__(self, config: Config):
        self.config = config
        self.gamma_url = config.GAMMA_URL
        self._http = httpx.AsyncClient(timeout=30.0)

        # Initialize CLOB client for trading
        if config.PRIVATE_KEY:
            self.clob = ClobClient(
                config.CLOB_URL,
                key=config.PRIVATE_KEY,
                chain_id=config.CHAIN_ID,
            )
            # Derive or set API credentials if provided
            if config.API_KEY:
                self.clob.set_api_creds(
                    self.clob.create_or_derive_api_creds()
                )
            logger.info("CLOB client initialized with trading capabilities")
        else:
            self.clob = ClobClient(config.CLOB_URL, chain_id=config.CHAIN_ID)
            logger.warning("CLOB client initialized in read-only mode (no private key)")

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

        markets = []
        for raw in raw_markets:
            try:
                market = self._parse_market(raw)
                markets.append(market)
            except Exception as e:
                logger.debug(f"Skipping market {raw.get('id', '?')}: {e}")

        return markets

    async def get_all_active_markets(self, max_markets: int = 1000) -> list[Market]:
        """Fetch all active markets, paginating through results."""
        all_markets = []
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
            return OrderBook(
                bids=[
                    PricePoint(price=float(b["price"]), size=float(b["size"]))
                    for b in book.get("bids", [])
                ],
                asks=[
                    PricePoint(price=float(a["price"]), size=float(a["size"]))
                    for a in book.get("asks", [])
                ],
            )
        except Exception as e:
            logger.warning(f"Failed to get order book for {token_id}: {e}")
            return OrderBook()

    async def get_price(self, token_id: str) -> float | None:
        """Get current midpoint price for a token."""
        try:
            price_data = self.clob.get_price(token_id)
            return float(price_data.get("mid", 0))
        except Exception:
            return None

    async def place_order(
        self,
        token_id: str,
        side: str,
        price: float,
        size: float,
    ) -> dict:
        """Place a limit order on the CLOB.

        Args:
            token_id: The outcome token to trade
            side: "BUY" or "SELL"
            price: Limit price (0-1)
            size: Number of shares
        """
        order_args = OrderArgs(
            price=price,
            size=size,
            side=side,
            token_id=token_id,
        )
        signed_order = self.clob.create_order(order_args)
        result = self.clob.post_order(signed_order, OrderType.GTC)
        logger.info(
            f"Order placed: {side} {size} @ {price} for token {token_id[:16]}... "
            f"-> {result}"
        )
        return result

    async def cancel_order(self, order_id: str) -> dict:
        """Cancel an open order."""
        result = self.clob.cancel(order_id)
        logger.info(f"Order cancelled: {order_id}")
        return result

    async def get_balance(self) -> float:
        """Get USDC balance on Polygon. Returns balance in USD."""
        # The CLOB client doesn't expose balance directly.
        # We check via the Gamma API or on-chain.
        try:
            # Try CLOB balance endpoint
            resp = await self._http.get(
                f"{self.config.CLOB_URL}/balance",
                headers=self._get_auth_headers(),
            )
            if resp.status_code == 200:
                return float(resp.json().get("balance", 0))
        except Exception:
            pass

        logger.warning("Could not fetch balance - returning 0")
        return 0.0

    def _get_auth_headers(self) -> dict:
        """Generate auth headers for authenticated endpoints."""
        if hasattr(self.clob, "creds") and self.clob.creds:
            return {
                "POLY_API_KEY": self.clob.creds.api_key,
                "POLY_API_SECRET": self.clob.creds.api_secret,
                "POLY_PASSPHRASE": self.clob.creds.api_passphrase,
            }
        return {}

    def _parse_market(self, raw: dict) -> Market:
        """Parse a raw Gamma API market response into our Market model."""
        question = raw.get("question", "")
        description = raw.get("description", "")

        # Extract token IDs from outcomes/tokens
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
            # Try by ID first, fall back to condition_id
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

            # API may return a single object or a list
            markets = data if isinstance(data, list) else [data]
            if not markets:
                return None

            market = markets[0]
            if not market.get("closed", False):
                return None

            # outcomePrices is a JSON string like '["1","0"]' or '["0","1"]'
            outcome_prices_raw = market.get("outcomePrices", "[]")
            try:
                prices = json.loads(outcome_prices_raw) if isinstance(outcome_prices_raw, str) else outcome_prices_raw
            except (json.JSONDecodeError, ValueError):
                return None

            if len(prices) < 2:
                return None

            yes_price = _safe_float(prices[0], 0.0)
            no_price = _safe_float(prices[1], 0.0)

            # A resolved market has one outcome at 1.0 and the other at 0.0
            if yes_price >= 0.99:
                return {"resolved": True, "winning_outcome": "Yes"}
            elif no_price >= 0.99:
                return {"resolved": True, "winning_outcome": "No"}

            # Market is closed but not cleanly resolved (voided?)
            return None

        except Exception as e:
            logger.warning(f"Failed to check resolution for {market_id}: {e}")
            return None

    async def close(self):
        await self._http.aclose()
