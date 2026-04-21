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
from clients.ctf_redeemer import CTFRedeemer
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

    def __init__(self, strategy_cfg: StrategyConfig, db_path: str = "btc_trades.db", coin: str = "btc", market_type: str = "5m") -> None:
        self.cfg = strategy_cfg
        self._coin = coin.lower()
        self._market_type = market_type
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
        self._redeemer: CTFRedeemer | None = None
        self._redeem_drift: bool = False
        if not strategy_cfg.paper.enabled:
            private_key = os.environ.get("POLYMARKET_PRIVATE_KEY", "")
            if not private_key:
                raise RuntimeError(
                    "POLYMARKET_PRIVATE_KEY must be set for live trading"
                )
            base_cfg = Config()
            self._polymarket = PolymarketClient(base_cfg)
            try:
                self._redeemer = CTFRedeemer(
                    web3_provider_url=base_cfg.POLYGON_RPC_URL,
                    private_key=private_key,
                    ctf_address=base_cfg.CTF_ADDRESS,
                    collateral_token_address=base_cfg.COLLATERAL_TOKEN_ADDRESS,
                    chain_id=base_cfg.CHAIN_ID,
                )
            except Exception as e:
                logger.critical(
                    "Failed to initialize CTFRedeemer: %s. Winning positions "
                    "will NOT be auto-redeemed; balance reconciliation will "
                    "drift after market resolution.",
                    e,
                )
                self._redeemer = None
            logger.warning("LIVE TRADING ENABLED: real orders will be placed")

        # Reconstruct balance from event_log: starting_balance + sum of
        # realized pnl_usd across all resolution events. Makes restart
        # balance-persistent without schema changes.
        self.balance: float = strategy_cfg.paper.starting_balance
        try:
            import json as _json
            rows = self.memory.conn.execute(
                "SELECT details FROM event_log WHERE event_type = 'resolution'"
            ).fetchall()
            realized = 0.0
            n = 0
            for (details,) in rows:
                try:
                    d = _json.loads(details)
                    pnl = d.get("pnl_usd")
                    if pnl is not None:
                        realized += float(pnl)
                        n += 1
                except Exception:
                    continue
            if n:
                self.balance = strategy_cfg.paper.starting_balance + realized
                logger.info(
                    f"Restored balance from event_log: "
                    f"${self.balance:,.2f} = "
                    f"${strategy_cfg.paper.starting_balance:.2f} + "
                    f"${realized:+,.2f} realized across {n} resolutions"
                )
        except Exception as e:
            logger.warning(f"Could not restore balance from event_log: {e}")
        self.risk.set_bankroll(self.balance)
        self.risk.set_peak_balance(self.balance)

        self.current_window: MarketWindow | None = None
        self._window_open_price: float = 0.0
        self._current_btc_price: float = 0.0
        # Rolling 60-min price history for regime tagging on each trade.
        # Deque of (timestamp_sec, price). Evicted on each tick.
        from collections import deque
        self._price_history: deque[tuple[float, float]] = deque()
        self._already_traded_this_window: bool = False
        self._running: bool = False
        self._paper_trades: list[dict] = []
        self._scanner: MarketWindowScanner | None = None
        self._scan_interval: float = 30.0
        self._last_scan_time: float = 0.0
        self._latest_book: OrderBookSnapshot | None = None
        self._last_processed_snap: tuple[float, float, float] | None = None
        self._reconcile_halt: bool = False
        self._drift_threshold_usd: float = float(
            getattr(strategy_cfg.risk, "drift_threshold_usd", 2.0)
        )
        self._order_poll_interval_s: float = float(
            getattr(strategy_cfg.risk, "order_poll_interval_seconds", 3.0)
        )
        self._order_fill_deadline_buffer_s: float = float(
            getattr(strategy_cfg.risk, "order_fill_deadline_buffer_seconds", 30.0)
        )

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
        base.MAX_DAILY_TRADES = cfg.risk.max_daily_trades
        base.REJECT_STREAK_LIMIT = cfg.risk.reject_streak_limit
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
        now = _time.time()
        self._price_history.append((now, update.price))
        cutoff = now - 3600
        while self._price_history and self._price_history[0][0] < cutoff:
            self._price_history.popleft()

    def _compute_regime(self) -> dict:
        """60-min vol + trend + categorical regime label at trigger time.

        Returns a dict with:
          vol_60m_bps: stddev of 1-min returns over last 60 min, in bps
          trend_60m_pct: cumulative % move over last 60 min
          regime_label: categorical bucket
          regime_n_samples: how many 1-min price samples contributed
        """
        if len(self._price_history) < 10:
            return {
                "vol_60m_bps": None, "trend_60m_pct": None,
                "regime_label": "insufficient_history",
                "regime_n_samples": len(self._price_history),
            }
        # Downsample to 1-min buckets; keep the last price in each minute.
        buckets: dict[int, float] = {}
        for ts, p in self._price_history:
            buckets[int(ts // 60)] = p
        series = [buckets[k] for k in sorted(buckets.keys())]
        if len(series) < 10:
            return {
                "vol_60m_bps": None, "trend_60m_pct": None,
                "regime_label": "insufficient_history",
                "regime_n_samples": len(series),
            }
        # 1-min log returns, scaled to bps.
        import math
        returns = [math.log(series[i] / series[i - 1]) * 10000
                   for i in range(1, len(series))]
        mean = sum(returns) / len(returns)
        var = sum((r - mean) ** 2 for r in returns) / len(returns)
        vol_bps = var ** 0.5
        trend_pct = (series[-1] - series[0]) / series[0] * 100

        # Regime buckets. Cutoffs are rough priors; refine from data later.
        vol_hi = vol_bps > 8.0  # ~10bps/min = high intraday vol
        trend_up = trend_pct > 0.2
        trend_down = trend_pct < -0.2
        if vol_hi and trend_up:
            label = "high_vol_up"
        elif vol_hi and trend_down:
            label = "high_vol_down"
        elif vol_hi:
            label = "high_vol_flat"
        elif trend_up:
            label = "low_vol_up"
        elif trend_down:
            label = "low_vol_down"
        else:
            label = "low_vol_flat"
        return {
            "vol_60m_bps": vol_bps,
            "trend_60m_pct": trend_pct,
            "regime_label": label,
            "regime_n_samples": len(series),
        }

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
        if self.risk.is_daily_trade_cap_reached():
            return []
        if self.risk.is_reject_streak_tripped():
            return []
        if self._reconcile_halt:
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
        regime = self._compute_regime()
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
            "regime": regime,
        }

        logger.info(
            f"ENTRY [{self._strategy.name.upper()}]: {direction} @ ${token_price:.3f} | "
            f"${size_usd:.2f} ({trade['shares']:.0f} shares) | "
            f"move={move_pct:+.3f}% | BTC=${self._current_btc_price:,.2f}"
        )
        self.persistence.record_entry(self._strategy.name, trade)
        return [trade]

    async def _await_order_fill(
        self,
        order_id: str,
        original_size: float,
        window_end_time: datetime | None,
        now_fn=None,
    ) -> dict:
        """Poll the CLOB for order status until it settles or the window is about to close.

        Returns a dict describing the outcome:
          {
            "status": "matched" | "partial" | "cancelled" | "rejected" | "timeout",
            "size_matched": float,   # shares actually filled, 0 if none
            "last_response": dict | None,
          }

        Polls every self._order_poll_interval_s. If the window end_time is
        known, stops when time-remaining drops below
        self._order_fill_deadline_buffer_s and cancels the order (a timeout).

        The CLOB post_order immediate response uses lowercase status strings
        (matched, live, delayed, unmatched). The GET /data/order/{id}
        response uses uppercase (LIVE, MATCHED, CANCELED, FAILED). We
        accept both, case-insensitively.
        """
        if self._polymarket is None:
            raise RuntimeError("live polling requires polymarket client")

        now_fn = now_fn or (lambda: datetime.now(timezone.utc))

        def _normalize(status: str | None) -> str:
            return (status or "").strip().lower()

        def _fill_size(resp: dict) -> float:
            raw = resp.get("size_matched", resp.get("sizeMatched", 0))
            try:
                return float(raw)
            except (TypeError, ValueError):
                return 0.0

        last_resp: dict | None = None
        while True:
            if window_end_time is not None:
                remaining = (window_end_time - now_fn()).total_seconds()
                if remaining < self._order_fill_deadline_buffer_s:
                    # One last status read so we can record any partial fill
                    # that landed between the previous poll and the deadline.
                    try:
                        final = await self._polymarket.get_order_status(order_id)
                        if isinstance(final, dict):
                            last_resp = final
                    except Exception as e:
                        logger.warning(
                            f"[LIVE] final get_order_status({order_id}) failed: {e}"
                        )
                    try:
                        await self._polymarket.cancel_order(order_id)
                    except Exception as e:
                        logger.warning(
                            f"[LIVE] cancel_order({order_id}) failed: {e}"
                        )
                    filled = _fill_size(last_resp) if last_resp is not None else 0.0
                    if filled > 0 and filled < original_size:
                        return {
                            "status": "partial",
                            "size_matched": filled,
                            "last_response": last_resp,
                        }
                    return {
                        "status": "timeout",
                        "size_matched": filled,
                        "last_response": last_resp,
                    }

            try:
                resp = await self._polymarket.get_order_status(order_id)
            except Exception as e:
                logger.warning(
                    f"[LIVE] get_order_status({order_id}) failed: {e}"
                )
                await asyncio.sleep(self._order_poll_interval_s)
                continue

            last_resp = resp if isinstance(resp, dict) else {}
            status = _normalize(last_resp.get("status"))
            filled = _fill_size(last_resp)

            if status == "matched":
                if filled <= 0:
                    filled = original_size
                if filled + 1e-9 < original_size:
                    # Partial fill on a matched order: cancel the rest
                    # so we don't leave stray open size on book.
                    try:
                        await self._polymarket.cancel_order(order_id)
                    except Exception as e:
                        logger.warning(
                            f"[LIVE] cancel_order({order_id}) on partial "
                            f"failed: {e}"
                        )
                    return {
                        "status": "partial",
                        "size_matched": filled,
                        "last_response": last_resp,
                    }
                return {
                    "status": "matched",
                    "size_matched": filled,
                    "last_response": last_resp,
                }
            if status in ("cancelled", "canceled"):
                return {
                    "status": "cancelled",
                    "size_matched": filled,
                    "last_response": last_resp,
                }
            if status in ("rejected", "failed"):
                return {
                    "status": "rejected",
                    "size_matched": filled,
                    "last_response": last_resp,
                }

            await asyncio.sleep(self._order_poll_interval_s)

    async def _submit_and_confirm_live_order(self, t: dict) -> dict | None:
        """Place a live CLOB order and confirm fill via status polling.

        Returns the (possibly adjusted for partial fill) trade dict on
        successful fill, or None if the order was rejected, cancelled,
        or timed out. On None the caller must NOT record a trade; this
        method already releases the window and bumps reject/success
        counters on risk.
        """
        if self._polymarket is None:
            raise RuntimeError("live order requires polymarket client")

        try:
            result = await self._polymarket.place_order(
                token_id=t["token_id"],
                side="BUY",
                price=t["token_price"],
                size=t["shares"],
            )
        except Exception as e:
            self.risk.record_order_rejection()
            self._already_traded_this_window = False
            logger.error(f"[LIVE] Order placement FAILED: {e}")
            return None

        order_id = (
            result.get("orderID")
            or result.get("orderId")
            or result.get("id")
        )
        if not order_id:
            self.risk.record_order_rejection()
            self._already_traded_this_window = False
            logger.error(
                f"[LIVE] place_order returned no order id: {result}"
            )
            return None

        immediate_status = (result.get("status") or "").strip().lower()
        t["order_id"] = order_id
        t["order_status"] = immediate_status or "posted"
        original_size = float(t["shares"])

        # Fast path: order already fully matched on submit.
        if immediate_status == "matched":
            self.risk.record_order_success()
            logger.warning(
                f"[LIVE] Order matched on submit: id={order_id}"
            )
            return t

        # Slow path: poll until settled or window is about to close.
        window_end = self.current_window.end_time if self.current_window else None
        logger.warning(
            f"[LIVE] Order posted, polling: id={order_id} "
            f"status={immediate_status or 'unknown'}"
        )
        outcome = await self._await_order_fill(
            order_id=order_id,
            original_size=original_size,
            window_end_time=window_end,
        )
        status = outcome["status"]
        filled = float(outcome["size_matched"])

        if status == "matched":
            self.risk.record_order_success()
            t["order_status"] = "matched"
            logger.warning(
                f"[LIVE] Order filled: id={order_id} size={filled}"
            )
            return t

        if status == "partial" and filled > 0:
            # Record only the filled portion. _await_order_fill already
            # cancelled any remaining open size on both paths (matched
            # with partial, and timeout with partial).
            fill_ratio = filled / original_size if original_size > 0 else 0.0
            t["order_status"] = "partial"
            t["shares"] = filled
            t["size_usd"] = float(t["size_usd"]) * fill_ratio
            self.risk.record_order_success()
            logger.warning(
                f"[LIVE] Order PARTIAL fill: id={order_id} "
                f"filled={filled}/{original_size} "
                f"size_usd=${t['size_usd']:.2f}"
            )
            return t

        # Any other terminal state: rejected, cancelled, or timeout.
        self.risk.record_order_rejection()
        self._already_traded_this_window = False
        logger.warning(
            f"[LIVE] Order NOT filled (status={status}): id={order_id}. "
            f"No position recorded. Releasing window."
        )
        return None

    async def _reconcile_live_balance(self) -> None:
        if self.cfg.paper.enabled:
            return
        if self._polymarket is None:
            return
        if len(self._paper_trades) > 0:
            return
        try:
            result = await self._polymarket.get_balance_allowance()
        except Exception as e:
            logger.warning(f"Live balance reconciliation failed: {e}")
            return
        actual = float(result.get("balance_usdc", 0.0))
        expected = float(self.balance)
        diff = abs(actual - expected)
        if diff > self._drift_threshold_usd:
            logger.warning(
                f"LIVE BALANCE DRIFT: actual=${actual:.2f} "
                f"expected=${expected:.2f} diff=${diff:.2f} "
                f"(threshold=${self._drift_threshold_usd:.2f}). Halting entries."
            )
            self._reconcile_halt = True

    async def _scan_for_window(self) -> None:
        now = _time.time()
        if now - self._last_scan_time < self._scan_interval:
            return
        self._last_scan_time = now

        if not self._scanner:
            self._scanner = MarketWindowScanner(coin=self._coin, market_type=self._market_type)
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

            if res.won and self._redeemer is not None:
                await self._redeem_winning_trade(trade)

        self._paper_trades = self.resolver.prune_resolved(self._paper_trades)

    async def _redeem_winning_trade(self, trade: dict) -> None:
        """Call CTF.redeemPositions for a resolved, winning live trade.

        Logs loudly on failure and sets a drift flag for the next
        reconciliation loop to detect. Does not retry here.
        """
        condition_id = trade.get("condition_id")
        if not condition_id:
            logger.error(
                "[REDEEM] No condition_id on won trade market=%s; cannot redeem. "
                "Winning outcome tokens remain as ERC-1155 positions until "
                "manually redeemed.",
                trade.get("market_id"),
            )
            self._redeem_drift = True
            return
        token_id = trade.get("token_id")
        try:
            if token_id:
                bal = await self._redeemer.get_position_balance(token_id)
                if bal == 0:
                    logger.info(
                        "[REDEEM] No on-chain position for token %s... "
                        "(balance=0), skipping redeem for condition %s",
                        str(token_id)[:16],
                        condition_id,
                    )
                    return
        except Exception as e:
            logger.warning(
                "[REDEEM] balanceOf precheck failed for token %s: %s; "
                "proceeding with redeem anyway.",
                str(token_id)[:16] if token_id else "?",
                e,
            )

        try:
            result = await self._redeemer.redeem(condition_id)
        except Exception as e:
            logger.error(
                "[REDEEM] redeemPositions threw for condition %s: %s",
                condition_id,
                e,
            )
            self._redeem_drift = True
            return

        if result.get("status") == "success":
            logger.warning(
                "[REDEEM] success condition=%s tx=%s gas_used=%s",
                condition_id,
                result.get("tx_hash"),
                result.get("gas_used"),
            )
        else:
            logger.error(
                "[REDEEM] FAILED status=%s condition=%s tx=%s. "
                "USDC balance will drift from expected until resolved.",
                result.get("status"),
                condition_id,
                result.get("tx_hash"),
            )
            self._redeem_drift = True

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
                        if not self.cfg.paper.enabled:
                            trade = await self._submit_and_confirm_live_order(t)
                            if trade is None:
                                continue
                            t = trade
                        self._paper_trades.append(t)
                        self.balance -= t["size_usd"]
                        tag = "[PAPER]" if self.cfg.paper.enabled else "[LIVE] "
                        logger.info(f"{tag} Balance: ${self.balance:.2f}")
                        await self.slack.notify_trade(
                            direction=t["direction"], token_price=t["token_price"],
                            size_usd=t["size_usd"], shares=t["shares"],
                            p_win=t["p_win"], btc_price=t["btc_price"],
                            balance=self.balance,
                        )

                    tick_count += 1
                    if tick_count % 3000 == 0:
                        await self._reconcile_live_balance()
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
