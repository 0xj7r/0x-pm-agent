"""Polymarket Trading Agent

Autonomous trading agent that scans prediction markets, finds mispricings,
and executes trades using data-driven strategies.

Usage:
    python main.py              # Run in paper trading mode (default)
    python main.py --live       # Run with real money (requires funded wallet)
    python main.py --backtest   # Run backtests against historical data
"""

from __future__ import annotations

import argparse
import asyncio
import logging
import signal
import sys

from config import Config
from core.engine import TradingEngine
from backtesting.engine import BacktestEngine
from clients.weather import WeatherClient
from clients.claude_client import ClaudeClient
from strategies.weather import WeatherStrategy
from strategies.arbitrage import ArbitrageStrategy
from strategies.copy_trading import CopyTradingStrategy


def setup_logging(level: str = "INFO"):
    logging.basicConfig(
        level=getattr(logging, level.upper(), logging.INFO),
        format="%(asctime)s | %(levelname)-8s | %(name)-20s | %(message)s",
        datefmt="%Y-%m-%d %H:%M:%S",
    )


def build_strategies(config: Config, engine: TradingEngine) -> list:
    """Build and register enabled strategies."""
    strategies = []

    if config.ENABLE_WEATHER:
        weather_strategy = WeatherStrategy(
            config=config,
            weather_client=engine.weather,
            claude_client=engine.claude,
        )
        strategies.append(weather_strategy)

    if config.ENABLE_ARBITRAGE:
        arb_strategy = ArbitrageStrategy()
        strategies.append(arb_strategy)

    if config.ENABLE_COPY_TRADING:
        copy_strategy = CopyTradingStrategy()
        strategies.append(copy_strategy)

    return strategies


async def run_live(config: Config):
    """Run the trading agent (paper or live mode)."""
    engine = TradingEngine(config)

    # Register strategies
    strategies = build_strategies(config, engine)
    for strategy in strategies:
        engine.register_strategy(strategy)

    # Handle graceful shutdown
    loop = asyncio.get_event_loop()

    def shutdown_handler():
        logging.info("Received shutdown signal...")
        asyncio.create_task(engine.stop())

    for sig in (signal.SIGINT, signal.SIGTERM):
        try:
            loop.add_signal_handler(sig, shutdown_handler)
        except NotImplementedError:
            # Windows event loops may not implement add_signal_handler.
            signal.signal(sig, lambda *_: asyncio.create_task(engine.stop()))

    mode = "PAPER" if config.PAPER_TRADE else "LIVE"
    logging.info(f"Starting Polymarket Agent in {mode} mode...")
    logging.info(f"Strategies: {[s.name for s in strategies]}")
    logging.info(f"Scan interval: {config.SCAN_INTERVAL_SECONDS}s")

    if not config.PAPER_TRADE:
        logging.warning("LIVE TRADING MODE - Real money at risk!")
        logging.warning(f"Kill balance: ${config.KILL_BALANCE_USD:.2f}")

    try:
        await engine.run()
    finally:
        await engine.stop()


async def run_backtest(config: Config):
    """Run backtests against historical data."""
    bt_engine = BacktestEngine(config)

    # Build strategies for backtesting
    # Note: weather strategy needs clients that work differently in backtest mode
    # For now, backtest the arbitrage strategy which is purely price-based
    arb_strategy = ArbitrageStrategy()

    logging.info("Running backtest for arbitrage strategy...")
    result = await bt_engine.run(
        strategy=arb_strategy,
        initial_balance=100.0,
        max_markets=200,
    )
    print(result.summary())

    await bt_engine.close()


def main():
    parser = argparse.ArgumentParser(description="Polymarket Trading Agent")
    parser.add_argument(
        "--live",
        action="store_true",
        help="Run in live trading mode (real money)",
    )
    parser.add_argument(
        "--backtest",
        action="store_true",
        help="Run backtests against historical data",
    )
    parser.add_argument(
        "--log-level",
        default=None,
        help="Override log level (DEBUG, INFO, WARNING, ERROR)",
    )
    args = parser.parse_args()

    config = Config()

    # CLI overrides
    if args.live:
        config.PAPER_TRADE = False
    if args.log_level:
        config.LOG_LEVEL = args.log_level

    setup_logging(config.LOG_LEVEL)

    # Validate config
    if not config.PAPER_TRADE and not config.PRIVATE_KEY:
        logging.error("POLYMARKET_PRIVATE_KEY required for live trading")
        sys.exit(1)

    if not config.ANTHROPIC_API_KEY:
        logging.error("ANTHROPIC_API_KEY required")
        sys.exit(1)

    if args.backtest:
        asyncio.run(run_backtest(config))
    else:
        asyncio.run(run_live(config))


if __name__ == "__main__":
    main()
