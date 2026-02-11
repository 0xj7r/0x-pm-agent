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
from core.portfolio import Portfolio, PortfolioSnapshot
from core.risk import RiskManager
from models.market import Market, Outcome
from models.trade import Side, Signal, Trade
from strategies.base import Strategy

logger = logging.getLogger(__name__)


class TradingEngine:
    """Core engine that orchestrates scanning, evaluation, and execution."""

    def __init__(self, config: Config) -> None:
        self.config = config
        self.polymarket = PolymarketClient(config)
        self.weather = WeatherClient()
        self.claude = ClaudeClient(config)
        self.risk = RiskManager(config)
        self.memory = MemoryStore(config.DB_PATH)
        self.portfolio = Portfolio()
        self.strategies: list[Strategy] = []
        self._running: bool = False
        self._market_cache: dict[str, Market] = {}

    def register_strategy(self, strategy: Strategy) -> None:
        """Register a trading strategy for evaluation each cycle."""
        self.strategies.append(strategy)
        logger.info(f"Registered strategy: {strategy.name}")

    async def initialize(self) -> None:
        """Set up the agent: fetch balance, load state."""
        balance = await self.polymarket.get_balance()
        if balance <= 0 and self.config.PAPER_TRADE:
            balance = self.config.PAPER_STARTING_BALANCE
            logger.info(f"Paper mode: using starting balance of ${balance:.2f}")
        elif balance <= 0:
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

    async def run(self) -> None:
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

    async def _check_exits(self) -> None:
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

    async def _check_resolutions(self) -> None:
        """Check if any open paper trades have resolved and record P&L."""
        open_trades = self.memory.get_open_trades()
        if not open_trades:
            return

        logger.info(f"Checking resolution for {len(open_trades)} open paper trades...")

        for trade_row in open_trades:
            market_id = trade_row["market_id"]
            try:
                result = await self.polymarket.check_market_resolution(market_id)
            except Exception as e:
                logger.debug(f"Resolution check failed for {market_id}: {e}")
                continue

            if not result:
                continue

            # Market resolved — calculate P&L
            winning_outcome = result["winning_outcome"]  # "Yes" or "No"
            our_outcome = trade_row["outcome"]  # "Yes" or "No"
            side = trade_row["side"]  # "BUY" or "SELL"
            size_usd = trade_row["size_usd"]
            price = trade_row["price"]
            shares = size_usd / price if price > 0 else 0
            question = trade_row["market_question"] or market_id

            if side == "BUY":
                # BUY YES @ 0.25: if YES wins, payout = shares * $1, profit = payout - cost
                # BUY YES @ 0.25: if NO wins, payout = $0, loss = -cost
                won = (our_outcome == winning_outcome)
                if won:
                    payout = shares * 1.0
                    pnl = payout - size_usd
                else:
                    pnl = -size_usd
            else:
                # SELL YES @ 0.75: if NO wins (YES loses), profit = size_usd
                # SELL YES @ 0.75: if YES wins, loss = shares * 1.0 - size_usd
                won = (our_outcome != winning_outcome)
                if won:
                    pnl = size_usd
                else:
                    pnl = -(shares * 1.0 - size_usd)

            # Record in DB
            trade_result = self.memory.mark_trade_resolved(
                trade_id=trade_row["id"],
                market_id=market_id,
                won=won,
                pnl=pnl,
            )

            # Record in portfolio for live snapshot tracking
            self.portfolio.record_result(trade_result)

            status = "WON" if won else "LOST"
            logger.info(
                f"🎯 RESOLVED: {status} ${pnl:+.2f} on [{question[:80]}] "
                f"(bought {our_outcome} @ {price:.3f}, resolved {winning_outcome})"
            )

    async def _run_cycle(self) -> None:
        """Execute a single scan → evaluate → trade cycle."""
        # 0. Refresh market cache and check exits on existing positions
        logger.info("Scanning markets...")
        markets = await self.polymarket.get_all_active_markets()
        logger.info(f"Found {len(markets)} active markets")

        # Cache market lookup for token ID resolution
        self._market_cache = {m.id: m for m in markets}

        # 1b. Check existing positions for exits
        await self._check_exits()

        # 1c. Check if any paper trades have resolved
        # TODO: re-enable after batching API calls — currently kills process
        # await self._check_resolutions()

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

        # 5. Deduplicate — skip markets we already have open trades on
        traded_markets = set()
        if hasattr(self, 'memory') and self.memory:
            try:
                existing = self.memory.get_open_trades()
                traded_markets = {t['market_id'] for t in existing}
            except Exception:
                pass
        # Also check in-memory positions
        traded_markets.update(self.portfolio.positions.keys())

        deduped = [s for s in approved if s.market_id not in traded_markets]
        if len(deduped) < len(approved):
            logger.info(f"Deduped: {len(approved) - len(deduped)} signals skipped (already traded)")

        # 6. Execute trades
        for signal in deduped:
            await self._execute_signal(signal)

    async def _execute_signal(self, signal: Signal) -> None:
        """Size and execute a single trading signal."""
        bankroll = self.portfolio.balance_usd
        if self.config.PAPER_TRADE:
            bankroll = max(bankroll, self.config.PAPER_STARTING_BALANCE)

        size_usd = self.risk.size_position(signal, bankroll)
        if size_usd <= 0:
            logger.debug(f"Position size $0 for {signal.market_question[:50]}")
            return

        # Look up token ID from cached markets, or from signal itself
        market = self._market_cache.get(signal.market_id)
        if market:
            token_id = market.yes_token_id if signal.outcome == Outcome.YES else market.no_token_id
        elif signal.yes_token_id or signal.no_token_id:
            token_id = signal.yes_token_id if signal.outcome == Outcome.YES else signal.no_token_id
        else:
            logger.error(f"Market {signal.market_id} not found in cache and no token IDs on signal")
            return

        if not token_id:
            logger.error(f"No token ID for {signal.outcome.value} on {signal.market_id}")
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

    def _log_snapshot(self, snapshot: PortfolioSnapshot) -> None:
        """Log current portfolio state."""
        logger.info(
            f"Portfolio: ${snapshot.balance_usd:.2f} balance | "
            f"{snapshot.num_open_positions} positions | "
            f"{snapshot.num_trades} trades | "
            f"{snapshot.win_rate:.0%} win rate | "
            f"API cost: ${snapshot.total_api_cost:.4f} | "
            f"Net PnL: ${snapshot.net_pnl:+.2f}"
        )

    def _sync_learnings(self) -> None:
        """Sync trading performance to MEMORY.md for cross-session learning."""
        if not self.config.MEMORY_PATH:
            logger.debug("MEMORY_PATH not set; skipping MEMORY.md sync")
            return

        try:
            self.memory.sync_to_memory_md(self.config.MEMORY_PATH)
        except Exception as e:
            logger.error(f"Failed to sync learnings: {e}")

    async def stop(self) -> None:
        """Graceful shutdown of all resources."""
        self._running = False
        logger.info("Shutting down trading engine...")
        await self.polymarket.close()
        await self.weather.close()
        self.memory.close()
        logger.info("Engine stopped.")
