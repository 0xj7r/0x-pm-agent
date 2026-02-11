"""Binary complement arbitrage strategy.

If YES ask + NO ask < $1.00 on the same market, buying both
guarantees a profit since one MUST resolve to $1.00.

Risk: near-zero (only execution risk).
"""

from __future__ import annotations

import logging

from models.market import Market, Outcome
from models.trade import Side, Signal, SignalSource
from strategies.base import Strategy

logger = logging.getLogger(__name__)

# Minimum profit to bother with (after potential fees/slippage)
MIN_ARB_PROFIT = 0.005  # $0.005 per share pair


class ArbitrageStrategy(Strategy):
    @property
    def name(self) -> str:
        return "arbitrage"

    async def evaluate(self, markets: list[Market]) -> list[Signal]:
        """Find markets where YES + NO < $1.00.

        For each arb opportunity, emits TWO signals (buy YES + buy NO)
        so the engine executes both legs.
        """
        signals: list[Signal] = []

        for market in markets:
            if not market.yes_token_id or not market.no_token_id:
                continue

            arb_profit = market.arb_opportunity
            if arb_profit <= MIN_ARB_PROFIT:
                continue

            yes_ask = market.yes_book.best_ask or market.yes_price
            no_ask = market.no_book.best_ask or market.no_price
            complement_cost = yes_ask + no_ask

            reasoning = (
                f"Binary arb: YES@{yes_ask:.3f} + NO@{no_ask:.3f} = "
                f"${complement_cost:.3f} < $1.00 | "
                f"Guaranteed profit: ${arb_profit:.4f}/share"
            )

            # Emit both legs - engine will execute both
            yes_signal = self._build_arb_signal(
                market=market,
                outcome=Outcome.YES,
                market_price=yes_ask,
                opposite_price=no_ask,
                arb_profit=arb_profit,
                reasoning=reasoning,
            )
            no_signal = self._build_arb_signal(
                market=market,
                outcome=Outcome.NO,
                market_price=no_ask,
                opposite_price=yes_ask,
                arb_profit=arb_profit,
                reasoning=reasoning,
            )

            logger.info(
                f"ARB FOUND: {market.question[:60]} | "
                f"YES@{yes_ask:.3f} + NO@{no_ask:.3f} = ${complement_cost:.3f} | "
                f"Profit: ${arb_profit:.4f}"
            )
            signals.extend([yes_signal, no_signal])

        if signals:
            logger.info(f"Found {len(signals) // 2} arbitrage opportunities")

        return signals

    @staticmethod
    def _build_arb_signal(
        market: Market,
        outcome: Outcome,
        market_price: float,
        opposite_price: float,
        arb_profit: float,
        reasoning: str,
    ) -> Signal:
        return Signal(
            market_id=market.id,
            market_question=market.question,
            outcome=outcome,
            side=Side.BUY,
            source=SignalSource.ARBITRAGE,
            fair_value=1.0 - opposite_price,
            market_price=market_price,
            edge=arb_profit / 2,
            confidence=1.0,
            reasoning=reasoning,
        )
