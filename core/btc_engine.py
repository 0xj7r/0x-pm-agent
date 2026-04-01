"""BTC 5-minute sniper trading engine.

Orchestrates: Binance WS → Signal Engine → Entry Check → Execution → Persistence.
Delegates resolution to core.resolver, notifications to core.notifier,
health reporting to core.health.
"""
from __future__ import annotations

import asyncio
import logging
import time as _time
from datetime import datetime, timezone

from clients.binance_ws import BinanceWSClient, OrderBookSnapshot, TradeUpdate
from clients.market_scanner import MarketWindowScanner
from clients.polymarket import PolymarketClient
from core.health import HealthServer
from core.memory import MemoryStore
from core.notifier import SlackNotifier
from core.resolver import PaperTradeResolver
from core.risk import RiskManager
from config import Config
from models.market import MarketWindow
from strategies.btc_sniper import BayesianSignalEngine
from strategies.strategy_config import StrategyConfig

logger = logging.getLogger(__name__)

MAX_SINGLE_ORDER_USD = 100.0


class BTCTradingEngine:
    """Core engine for sniping cheap tokens on BTC Up/Down markets."""

    def __init__(self, strategy_cfg: StrategyConfig, db_path: str = "btc_trades.db") -> None:
        self.cfg = strategy_cfg
        self.signal_engine = BayesianSignalEngine(strategy_cfg.signal)
        self.memory = MemoryStore(db_path)
        self.risk = self._init_risk(strategy_cfg)
        self.health = HealthServer()
        self.slack = SlackNotifier()
        self.resolver = PaperTradeResolver()

        self._polymarket: PolymarketClient | None = None
        if not strategy_cfg.paper.enabled:
            private_key = os.environ.get("POLYMARKET_PRIVATE_KEY", "")
            if not private_key:
                raise RuntimeError(
                    "POLYMARKET_PRIVATE_KEY must be set for live trading"
                )
            self._polymarket = PolymarketClient(Config())
            logger.warning("LIVE TRADING ENABLED — real orders will be placed")

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
        self._paper_trades: list[dict] = []
        self._scanner: MarketWindowScanner | None = None
        self._scan_interval: float = 30.0
        self._last_scan_time: float = 0.0
        self._latest_book: OrderBookSnapshot | None = None

    @staticmethod
    def _init_risk(cfg: StrategyConfig) -> RiskManager:
        base = Config()
        base.MAX_POSITION_USD = cfg.risk.max_position_usd
        base.MAX_POSITION_PCT = cfg.risk.max_position_pct
        base.DAILY_LOSS_LIMIT_PCT = cfg.risk.daily_loss_limit_pct
        base.KILL_BALANCE_USD = cfg.risk.kill_balance_usd
        base.MAX_CONCURRENT_POSITIONS = cfg.risk.max_concurrent_positions
        base.LOSS_COOLDOWN_TRADES = cfg.risk.loss_cooldown_trades
        base.LOSS_COOLDOWN_SECONDS = cfg.risk.loss_cooldown_seconds
        return RiskManager(base)

    def _on_new_window(self, window: MarketWindow) -> None:
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
        self._prev_btc_price = self._last_btc_price
        self._last_btc_price = update.price

        if update.is_buy:
            self._buy_volume += update.quantity
        else:
            self._sell_volume += update.quantity

        if not self.current_window or self._window_open_price == 0:
            return

        total_vol = self._buy_volume + self._sell_volume
        ofi = (self._buy_volume - self._sell_volume) / total_vol if total_vol > 0 else 0.0

        price_delta = (update.price - self._window_open_price) / self._window_open_price * 100
        accel = 0.0
        if self._prev_btc_price > 0:
            prev_delta = (self._prev_btc_price - self._window_open_price) / self._window_open_price * 100
            accel = price_delta - prev_delta

        microprice_dev = 0.0
        if self._latest_book and self._latest_book.mid > 0:
            microprice_dev = (self._latest_book.microprice - self._latest_book.mid) / self._latest_book.mid * 100

        self.signal_engine.set_state(
            order_flow_imbalance=ofi,
            microprice_deviation=microprice_dev,
            price_delta=price_delta,
            acceleration=accel,
        )

    def _in_entry_window(self) -> bool:
        if not self.current_window:
            return False
        now = datetime.now(timezone.utc)
        elapsed = self.current_window.elapsed_seconds(now)
        early = self.cfg.execution.entry_window_early
        late = self.cfg.execution.entry_window_late
        if self.cfg.execution.enable_early_snipe and early[0] <= elapsed <= early[1]:
            return True
        if self.cfg.execution.enable_late_snipe and late[0] <= elapsed <= late[1]:
            return True
        return False

    def _check_entry(self) -> list[dict]:
        if not self.current_window or self._already_traded_this_window:
            return []
        if not self.signal_engine.confident:
            return []
        if not self._in_entry_window():
            return []

        direction = self.signal_engine.direction
        if direction == "UP":
            token_price = self.current_window.up_price
            token_id = self.current_window.up_token_id
        else:
            token_price = self.current_window.down_price
            token_id = self.current_window.down_token_id

        if token_price > self.cfg.execution.max_entry_price:
            return []

        p_win = self.signal_engine.p_up if direction == "UP" else self.signal_engine.p_down
        size_usd = self.risk.asymmetric_kelly_size(
            p_win=p_win, token_price=token_price,
            bankroll=self.balance, risk_cfg=self.cfg.risk,
        )
        if size_usd <= 0:
            return []

        self._already_traded_this_window = True
        trade = {
            "market_id": self.current_window.market_id,
            "direction": direction,
            "token_id": token_id,
            "token_price": token_price,
            "size_usd": size_usd,
            "shares": size_usd / token_price,
            "p_win": p_win,
            "log_odds": self.signal_engine.log_odds,
            "btc_price": self._last_btc_price,
            "timestamp": datetime.now(timezone.utc).isoformat(),
        }

        logger.info(
            f"ENTRY: {direction} @ ${token_price:.3f} | "
            f"${size_usd:.2f} ({trade['shares']:.0f} shares) | "
            f"P({direction})={p_win:.3f} | BTC=${self._last_btc_price:,.2f}"
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

    async def _scan_for_window(self) -> None:
        now = _time.time()
        if now - self._last_scan_time < self._scan_interval:
            return
        self._last_scan_time = now

        if not self._scanner:
            self._scanner = MarketWindowScanner()
        try:
            window = await self._scanner.get_current_window()
            if window and (not self.current_window or window.market_id != self.current_window.market_id):
                self._on_new_window(window)
        except Exception as e:
            logger.warning(f"Market scan failed: {e}")

    async def _process_resolutions(self) -> None:
        resolved = await self.resolver.resolve_trades(self._paper_trades)
        for trade, res, resolved_dir in resolved:
            self.balance += trade["size_usd"] + res.pnl_usd
            self.risk.record_trade_result(res.pnl_usd)

            status = "WON" if res.won else "LOST"
            logger.info(
                f"RESOLVED: {status} ${res.pnl_usd:+.2f} | "
                f"{trade['direction']} @ {trade['token_price']:.3f} | "
                f"Outcome: {resolved_dir} | Balance: ${self.balance:.2f}"
            )
            await self.slack.notify_resolution(
                won=res.won, pnl=res.pnl_usd,
                direction=trade["direction"],
                token_price=trade["token_price"],
                resolved_direction=resolved_dir,
                balance=self.balance,
            )
            self.memory.save_event(
                window_id=trade["market_id"],
                event_type="resolution",
                p_up=None, log_odds=None, btc_price=None,
                details={"won": res.won, "pnl_usd": res.pnl_usd,
                         "resolved_direction": resolved_dir, "trade": trade},
            )

        self._paper_trades = self.resolver.prune_resolved(self._paper_trades)

    async def run(self) -> None:
        self._running = True
        await self.health.start()

        async def on_book(snap: OrderBookSnapshot) -> None:
            self._latest_book = snap

        binance = BinanceWSClient(on_trade=self._on_binance_trade, on_book_update=on_book)
        binance_task = asyncio.create_task(binance.connect())

        logger.info(
            f"BTC Sniper started | Paper: {self.cfg.paper.enabled} | "
            f"Balance: ${self.balance:.2f} | Threshold: {self.cfg.signal.confidence_threshold}"
        )
        await self.slack.notify_startup(
            self.balance, self.cfg.signal.confidence_threshold, self.cfg.paper.enabled
        )

        tick_count = 0
        start_time = _time.time()
        try:
            while self._running:
                try:
                    await self._scan_for_window()

                    for t in self._check_entry():
                        self._paper_trades.append(t)
                        if self.cfg.paper.enabled:
                            self.balance -= t["size_usd"]
                            logger.info(f"[PAPER] Balance: ${self.balance:.2f}")
                        await self.slack.notify_trade(
                            direction=t["direction"], token_price=t["token_price"],
                            size_usd=t["size_usd"], shares=t["shares"],
                            p_win=t["p_win"], btc_price=t["btc_price"],
                            balance=self.balance,
                        )

                    tick_count += 1
                    if tick_count % 600 == 0:
                        await self._process_resolutions()
                        total = len(self._paper_trades) + len(self.resolver.resolved_ids)
                        self.health.update(
                            balance=self.balance, trades_total=total,
                            trades_resolved=len(self.resolver.resolved_ids),
                            p_up=self.signal_engine.p_up,
                            binance_connected=binance.seconds_since_last_message < 10,
                            binance_last_msg_age_s=round(binance.seconds_since_last_message, 1),
                            current_window=self.current_window.question if self.current_window else None,
                        )
                        logger.info(
                            f"Status: P(UP)={self.signal_engine.p_up:.3f} | "
                            f"Balance=${self.balance:.2f} | Trades={len(self._paper_trades)} | "
                            f"Resolved={len(self.resolver.resolved_ids)}"
                        )

                    if tick_count % 36000 == 0:
                        uptime_h = (_time.time() - start_time) / 3600
                        total = len(self._paper_trades) + len(self.resolver.resolved_ids)
                        await self.slack.notify_status(
                            balance=self.balance, trades=total,
                            resolved=len(self.resolver.resolved_ids),
                            p_up=self.signal_engine.p_up, uptime_hours=uptime_h,
                        )
                except Exception as e:
                    logger.error(f"Engine tick error: {e}", exc_info=True)
                    self.health.record_error(str(e))
                    await self.slack.notify_error(str(e))

                await asyncio.sleep(0.1)
        finally:
            await self.health.stop()
            await binance.close()
            binance_task.cancel()
            if self._scanner:
                await self._scanner.close()
            await self.resolver.close()
            await self.slack.close()

    async def stop(self) -> None:
        self._running = False
        self.memory.close()
