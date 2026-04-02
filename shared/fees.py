"""Polymarket fee calculations. Single source of truth."""


def taker_fee(price: float) -> float:
    """Polymarket dynamic taker fee rate: 0.072 * p * (1-p).

    Returns the fee as a fraction of the trade size.
    At p=0.50: 1.8%. At p=0.10: 0.65%.
    """
    return 0.072 * price * (1.0 - price)


def taker_fee_usd(price: float, size_usd: float) -> float:
    """Fee in dollars for a given trade size."""
    return size_usd * taker_fee(price)
