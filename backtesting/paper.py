"""Paper trading mode.

Runs the full trading pipeline with real-time data but simulates
execution instead of placing real orders. Perfect for validation
before going live.
"""

from __future__ import annotations

import logging

logger = logging.getLogger(__name__)


# Paper trading is implemented as a flag in the main engine (config.PAPER_TRADE).
# When enabled:
# - Engine fetches real market data
# - Strategies evaluate real markets
# - Risk manager sizes real positions
# - But execution is simulated (no orders placed)
# - Trades are logged to SQLite with paper=True
#
# This file exists as a placeholder for any paper-trading-specific
# utilities we might add later (e.g., simulated fills with slippage,
# simulated order book impact, etc.)


def estimate_fill_with_slippage(
    price: float,
    size_usd: float,
    book_depth: float,
    side: str = "BUY",
) -> float:
    """Estimate realistic fill price accounting for order book impact.

    For small orders relative to book depth, slippage is minimal.
    For large orders, we model linear price impact.
    """
    if book_depth <= 0:
        return price

    # Simple linear impact model
    impact_ratio = size_usd / book_depth
    slippage = impact_ratio * 0.01  # 1% slippage per 100% of book depth

    if side == "BUY":
        return min(price + slippage, 0.99)
    else:
        return max(price - slippage, 0.01)
