"""Main trading engine: the brain of the agent.

Runs the scan → evaluate → risk check → execute loop.
"""

from __future__ import annotations

import asyncio
import logging
import uuid
from datetime import datetime

from clients.claude_client import ClaudeClient
from clients.polymarket import PolymarketClient
from clients.weather import WeatherClient
from config import Config
from core.memory import MemoryStore
from core.portfolio import Portfolio
from core.risk import RiskManager
from models.market import Outcome
from models.trade import Side, Signal, Trade

logger = logging.getLogger(__name__)


class TradingEngine:
    def __init__(self, config: Config):
        self.config = config
        self.polymarket = PolymarketClient(config)
        self.weather = WeatherClient()
        self.claude = ClaudeClient(config)
        self.risk = RiskManager(config)
        self.memory = MemoryStore(config.DB_PATH)
        self.portfolio = Portfolio()
        self.strategies: list = []
        self._running = False
        self._market_cache: dict[str, object] = {}

    def register_strategy(self, strategy):
        """Register a trading strategy."""
        self.strategies.append(strategy)
        logger.info(f"Registered strategy: {strategy.name}")

    async def initialize(self):
        """Set up the agent: fetch balance, load state."""
        balance = await self.polymarket.get_balance()
        if balance <= 0 and not self.config.PAPER_TRADE:
            logger.warning("No balance detected. Ensure wallet is funded with USDC on Polygon.")
            balance = 0.0

        self.portfolio.balance_usd = balance
        self.risk.set_bankroll(balance)
        mode = "PAPER" if self.config.PAPER_TRADE else "LIVE"
        logger.info(
            f"Agent initialized in {mode} mode | "
            f"Balance: ${balance:.2f} | "
            f"Strategies: {[s.name for s in self.strategies]} | "
            f"Max position: {self.config.MAX_POSITION_PCT * 100:.0f}% | "
            f"Min edge: {self.config.MIN_EDGE_THRESHOLD * 100:.0f}%"
        )

    async def run(self):
        """Main loop: scan, evaluate, trade. Repeat until dead."""
        await self.initialize()
        self._running = True
        cycle = 0

        while self._running:
            cycle += 1
            logger.info(f"\n{'='*60}\nCYCLE {cycle} | {datetime.utcnow().isoformat()}\n{'='*60}")

            try:
                await self._run_cycle()
            except Exception as e:
                logger.error(f"Cycle {cycle} failed: {e}", exc_info=True)

            # Check kill switch
            snapshot = self.portfolio.snapshot()
            if not self.config.PAPER_TRADE and self.risk.should_die(snapshot.balance_usd):
                logger.critical("Agent is dead. Balance too low.")
                self._running = False
                break

            # Save snapshot
            self.memory.save_snapshot(snapshot)
            self._log_snapshot(snapshot)

            # Sync learnings periodically (every 10 cycles)
            if cycle % 10 == 0:
                self._sync_learnings()

            # Wait for next cycle
            logger.info(f"Sleeping {self.config.SCAN_INTERVAL_SECONDS}s until next cycle...")
            await asyncio.sleep(self.config.SCAN_INTERVAL_SECONDS)

    async def _check_exits(self):
        """Check existing positions for exit signals.

        If a position's current fair value exceeds EXIT_THRESHOLD,
        generate a sell signal to close it out.
        """
        exit_signals: list[Signal] = []
        threshold = self.config.EXIT_THRESHOLD

        for pos_key, position in list(self.portfolio.positions.items()):
            # Update current price from market data
            market = self._market_cache.get(position.market_id)
            if not market:
                continue

            if position.outcome == Outcome.YES:
                current_price = market.yes_price
            else:
                current_price = market.no_price

            position.current_price = current_price

            # If fair value (current price) exceeds exit threshold, sell
            if current_price >= threshold:
                signal = Signal(
                    market_id=position.market_id,
                    market_question=position.market_question,
                    outcome=position.outcome,
                    side=Side.SELL,
                    source=position.source,
                    fair_value=current_price,
                    market_price=current_price,
                    edge=current_price - position.avg_price,
                    confidence=0.8,
                    reasoning=(
                        f"EXIT: price {current_price:.3f} >= threshold {threshold:.3f} | "
                        f"Entry: {position.avg_price:.3f} | "
                        f"Unrealized PnL: ${position.unrealized_pnl:+.2f}"
                    ),
                )
                logger.info(
                    f"Exit signal: {position.market_question[:50]} | "
                    f"Price {current_price:.3f} >= {threshold:.3f}"
                )
                exit_signals.append(signal)

        # Execute exit signals
        for signal in exit_signals:
            await self._execute_signal(signal)

    async def _run_cycle(self):
        """Single scan → evaluate → trade cycle."""
        # 0. Refresh market cache and check exits on existing positions
        logger.info("Scanning markets...")
        markets = await self.polymarket.get_all_active_markets()
        logger.info(f"Found {len(markets)} active markets")

        # Cache market lookup for token ID resolution
        self._market_cache = {m.id: m for m in markets}

        # 1b. Check existing positions for exits
        await self._check_exits()

        # 2. Evaluate with each strategy
        all_signals: list[Signal] = []
        for strategy in self.strategies:
            try:
                signals = await strategy.evaluate(markets)
                logger.info(f"[{strategy.name}] produced {len(signals)} signals")
                all_signals.extend(signals)
            except Exception as e:
                logger.error(f"[{strategy.name}] evaluation failed: {e}", exc_info=True)

        if not all_signals:
            logger.info("No signals this cycle")
            return

        # 3. Filter through risk manager (update position count first)
        self.risk.set_open_positions(len(self.portfolio.positions))
        approved = [s for s in all_signals if self.risk.passes_filters(s)]
        logger.info(f"{len(approved)}/{len(all_signals)} signals passed risk filters")

        # 4. Sort by edge (best opportunities first)
        approved.sort(key=lambda s: abs(s.edge), reverse=True)

        # 5. Execute trades
        for signal in approved:
            await self._execute_signal(signal)

    async def _execute_signal(self, signal: Signal):
        """Size and execute a single signal."""
        bankroll = self.portfolio.balance_usd
        if self.config.PAPER_TRADE:
            bankroll = max(bankroll, 100.0)  # paper trading starts with $100 minimum

        size_usd = self.risk.size_position(signal, bankroll)
        if size_usd <= 0:
            logger.debug(f"Position size $0 for {signal.market_question[:50]}")
            return

        # Look up token ID from cached markets
        market = self._market_cache.get(signal.market_id)
        if not market:
            logger.error(f"Market {signal.market_id} not found in cache")
            return

        if signal.outcome == Outcome.YES:
            token_id = market.yes_token_id
        else:
            token_id = market.no_token_id

        if not token_id:
            logger.error(f"No token ID for {signal.outcome.value} on {market.id}")
            return

        price = signal.market_price

        trade = Trade(
            id=str(uuid.uuid4()),
            signal=signal,
            size_usd=size_usd,
            price=price,
            outcome=signal.outcome,
            side=signal.side,
            token_id=token_id,
            paper=self.config.PAPER_TRADE,
            api_cost_usd=0.0,
        )

        if self.config.PAPER_TRADE:
            trade.executed = True
            logger.info(
                f"[PAPER] {signal.side.value} {signal.outcome.value} "
                f"${size_usd:.2f} @ {price:.3f} | "
                f"Edge: {signal.edge:+.3f} | "
                f"{signal.market_question[:60]}"
            )
        else:
            try:
                result = await self.polymarket.place_order(
                    token_id=token_id,
                    side=signal.side.value,
                    price=price,
                    size=size_usd / price if price > 0 else 0,
                )
                trade.executed = True
                trade.order_id = result.get("orderID", "")
                logger.info(
                    f"[LIVE] {signal.side.value} {signal.outcome.value} "
                    f"${size_usd:.2f} @ {price:.3f} | "
                    f"Edge: {signal.edge:+.3f} | "
                    f"Order: {trade.order_id}"
                )
            except Exception as e:
                logger.error(f"Order execution failed: {e}")
                trade.executed = False

        # Record
        self.portfolio.record_trade(trade)
        self.portfolio.record_api_cost(self.claude.total_cost_usd)
        self.memory.save_trade(trade)

    def _log_snapshot(self, snapshot):
        logger.info(
            f"Portfolio: ${snapshot.balance_usd:.2f} balance | "
            f"{snapshot.num_open_positions} positions | "
            f"{snapshot.num_trades} trades | "
            f"{snapshot.win_rate:.0%} win rate | "
            f"API cost: ${snapshot.total_api_cost:.4f} | "
            f"Net PnL: ${snapshot.net_pnl:+.2f}"
        )

    def _sync_learnings(self):
        """Sync trading performance to MEMORY.md."""
        if not self.config.MEMORY_PATH:
            logger.debug("MEMORY_PATH not set; skipping MEMORY.md sync")
            return

        try:
            self.memory.sync_to_memory_md(self.config.MEMORY_PATH)
        except Exception as e:
            logger.error(f"Failed to sync learnings: {e}")

    async def stop(self):
        """Graceful shutdown."""
        self._running = False
        logger.info("Shutting down trading engine...")
        await self.polymarket.close()
        await self.weather.close()
        self.memory.close()
        logger.info("Engine stopped.")
