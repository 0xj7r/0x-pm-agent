"""BTC 5-minute sniper trading engine.

Orchestrates: Binance WS → Signal Engine → Entry Check → Execution → Persistence.
Runs as a continuous async loop, one iteration per 100ms tick.
"""
from __future__ import annotations

import asyncio
import logging
from datetime import datetime, timezone

from clients.binance_ws import BinanceWSClient, TradeUpdate
from core.memory import MemoryStore
from core.risk import RiskManager
from config import Config
from models.market import MarketWindow
from strategies.btc_sniper import BayesianSignalEngine
from strategies.strategy_config import StrategyConfig

logger = logging.getLogger(__name__)


class BTCTradingEngine:
    """Core engine for sniping cheap tokens on BTC Up/Down markets."""

    def __init__(self, strategy_cfg: StrategyConfig, db_path: str = "btc_trades.db") -> None:
        self.cfg = strategy_cfg
        self.signal_engine = BayesianSignalEngine(strategy_cfg.signal)
        self.memory = MemoryStore(db_path)

        base_config = Config()
        base_config.MAX_POSITION_USD = strategy_cfg.risk.max_position_usd
        base_config.MAX_POSITION_PCT = strategy_cfg.risk.max_position_pct
        base_config.DAILY_LOSS_LIMIT_PCT = strategy_cfg.risk.daily_loss_limit_pct
        base_config.KILL_BALANCE_USD = strategy_cfg.risk.kill_balance_usd
        base_config.MAX_CONCURRENT_POSITIONS = strategy_cfg.risk.max_concurrent_positions
        base_config.LOSS_COOLDOWN_TRADES = strategy_cfg.risk.loss_cooldown_trades
        base_config.LOSS_COOLDOWN_SECONDS = strategy_cfg.risk.loss_cooldown_seconds
        self.risk = RiskManager(base_config)

        self.balance: float = strategy_cfg.paper.starting_balance
        self.risk.set_bankroll(self.balance)

        self.current_window: MarketWindow | None = None
        self._window_open_price: float = 0.0
        self._last_btc_price: float = 0.0
        self._prev_btc_price: float = 0.0
        self._already_traded_this_window: bool = False
        self._running: bool = False

        self._buy_volume: float = 0.0
        self._sell_volume: float = 0.0

    def _on_new_window(self, window: MarketWindow) -> None:
        """Reset state for a new 5-minute market window."""
        self.signal_engine.reset()
        self.current_window = window
        self._window_open_price = self._last_btc_price
        self._already_traded_this_window = False
        self._buy_volume = 0.0
        self._sell_volume = 0.0
        logger.info(
            f"New window: {window.question} | "
            f"BTC open: ${self._window_open_price:,.2f} | "
            f"UP: {window.up_price:.2f} DOWN: {window.down_price:.2f}"
        )

    async def _on_binance_trade(self, update: TradeUpdate) -> None:
        """Process a single Binance trade and update the signal engine."""
        self._prev_btc_price = self._last_btc_price
        self._last_btc_price = update.price

        if update.is_buy:
            self._buy_volume += update.quantity
        else:
            self._sell_volume += update.quantity

        if not self.current_window or self._window_open_price == 0:
            return

        total_vol = self._buy_volume + self._sell_volume
        ofi = 0.0
        if total_vol > 0:
            ofi = (self._buy_volume - self._sell_volume) / total_vol

        microprice_dev = 0.0

        price_delta = (update.price - self._window_open_price) / self._window_open_price * 100
        accel = 0.0
        if self._prev_btc_price > 0:
            prev_delta = (self._prev_btc_price - self._window_open_price) / self._window_open_price * 100
            accel = price_delta - prev_delta

        self.signal_engine.update(
            order_flow_imbalance=ofi,
            microprice_deviation=microprice_dev,
            price_delta=price_delta,
            acceleration=accel,
        )

    def _check_entry(self) -> list[dict]:
        """Check if entry conditions are met. Returns list of paper trades to execute."""
        if not self.current_window or self._already_traded_this_window:
            return []

        if not self.signal_engine.confident:
            return []

        direction = self.signal_engine.direction
        max_price = self.cfg.execution.max_entry_price

        if direction == "UP":
            token_price = self.current_window.up_price
            token_id = self.current_window.up_token_id
        else:
            token_price = self.current_window.down_price
            token_id = self.current_window.down_token_id

        if token_price > max_price:
            return []

        p_win = self.signal_engine.p_up if direction == "UP" else self.signal_engine.p_down
        size_usd = self.risk.asymmetric_kelly_size(
            p_win=p_win,
            token_price=token_price,
            bankroll=self.balance,
            risk_cfg=self.cfg.risk,
        )

        if size_usd <= 0:
            return []

        self._already_traded_this_window = True
        shares = size_usd / token_price

        trade = {
            "market_id": self.current_window.market_id,
            "direction": direction,
            "token_id": token_id,
            "token_price": token_price,
            "size_usd": size_usd,
            "shares": shares,
            "p_win": p_win,
            "log_odds": self.signal_engine.log_odds,
            "btc_price": self._last_btc_price,
            "timestamp": datetime.now(timezone.utc).isoformat(),
        }

        logger.info(
            f"ENTRY: {direction} @ ${token_price:.3f} | "
            f"${size_usd:.2f} ({shares:.0f} shares) | "
            f"P({direction})={p_win:.3f} | "
            f"BTC=${self._last_btc_price:,.2f}"
        )

        self.memory.save_event(
            window_id=self.current_window.market_id,
            event_type="entry",
            log_odds=self.signal_engine.log_odds,
            p_up=self.signal_engine.p_up,
            btc_price=self._last_btc_price,
            details=trade,
        )

        return [trade]

    async def run(self) -> None:
        """Main loop. Connects to Binance, processes data, checks entries."""
        self._running = True

        binance = BinanceWSClient(on_trade=self._on_binance_trade)
        binance_task = asyncio.create_task(binance.connect())

        logger.info(
            f"BTC Sniper started | Paper: {self.cfg.paper.enabled} | "
            f"Balance: ${self.balance:.2f} | "
            f"Threshold: {self.cfg.signal.confidence_threshold}"
        )

        try:
            while self._running:
                try:
                    trades = self._check_entry()
                    for t in trades:
                        if self.cfg.paper.enabled:
                            self.balance -= t["size_usd"]
                            logger.info(f"[PAPER] Balance: ${self.balance:.2f}")
                except Exception as e:
                    logger.error(f"Engine tick error: {e}", exc_info=True)

                await asyncio.sleep(0.1)
        finally:
            await binance.close()
            binance_task.cancel()

    async def stop(self) -> None:
        self._running = False
        self.memory.close()
