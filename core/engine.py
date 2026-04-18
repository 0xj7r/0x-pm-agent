"""5-minute sniper trading engine (multi-coin).

Orchestrates: Binance WS -> Signal Check -> Entry -> Execution -> Persistence.
Delegates resolution to core.resolver, notifications to core.notifier,
health reporting to core.health.
"""
from __future__ import annotations

import asyncio
import logging
import os
import time as _time
from datetime import datetime, timezone

from clients.binance_ws import BinanceWSClient, OrderBookSnapshot, TradeUpdate
from clients.market_scanner import MarketWindowScanner
from clients.polymarket import PolymarketClient
from clients.polymarket_ws import PolymarketWSClient
from core.health import HealthServer
from core.memory import MemoryStore
from core.notifier import SlackNotifier
from core.resolver import PaperTradeResolver
from core.risk import RiskManager
from core.trade_persistence import TradePersistence
from config import Config
from models.market import MarketWindow
from shared.constants import COIN_CONFIGS
from strategies.live_runtime import LiveRuntimeStrategy
from strategies.strategy_config import StrategyConfig, normalize_coin_config

logger = logging.getLogger(__name__)

MAX_SINGLE_ORDER_USD = 100.0


class BTCTradingEngine:
    """Core engine for sniping cheap tokens on BTC Up/Down markets."""

    def __init__(self, strategy_cfg: StrategyConfig, db_path: str = "btc_trades.db", coin: str = "btc") -> None:
        self.cfg = strategy_cfg
        self._coin = coin.lower()
        self.memory = MemoryStore(db_path)
        self.risk = self._init_risk(strategy_cfg)
        self._supa = None
        try:
            from shared.supabase_client import SupabaseClient
            self._supa = SupabaseClient()
            if not self._supa.health_check():
                raise RuntimeError("Supabase health check failed")
        except Exception as e:
            # Supabase mirroring is only required for live trading. Paper mode
            # persists trades to local SQLite via MemoryStore, which is
            # sufficient for research/validation runs.
            if strategy_cfg.paper.enabled:
                logger.warning(
                    f"Supabase unavailable ({e}); continuing in paper mode with "
                    "SQLite-only persistence."
                )
                self._supa = None
            else:
                logger.critical(
                    f"FATAL: Could not initialize Supabase client: {e}. "
                    "Live trading requires persistence. Check SUPABASE_URL/SUPABASE_KEY."
                )
                raise RuntimeError(f"Supabase unavailable: {e}") from e
        coin_conf = normalize_coin_config(strategy_cfg.coins.get(self._coin))
        strategy_config = {
            "strategy": coin_conf.strategy,
            "params": dict(coin_conf.params),
        }
        self._strategy = LiveRuntimeStrategy.from_config(self._coin, strategy_config)
        self.health = HealthServer(db_path=db_path, supa=self._supa)
        self.slack = SlackNotifier()
        self.resolver = PaperTradeResolver()
        self.poly_ws = PolymarketWSClient()
        self.persistence = TradePersistence(self._coin, self.memory, self._supa)

        self._polymarket: PolymarketClient | None = None
        if not strategy_cfg.paper.enabled:
            private_key = os.environ.get("POLYMARKET_PRIVATE_KEY", "")
            if not private_key:
                raise RuntimeError(
                    "POLYMARKET_PRIVATE_KEY must be set for live trading"
                )
            self._polymarket = PolymarketClient(Config())
            logger.warning("LIVE TRADING ENABLED: real orders will be placed")

        self.balance: float = strategy_cfg.paper.starting_balance
        self.risk.set_bankroll(self.balance)
        self.risk.set_peak_balance(self.balance)

        self.current_window: MarketWindow | None = None
        self._window_open_price: float = 0.0
        self._current_btc_price: float = 0.0
        self._already_traded_this_window: bool = False
        self._running: bool = False
        self._paper_trades: list[dict] = []
        self._scanner: MarketWindowScanner | None = None
        self._scan_interval: float = 30.0
        self._last_scan_time: float = 0.0
        self._latest_book: OrderBookSnapshot | None = None
        self._last_processed_snap: tuple[float, float, float] | None = None

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
        self.current_window = window
        self._window_open_price = self._current_btc_price
        self._already_traded_this_window = False
        self._last_processed_snap = None
        if self._window_open_price > 0:
            self._strategy.start_window(window.market_id, self._window_open_price)
        token_ids = [t for t in [window.up_token_id, window.down_token_id] if t]
        if token_ids:
            try:
                loop = asyncio.get_running_loop()
            except RuntimeError:
                loop = None
            if loop is not None:
                loop.create_task(self.poly_ws.subscribe(token_ids))
        logger.info(
            f"New window: {window.question} | "
            f"BTC open: ${self._window_open_price:,.2f} | "
            f"UP: {window.up_price:.2f} DOWN: {window.down_price:.2f}"
        )

    async def _on_binance_trade(self, update: TradeUpdate) -> None:
        self._current_btc_price = update.price

    def _check_entry(self) -> list[dict]:
        if not self.current_window or self._already_traded_this_window:
            return []
        if self._supa is None and not self.cfg.paper.enabled:
            if not getattr(self, "_logged_no_supa", False):
                logger.error(
                    "BLOCKING ALL TRADES: Supabase client is None in live mode. "
                    "Check SUPABASE_URL/SUPABASE_KEY."
                )
                self._logged_no_supa = True
            return []
        if self._current_btc_price == 0:
            return []
        if self.balance <= self.risk.kill_balance:
            return []
        if self.risk.is_drawdown_breaker_tripped(self.balance):
            return []
        if self.risk.is_rate_limited():
            return []
        if self._window_open_price == 0:
            self._window_open_price = self._current_btc_price
            self._strategy.start_window(self.current_window.market_id, self._window_open_price)
            logger.info(f"Backfilled open price: ${self._window_open_price:,.2f}")
            return []

        # Only trade with live book data, never stale fallbacks
        has_live_up = self.poly_ws.has_live_book(self.current_window.up_token_id)
        has_live_down = self.poly_ws.has_live_book(self.current_window.down_token_id)
        if not has_live_up or not has_live_down:
            return []

        move_pct = (self._current_btc_price - self._window_open_price) / self._window_open_price * 100

        price_up = self.poly_ws.get_price(self.current_window.up_token_id)
        price_down = self.poly_ws.get_price(self.current_window.down_token_id)

        current_snap = (self._current_btc_price, price_up, price_down)
        if self._last_processed_snap == current_snap:
            return []
        self._last_processed_snap = current_snap

        current_hour = datetime.now(timezone.utc).hour
        signal = self._strategy.check_signal(
            self.current_window.market_id,
            self._window_open_price,
            current_snap,
            current_hour=current_hour,
        )
        snap_count = self._strategy.num_snaps

        if signal is not None and signal != "SKIP":
            logger.info(
                f"SIGNAL: {signal} | move={move_pct:+.3f}% | UP={price_up:.2f} DOWN={price_down:.2f} | "
                f"snaps={snap_count}"
            )
        elif abs(move_pct) >= 0.04 and snap_count > 0 and snap_count % 20 == 0:
            logger.info(
                f"Checking: move={move_pct:+.3f}% | UP={price_up:.2f} DOWN={price_down:.2f} | "
                f"signal={signal} | snaps={snap_count}"
            )

        if signal is None or signal == "SKIP":
            return []

        direction = signal.upper()
        if direction == "UP":
            token_id = self.current_window.up_token_id
            token_price = price_up
        else:
            token_id = self.current_window.down_token_id
            token_price = price_down

        size_usd = min(
            self.balance * self.cfg.risk.max_position_pct,
            self.cfg.risk.max_position_usd,
        )
        if size_usd < 1.0:
            return []

        self._already_traded_this_window = True
        self.risk.record_trade_entry()
        trade = {
            "market_id": self.current_window.market_id,
            "direction": direction,
            "token_id": token_id,
            "token_price": token_price,
            "size_usd": size_usd,
            "shares": size_usd / token_price,
            "p_win": None,
            "btc_price": self._current_btc_price,
            "move_pct": move_pct,
            "strategy": self._strategy.name,
            "timestamp": datetime.now(timezone.utc).isoformat(),
        }

        logger.info(
            f"ENTRY [{self._strategy.name.upper()}]: {direction} @ ${token_price:.3f} | "
            f"${size_usd:.2f} ({trade['shares']:.0f} shares) | "
            f"move={move_pct:+.3f}% | BTC=${self._current_btc_price:,.2f}"
        )
        self.persistence.record_entry(self._strategy.name, trade)
        return [trade]

    async def _scan_for_window(self) -> None:
        now = _time.time()
        if now - self._last_scan_time < self._scan_interval:
            return
        self._last_scan_time = now

        if not self._scanner:
            self._scanner = MarketWindowScanner(coin=self._coin)
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
            self.risk.update_peak_balance(self.balance)

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
            self.persistence.record_resolution(self._strategy.name, trade, res, resolved_dir)

        self._paper_trades = self.resolver.prune_resolved(self._paper_trades)

    def set_strategy(self, strategy: LiveRuntimeStrategy) -> None:
        self._strategy = strategy
        if self.current_window and self._window_open_price > 0:
            self._strategy.start_window(self.current_window.market_id, self._window_open_price)

    async def run(self) -> None:
        self._running = True
        await self.health.start()

        async def on_book(snap: OrderBookSnapshot) -> None:
            self._latest_book = snap

        binance_symbol = COIN_CONFIGS.get(self._coin, COIN_CONFIGS["btc"])["binance_symbol"]
        binance = BinanceWSClient(on_trade=self._on_binance_trade, on_book_update=on_book, symbol=binance_symbol)
        binance_task = asyncio.create_task(binance.connect())
        poly_ws_task = asyncio.create_task(self.poly_ws.connect())

        logger.info(
            f"BTC Sniper started | Paper: {self.cfg.paper.enabled} | "
            f"Balance: ${self.balance:.2f} | "
            f"Strategy: {self._strategy.name} {self._strategy.params}"
        )
        await self.slack.notify_startup(
            self.balance, self._strategy.params.get("move", 0.08), self.cfg.paper.enabled
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

                        up_live = self.poly_ws.get_price(self.current_window.up_token_id) if self.current_window else 0
                        down_live = self.poly_ws.get_price(self.current_window.down_token_id) if self.current_window else 0
                        self.persistence.record_status(
                            window_id=self.current_window.market_id if self.current_window else "none",
                            btc_price=self._current_btc_price,
                            balance=self.balance,
                            trades_total=total,
                            up_price=up_live,
                            down_price=down_live,
                        )

                        if self._supa and self._supa._dlq:
                            self._supa.flush_dlq()

                        self.health.update(
                            balance=self.balance, trades_total=total,
                            trades_resolved=len(self.resolver.resolved_ids),
                            p_up=None,
                            binance_connected=binance.seconds_since_last_message < 10,
                            binance_last_msg_age_s=round(binance.seconds_since_last_message, 1),
                            current_window=self.current_window.question if self.current_window else None,
                        )
                        logger.info(
                            f"Status: Balance=${self.balance:.2f} | "
                            f"Trades={len(self._paper_trades)} | "
                            f"Resolved={len(self.resolver.resolved_ids)}"
                        )

                    if tick_count % 36000 == 0:
                        uptime_h = (_time.time() - start_time) / 3600
                        total = len(self._paper_trades) + len(self.resolver.resolved_ids)
                        await self.slack.notify_status(
                            balance=self.balance, trades=total,
                            resolved=len(self.resolver.resolved_ids),
                            p_up=None, uptime_hours=uptime_h,
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
            await self.poly_ws.close()
            poly_ws_task.cancel()
            if self._scanner:
                await self._scanner.close()
            await self.resolver.close()
            await self.slack.close()

    async def stop(self) -> None:
        self._running = False
        self.persistence.close()
