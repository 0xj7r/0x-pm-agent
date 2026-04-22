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
from uuid import uuid4

from clients.binance_ws import BinanceWSClient, OrderBookSnapshot, TradeUpdate
from clients.ctf_redeemer import CTFRedeemer
from clients.market_scanner import MarketWindowScanner
from clients.polymarket import PolymarketClient
from clients.polymarket_user_ws import PolymarketUserWS
from clients.polymarket_ws import PolymarketWSClient
from clients.resolution_watcher import ResolutionWatcher, derive_winner
from core.btc_resolution import PaperTradeRecord, resolve_paper_trade
from core.health import HealthServer
from core.kelly import (
    empirical_kelly_size,
    estimate_price_bucket_p_win,
    smoothed_bucket_estimate,
)
from core.memory import MemoryStore
from core.notifier import SlackNotifier
from core.resolver import PaperTradeResolver
from core.risk import RejectReason, RiskManager
from core.trade_persistence import TradePersistence
from config import Config
from models.market import MarketWindow
from shared.constants import COIN_CONFIGS
from shared.fees import taker_fee_usd
from strategies.live_runtime import LiveRuntimeStrategy
from strategies.strategy_config import StrategyConfig, normalize_coin_config

logger = logging.getLogger(__name__)

MAX_SINGLE_ORDER_USD = 100.0

# Default cross-the-spread slippage (USD per share) added to BUY price so
# live orders actually fill. Single source of truth shared by paper and
# live; overridable via RiskConfig.live_entry_slippage_usd.
LIVE_ENTRY_SLIPPAGE_USD = 0.01

# Hard cap: never quote a BUY above 0.99 even after slippage.
MAX_BUY_PRICE = 0.99

# A valid window open must be anchored to an exchange trade very close to the
# Polymarket window start. If the bot starts mid-window, do not fabricate an
# open from the current price; skip that window instead.
WINDOW_OPEN_ANCHOR_MAX_LAG_S = 5.0


def _compact_dict(d: dict | None, keys: list[str]) -> dict:
    if not isinstance(d, dict):
        return {}
    return {k: d.get(k) for k in keys if k in d}


def _classify_reject_reason(text: str) -> RejectReason:
    t = (text or "").lower()

    if "not enough balance" in t:
        return RejectReason.INSUFFICIENT_FUNDS
    if "insufficient" in t and "balance" in t:
        return RejectReason.INSUFFICIENT_FUNDS
    if "insufficient funds" in t:
        return RejectReason.INSUFFICIENT_FUNDS

    if "unauthorized" in t:
        return RejectReason.AUTH_FAILURE
    if "signature" in t:
        return RejectReason.AUTH_FAILURE

    if "too many requests" in t or "rate limit" in t or "429" in t:
        return RejectReason.RATE_LIMITED

    if "lower than the minimum" in t:
        return RejectReason.MIN_SHARES
    if "minimum" in t and ("size" in t or "shares" in t):
        return RejectReason.MIN_SHARES

    if "price" in t and "cross" in t:
        return RejectReason.PRICE_CROSSED

    return RejectReason.UNKNOWN


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
            if strategy_cfg.paper.enabled or os.environ.get("ALLOW_LIVE_WITHOUT_SUPABASE") == "1":
                logger.warning(
                    f"Supabase unavailable ({e}); continuing with SQLite-only "
                    "persistence. event_log is the source of truth."
                )
                self._supa = None
            else:
                logger.critical(
                    f"FATAL: Could not initialize Supabase client: {e}. "
                    "Live trading requires persistence. Check SUPABASE_URL/SUPABASE_KEY "
                    "or set ALLOW_LIVE_WITHOUT_SUPABASE=1 to bypass."
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
        self._redeem_sweep_interval_s: int = int(
            getattr(strategy_cfg.risk, "redeem_sweep_interval_seconds", 300) or 300
        )
        self._redeem_blind_when_token_missing: bool = bool(
            getattr(strategy_cfg.risk, "redeem_blind_when_token_missing", True)
        )
        self._last_redeem_sweep_ts: float = 0.0
        self._last_take_profit_ts_by_market: dict[str, float] = {}
        self._last_stop_loss_ts_by_market: dict[str, float] = {}
        self._window_open_skip_logged_market_id: str | None = None

        # Push-path state (parallel to polling). Keys are orderIDs; the
        # asyncio.Event fires as soon as a trade/order event on the user
        # WS channel reports MATCHED / CANCELED / UNMATCHED / FAILED for
        # that order. Size is mirrored into `_push_fill_sizes` so the
        # poller can pick up partial fills reported via push without a
        # redundant REST call.
        self._push_order_events: dict[str, asyncio.Event] = {}
        self._push_order_snapshots: dict[str, dict] = {}
        self._user_ws: PolymarketUserWS | None = None
        self._resolution_ws: ResolutionWatcher | None = None
        # Condition IDs seen on push path this session. Advisory; the
        # resolver still owns `resolved_ids` for idempotency against
        # `event_log` writes.
        self._push_resolved_condition_ids: set[str] = set()

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
                    signature_type=base_cfg.POLYMARKET_SIGNATURE_TYPE,
                    funder_address=base_cfg.POLYMARKET_FUNDER,
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
        self._trade_volume_history: deque[tuple[float, float]] = deque()
        self._already_traded_this_window: bool = False
        self._window_submitted: bool = False
        self._window_confirmed_fill: bool = False
        self._running: bool = False
        self._open_trades: list[dict] = self._query_open_entries()
        if self._open_trades:
            logger.warning(
                "Rehydrated %d open trade(s) from event_log",
                len(self._open_trades),
            )
        self._scanner: MarketWindowScanner | None = None
        self._scan_interval: float = 30.0
        self._last_scan_time: float = 0.0
        self._latest_book: OrderBookSnapshot | None = None
        self._last_processed_snap: tuple[float, float, float] | None = None
        self._reconcile_halt: bool = False
        self._drift_threshold_usd: float = float(
            getattr(strategy_cfg.risk, "drift_threshold_usd", 2.0)
        )
        self._live_usdc_anchor: float | None = None
        self._live_internal_balance_anchor: float | None = None
        self._order_poll_interval_s: float = float(
            getattr(strategy_cfg.risk, "order_poll_interval_seconds", 3.0)
        )
        self._order_fill_deadline_buffer_s: float = float(
            getattr(strategy_cfg.risk, "order_fill_deadline_buffer_seconds", 30.0)
        )
        self._live_entry_slippage_usd: float = float(
            getattr(strategy_cfg.risk, "live_entry_slippage_usd", LIVE_ENTRY_SLIPPAGE_USD)
        )
        self._paired_paper_enabled: bool = bool(
            getattr(strategy_cfg.risk, "paired_paper_enabled", False)
        )
        self._window_open_ts: float | None = None

    def _book_snapshot(self, token_id: str) -> dict:
        book = self.poly_ws.get_book(token_id)
        if book is None:
            return {"token_id": token_id, "ok": False}
        age_ms = max(0, int((_time.time() - float(book.last_update or 0.0)) * 1000))
        return {
            "ok": True,
            "token_id": token_id,
            "best_bid": float(book.best_bid or 0.0),
            "best_bid_size": float(getattr(book, "best_bid_size", 0.0) or 0.0),
            "best_ask": float(book.best_ask or 0.0),
            "best_ask_size": float(getattr(book, "best_ask_size", 0.0) or 0.0),
            "spread": float(book.spread or 0.0),
            "age_ms": age_ms,
        }

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
        base.REJECT_COOLDOWN_SECONDS = getattr(cfg.risk, "reject_cooldown_seconds", 60)
        base.REJECT_STREAK_COOLDOWN_SECONDS = getattr(
            cfg.risk, "reject_streak_cooldown_seconds", 600
        )
        return RiskManager(base)

    async def _on_user_ws_event(self, evt: dict) -> None:
        """Callback for PolymarketUserWS. Non-blocking.

        Maps trade / order events onto `_push_order_events` so the poller
        in `_await_order_fill` wakes up as soon as a push arrives. The
        snapshot stored on `_push_order_snapshots[order_id]` uses the
        same field names as the REST `get_order` response (status,
        size_matched) so the poller's normalization works either way.
        """
        evt_type = evt.get("event_type")
        if evt_type == "trade":
            # `taker_order_id` is our order (we always submit as taker
            # for sniping). Status lifecycle: MATCHED -> MINED -> CONFIRMED.
            order_id = evt.get("taker_order_id")
            size = evt.get("size")
            status = (evt.get("status") or "").upper()
            if not order_id:
                return
            # Only treat as "settled" once the chain has confirmed. Earlier
            # statuses still signal partial progress; store snapshot.
            try:
                size_f = float(size) if size is not None else 0.0
            except (TypeError, ValueError):
                size_f = 0.0
            snap = {
                "id": order_id,
                "status": "MATCHED" if status in ("MATCHED", "MINED", "CONFIRMED") else status,
                "size_matched": str(size_f),
                "source": "push",
            }
            self._push_order_snapshots[order_id] = snap
            ev = self._push_order_events.get(order_id)
            if ev is not None and status in ("MATCHED", "MINED", "CONFIRMED"):
                ev.set()
        elif evt_type == "order":
            order_id = evt.get("id")
            if not order_id:
                return
            op = (evt.get("type") or "").upper()
            status = (evt.get("status") or "").upper()
            snap = {
                "id": order_id,
                "status": status or op,
                "size_matched": evt.get("size_matched", "0"),
                "source": "push",
            }
            self._push_order_snapshots[order_id] = snap
            ev = self._push_order_events.get(order_id)
            if ev is None:
                return
            if op == "CANCELLATION" or status in (
                "CANCELED", "CANCELLED", "UNMATCHED", "FAILED", "REJECTED"
            ):
                ev.set()

    async def _on_resolution_push(
        self, condition_id: str, winner: str, numerators: list[int]
    ) -> None:
        """Callback for ResolutionWatcher. Idempotent against Gamma polling.

        The authoritative writer of resolution records stays
        `_process_resolutions` (via `PaperTradeResolver`). Here we only
        pre-seed `resolver.resolved_ids` so the next poll tick short-circuits
        into resolution path immediately, and annotate any open trades
        with the confirmed conditionId for downstream CTF redemption.
        """
        cid_norm = condition_id.lower()
        if cid_norm in self._push_resolved_condition_ids:
            return
        self._push_resolved_condition_ids.add(cid_norm)

        # Fast-path: if the next polling tick is 60s away we still want
        # to be able to resolve. The resolver uses market_id (Gamma id)
        # as the dedup key, not conditionId, so we can only force a
        # resolution by calling through the Gamma path next tick. What
        # we CAN do now: mark the trade and kick the loop.
        matched_any = False
        for t in self._open_trades:
            if not t.get("condition_id") and t.get("market_id"):
                # We do not yet have a conditionId on the trade; without
                # the Gamma lookup we can't map push conditionId -> trade.
                # Store pending_push for visibility in logs.
                continue
            if t.get("condition_id", "").lower() == condition_id.lower():
                t["pending_push_resolution"] = True
                t["push_resolution_winner"] = winner
                matched_any = True

        if matched_any:
            logger.warning(
                "[PUSH] Resolution event matched %d open trade(s) for "
                "condition=%s winner=%s. Awaiting next poll tick to record.",
                sum(1 for t in self._open_trades
                    if t.get("condition_id", "").lower() == condition_id.lower()),
                condition_id,
                winner,
            )
        else:
            logger.info(
                "[PUSH] Resolution event: condition=%s winner=%s (no matching "
                "open trade)",
                condition_id,
                winner,
            )

    def _on_new_window(self, window: MarketWindow) -> None:
        self.current_window = window
        self._window_open_price = 0.0
        self._window_open_ts = None
        self._window_open_skip_logged_market_id = None
        self._already_traded_this_window = False
        self._window_submitted = False
        self._window_confirmed_fill = False
        self._last_processed_snap = None
        self._anchor_window_open_from_history()
        token_ids = [t for t in [window.up_token_id, window.down_token_id] if t]
        if token_ids:
            try:
                loop = asyncio.get_running_loop()
            except RuntimeError:
                loop = None
            if loop is not None:
                loop.create_task(self.poly_ws.subscribe(token_ids))
        open_label = (
            f"${self._window_open_price:,.2f}"
            if self._window_open_price > 0
            else "pending Binance start anchor"
        )
        logger.info(
            f"New window: {window.question} | "
            f"BTC open: {open_label} | "
            f"UP: {window.up_price:.2f} DOWN: {window.down_price:.2f}"
        )

    async def _on_binance_trade(self, update: TradeUpdate) -> None:
        self._current_btc_price = update.price
        event_ts = (
            update.timestamp_ms / 1000.0
            if update.timestamp_ms
            else _time.time()
        )
        self._price_history.append((event_ts, update.price))
        cutoff = event_ts - 3600
        while self._price_history and self._price_history[0][0] < cutoff:
            self._price_history.popleft()
        self._trade_volume_history.append((event_ts, update.quantity))
        volume_cutoff = event_ts - 60
        while self._trade_volume_history and self._trade_volume_history[0][0] < volume_cutoff:
            self._trade_volume_history.popleft()
        self._anchor_window_open_from_history()

    def _btc_volume_60s(self) -> float:
        return float(sum(qty for _, qty in self._trade_volume_history))

    def _anchor_window_open_from_history(self) -> bool:
        """Set the current window open from the first Binance trade at start.

        This keeps paper/live decisions aligned around the same Polymarket
        market start. A process that starts mid-window lacks that anchor and
        must skip entries until the next window, rather than treating its first
        observed price as the open.
        """
        if self.current_window is None:
            return False
        if self._window_open_price > 0:
            return True

        start_ts = self.current_window.start_time.timestamp()
        end_ts = self.current_window.end_time.timestamp()
        for ts, price in self._price_history:
            if ts < start_ts:
                continue
            if ts >= end_ts:
                break
            lag_s = ts - start_ts
            if lag_s > WINDOW_OPEN_ANCHOR_MAX_LAG_S:
                market_id = self.current_window.market_id
                if self._window_open_skip_logged_market_id != market_id:
                    logger.warning(
                        "Skipping window %s until next rollover: first Binance "
                        "trade after start is %.3fs late (max %.3fs)",
                        market_id,
                        lag_s,
                        WINDOW_OPEN_ANCHOR_MAX_LAG_S,
                    )
                    self._window_open_skip_logged_market_id = market_id
                return False
            if price <= 0:
                return False
            self._window_open_price = float(price)
            self._window_open_ts = float(ts)
            self._strategy.start_window(
                self.current_window.market_id,
                self._window_open_price,
            )
            logger.info(
                "Anchored window open: market=%s open=$%.2f lag_ms=%d",
                self.current_window.market_id,
                self._window_open_price,
                int(lag_s * 1000),
            )
            return True
        return False

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
        if not self.current_window or self._already_traded_this_window or self._window_submitted:
            return []
        if (self._supa is None and not self.cfg.paper.enabled
                and os.environ.get("ALLOW_LIVE_WITHOUT_SUPABASE") != "1"):
            if not getattr(self, "_logged_no_supa", False):
                logger.error(
                    "BLOCKING ALL TRADES: Supabase client is None in live mode. "
                    "Check SUPABASE_URL/SUPABASE_KEY or set "
                    "ALLOW_LIVE_WITHOUT_SUPABASE=1 to bypass."
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
            self._anchor_window_open_from_history()
        if self._window_open_price == 0:
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
            raw_token_price = price_up
        else:
            token_id = self.current_window.down_token_id
            raw_token_price = price_down

        # Enforce max_entry on the *submit* price too. Otherwise slippage can
        # bypass the strategy gate (raw <= max_entry, but raw+slip > max_entry),
        # causing live to take the worst risk/reward corner that the config
        # intended to exclude.
        max_entry_price = float(self._strategy.params.get("max_entry", MAX_BUY_PRICE))
        effective_token_price = min(
            raw_token_price + self._live_entry_slippage_usd,
            max_entry_price,
            MAX_BUY_PRICE,
        )
        empirical_kelly_enabled = bool(getattr(self.cfg.risk, "empirical_kelly_enabled", False))
        p_win_estimate = None
        if empirical_kelly_enabled:
            p_win_estimate = estimate_price_bucket_p_win(self.memory, effective_token_price)
            # Cold-start: no resolutions in this bucket yet. Synthesize an
            # empty estimate so the prior still drives sizing instead of
            # falling back to flat.
            if p_win_estimate is None:
                from core.kelly import PriceBucketEstimate, bucket_for_price
                p_win_estimate = PriceBucketEstimate(
                    bucket=bucket_for_price(effective_token_price),
                    wins=0, losses=0, p_win=0.0,
                )

        if p_win_estimate is None:
            size_usd = min(
                self.balance * self.cfg.risk.max_position_pct,
                self.cfg.risk.max_position_usd,
            )
        else:
            p_win_estimate = smoothed_bucket_estimate(
                p_win_estimate,
                effective_token_price,
                float(getattr(self.cfg.risk, "empirical_kelly_prior_weight", 1.0)),
                float(getattr(self.cfg.risk, "empirical_kelly_prior_edge", 0.05)),
            )
            size_usd = empirical_kelly_size(
                p_win=p_win_estimate.p_win,
                token_price=effective_token_price,
                bankroll=self.balance,
                risk_cfg=self.cfg.risk,
            )

        if size_usd < 1.0:
            if p_win_estimate is not None:
                logger.info(
                    "SKIP: empirical Kelly size is zero | bucket=%s p_win=%.3f samples=%d price=%.3f",
                    p_win_estimate.bucket,
                    p_win_estimate.p_win,
                    p_win_estimate.samples,
                    effective_token_price,
                )
            return []

        # Polymarket CLOB rejects live orders below its per-market minimum
        # share count (typically 5). Paper mode has no such constraint —
        # keep paper behavior unchanged so historical paper stats remain
        # apples-to-apples with prior sessions.
        paper_livelike = bool(getattr(self.cfg.risk, "paper_livelike_enabled", False))
        if (not self.cfg.paper.enabled) or paper_livelike:
            min_shares = float(getattr(self.cfg.risk, "min_shares", 5.0))
            shares_at_size = size_usd / effective_token_price
            if shares_at_size < min_shares:
                needed_usd = min_shares * effective_token_price
                cap_usd = self.cfg.risk.max_position_usd * 1.5
                if empirical_kelly_enabled:
                    logger.info(
                        f"SKIP: Kelly size ${size_usd:.2f} gives shares {shares_at_size:.2f} "
                        f"< min_shares={min_shares}; not scaling beyond Kelly"
                    )
                    return []
                elif needed_usd <= cap_usd and needed_usd <= self.balance * self.cfg.risk.max_position_pct * 1.5:
                    logger.info(
                        f"Scaling size ${size_usd:.2f} -> ${needed_usd:.2f} to meet "
                        f"min_shares={min_shares} at price ${effective_token_price:.3f}"
                    )
                    size_usd = needed_usd
                else:
                    logger.info(
                        f"SKIP: shares {shares_at_size:.2f} < min {min_shares} "
                        f"and needed ${needed_usd:.2f} exceeds 1.5x cap"
                    )
                    return []

        self._already_traded_this_window = True
        self._window_submitted = True
        self.risk.record_trade_entry()
        regime = self._compute_regime()
        decision_ts = datetime.now(timezone.utc).isoformat()
        decision_id = f"{self._coin}-{self._strategy.name}-{self.current_window.market_id}-{uuid4().hex}"
        book_snapshots = {
            "up": self._book_snapshot(self.current_window.up_token_id),
            "down": self._book_snapshot(self.current_window.down_token_id),
            "selected": self._book_snapshot(token_id),
        }
        trade = {
            "decision_id": decision_id,
            "market_id": self.current_window.market_id,
            "direction": direction,
            "token_id": token_id,
            "token_price": effective_token_price,
            "raw_token_price": raw_token_price,
            "size_usd": size_usd,
            "shares": size_usd / effective_token_price,
            "p_win": p_win_estimate.p_win if p_win_estimate is not None else None,
            "p_win_bucket": p_win_estimate.bucket if p_win_estimate is not None else None,
            "p_win_samples": p_win_estimate.samples if p_win_estimate is not None else 0,
            "btc_price": self._current_btc_price,
            "move_pct": move_pct,
            "window_open_price": self._window_open_price,
            "window_open_ts": self._window_open_ts,
            "window_start_ts": self.current_window.start_time.timestamp(),
            "strategy": self._strategy.name,
            "timestamp": decision_ts,
            "regime": regime,
            "book": book_snapshots,
            "book_snapshot": book_snapshots,
            "btc_volume_60s": self._btc_volume_60s(),
        }

        logger.info(
            f"ENTRY [{self._strategy.name.upper()}]: {direction} @ ${effective_token_price:.3f} "
            f"(raw=${raw_token_price:.3f} +slip=${self._live_entry_slippage_usd:.3f}) | "
            f"${size_usd:.2f} ({trade['shares']:.0f} shares) | "
            f"move={move_pct:+.3f}% | BTC=${self._current_btc_price:,.2f} | "
            f"p_win={trade['p_win'] if trade['p_win'] is not None else 'n/a'}"
        )
        # NOTE: persistence.record_entry is deliberately NOT called here.
        # Moving the write to the async caller AFTER fill confirmation
        # prevents phantom event_log entries for orders that never fill
        # (timeouts, cancels, rejections). The caller records on paper
        # append or after _submit_and_confirm_live_order returns non-None.
        return [trade]

    async def _simulate_paper_live_like_fill(self, t: dict) -> dict | None:
        """Paper-mode simulation of live fill constraints.

        - Re-checks best ask after a small latency; if ask > submit price,
          treat as price-cross (no fill).
        - Optionally caps filled shares to best-ask size (partial fill).

        Returns adjusted trade dict on fill, or None on no-fill.
        """
        latency_ms = int(getattr(self.cfg.risk, "paper_livelike_latency_ms", 150) or 0)
        use_best_ask_size = bool(
            getattr(self.cfg.risk, "paper_livelike_use_best_ask_size", True)
        )
        if latency_ms > 0:
            await asyncio.sleep(latency_ms / 1000)

        token_id = t.get("token_id") or ""
        if not token_id:
            self.risk.record_order_rejection(
                reason=RejectReason.UNKNOWN,
                message="paper livelike: missing token_id",
            )
            self._already_traded_this_window = False
            self._window_submitted = False
            return None

        submit_price = float(t.get("token_price") or 0.0)
        desired_shares = float(t.get("shares") or 0.0)
        if submit_price <= 0 or desired_shares <= 0:
            self.risk.record_order_rejection(
                reason=RejectReason.UNKNOWN,
                message="paper livelike: invalid submit price/size",
            )
            self._already_traded_this_window = False
            self._window_submitted = False
            return None

        current_ask = float(self.poly_ws.get_price(token_id) or 0.0)
        if current_ask <= 0:
            self.risk.record_order_rejection(
                reason=RejectReason.UNKNOWN,
                message="paper livelike: no live ask",
            )
            self._already_traded_this_window = False
            self._window_submitted = False
            return None

        if current_ask > submit_price + 1e-9:
            self.risk.record_order_rejection(
                reason=RejectReason.PRICE_CROSSED,
                message=f"paper livelike: ask {current_ask:.4f} > submit {submit_price:.4f}",
            )
            self._already_traded_this_window = False
            self._window_submitted = False
            return None

        filled_shares = desired_shares
        if use_best_ask_size:
            book = self.poly_ws.get_book(token_id)
            ask_size = float(getattr(book, "best_ask_size", 0.0) or 0.0) if book else 0.0
            if ask_size > 0 and ask_size + 1e-9 < desired_shares:
                filled_shares = ask_size
                fill_ratio = filled_shares / desired_shares
                t["order_status"] = "partial_paper"
                t["shares"] = filled_shares
                t["size_usd"] = float(t["size_usd"]) * fill_ratio
                self.risk.record_order_success()
                return t

        t["order_status"] = "matched_paper"
        self.risk.record_order_success()
        return t

    async def _sleep_or_push(self, push_event: asyncio.Event, seconds: float) -> None:
        """Sleep up to `seconds`, but return early if push_event fires.

        Matches the semantics of the old `asyncio.sleep(interval)` so the
        polling loop keeps its 3s cadence as a fallback; user-WS pushes
        collapse latency to sub-100ms in the healthy case.
        """
        try:
            await asyncio.wait_for(push_event.wait(), timeout=seconds)
        except asyncio.TimeoutError:
            return
        # Clear so the next iteration can race again on a subsequent event
        # (e.g. a trade fires MATCHED then MINED then CONFIRMED).
        push_event.clear()

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

        # Register a push-event slot for this order. A trade/order WS
        # event sets the event as soon as it reports MATCHED / CANCELED
        # / UNMATCHED / FAILED. The first iteration below calls get_order
        # once; subsequent iterations race push vs 3s sleep.
        push_event = self._push_order_events.setdefault(
            order_id, asyncio.Event()
        )

        last_resp: dict | None = None
        push_won: bool = False
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

            # Prefer a push snapshot if one arrived since the last loop.
            # This makes the first iteration and every iteration thereafter
            # free of a REST call when push is healthy.
            push_snap = self._push_order_snapshots.pop(order_id, None)
            if push_snap is not None:
                push_won = True
                resp: dict | None = push_snap
            else:
                try:
                    resp = await self._polymarket.get_order_status(order_id)
                except Exception as e:
                    logger.warning(
                        f"[LIVE] get_order_status({order_id}) failed: {e}"
                    )
                    await self._sleep_or_push(
                        push_event, self._order_poll_interval_s
                    )
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
                    self._push_order_events.pop(order_id, None)
                    self._push_order_snapshots.pop(order_id, None)
                    if push_won:
                        logger.info(
                            f"[PUSH] fill detected via user WS order_id={order_id}"
                        )
                    return {
                        "status": "partial",
                        "size_matched": filled,
                        "last_response": last_resp,
                    }
                self._push_order_events.pop(order_id, None)
                self._push_order_snapshots.pop(order_id, None)
                if push_won:
                    logger.info(
                        f"[PUSH] fill detected via user WS order_id={order_id}"
                    )
                return {
                    "status": "matched",
                    "size_matched": filled,
                    "last_response": last_resp,
                }
            if status in ("cancelled", "canceled"):
                self._push_order_events.pop(order_id, None)
                self._push_order_snapshots.pop(order_id, None)
                return {
                    "status": "cancelled",
                    "size_matched": filled,
                    "last_response": last_resp,
                }
            if status in ("rejected", "failed"):
                self._push_order_events.pop(order_id, None)
                self._push_order_snapshots.pop(order_id, None)
                return {
                    "status": "rejected",
                    "size_matched": filled,
                    "last_response": last_resp,
                }

            await self._sleep_or_push(
                push_event, self._order_poll_interval_s
            )

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

        # Ensure a stable id for this execution attempt so order/exit/final
        # events can be joined later even if no fill occurs.
        self.persistence.ensure_trade_id(self._strategy.name, t)
        order_attempt_id = uuid4().hex
        t["order_attempt_id"] = order_attempt_id

        # t["token_price"] already includes the live-entry slippage bump
        # applied in _check_entry, capped at MAX_BUY_PRICE. We still
        # clamp defensively in case t was constructed elsewhere.
        submit_price = min(float(t["token_price"]), MAX_BUY_PRICE)
        submit_started = _time.time()
        submit_book = self._book_snapshot(str(t.get("token_id") or ""))
        try:
            result = await self._polymarket.place_order(
                token_id=t["token_id"],
                side="BUY",
                price=submit_price,
                size=t["shares"],
            )
        except Exception as e:
            msg = str(e)
            self.risk.record_order_rejection(
                reason=_classify_reject_reason(msg),
                message=msg,
            )
            self._already_traded_this_window = False
            self._window_submitted = False
            self.persistence.record_order_submit(
                market_id=t.get("market_id", "unknown"),
                payload={
                    "trade_id": t.get("id"),
                    "decision_id": t.get("decision_id"),
                    "order_attempt_id": order_attempt_id,
                    "side": "BUY",
                    "token_id": t.get("token_id"),
                    "submit_price": submit_price,
                    "shares": t.get("shares"),
                    "size_usd": t.get("size_usd"),
                    "btc_price": t.get("btc_price"),
                    "submit_book": submit_book,
                    "error": msg[:800],
                    "submit_ts": datetime.now(timezone.utc).isoformat(),
                },
            )
            logger.error(f"[LIVE] Order placement FAILED: {e}")
            return None

        order_id = (
            result.get("orderID")
            or result.get("orderId")
            or result.get("id")
        )
        if not order_id:
            msg = f"place_order returned no order id: {result}"
            self.risk.record_order_rejection(
                reason=_classify_reject_reason(msg),
                message=msg,
            )
            self._already_traded_this_window = False
            self._window_submitted = False
            self.persistence.record_order_submit(
                market_id=t.get("market_id", "unknown"),
                payload={
                    "trade_id": t.get("id"),
                    "decision_id": t.get("decision_id"),
                    "order_attempt_id": order_attempt_id,
                    "side": "BUY",
                    "token_id": t.get("token_id"),
                    "submit_price": submit_price,
                    "shares": t.get("shares"),
                    "size_usd": t.get("size_usd"),
                    "btc_price": t.get("btc_price"),
                    "submit_book": submit_book,
                    "result": _compact_dict(result, ["status", "error", "message", "fee_rate_bps"]),
                    "submit_ts": datetime.now(timezone.utc).isoformat(),
                },
            )
            logger.error(
                f"[LIVE] place_order returned no order id: {result}"
            )
            return None

        immediate_status = (result.get("status") or "").strip().lower()
        t["order_id"] = order_id
        t["order_status"] = immediate_status or "posted"
        t["fee_rate_bps"] = result.get("fee_rate_bps")
        t["fee_bps_ceiling"] = result.get("fee_rate_bps")
        t["submit_price"] = submit_price
        t["submit_book"] = submit_book
        original_size = float(t["shares"])
        self.persistence.record_order_submit(
            market_id=t.get("market_id", "unknown"),
            payload={
                "trade_id": t.get("id"),
                "decision_id": t.get("decision_id"),
                "order_attempt_id": order_attempt_id,
                "order_id": order_id,
                "side": "BUY",
                "token_id": t.get("token_id"),
                "submit_price": submit_price,
                "shares": original_size,
                "size_usd": t.get("size_usd"),
                "btc_price": t.get("btc_price"),
                "submit_book": submit_book,
                "result": _compact_dict(
                    result,
                    ["status", "fee_rate_bps", "orderID", "orderId", "id"],
                ),
                "submit_ts": datetime.now(timezone.utc).isoformat(),
            },
        )

        # Fast path: order already fully matched on submit.
        if immediate_status == "matched":
            self.risk.record_order_success()
            self._window_confirmed_fill = True
            t["fill_details"] = {
                "order_id": order_id,
                "status": "matched",
                "filled_shares": original_size,
                "original_shares": original_size,
                "latency_ms": int((_time.time() - submit_started) * 1000),
                "fee_bps_ceiling": t.get("fee_bps_ceiling"),
                "submit_book": submit_book,
                "submit_price": submit_price,
                "response": _compact_dict(
                    result,
                    [
                        "status",
                        "fee_rate_bps",
                        "orderID",
                        "orderId",
                        "id",
                        "takingAmount",
                        "makingAmount",
                        "transactionHash",
                        "transactionHashes",
                    ],
                ),
            }
            logger.warning(
                f"[LIVE] Order matched on submit: id={order_id}"
            )
            self.persistence.record_order_final(
                market_id=t.get("market_id", "unknown"),
                payload={
                    "trade_id": t.get("id"),
                    "decision_id": t.get("decision_id"),
                    "order_attempt_id": order_attempt_id,
                    "order_id": order_id,
                    "final_status": "matched",
                    "filled_shares": original_size,
                    "btc_price": t.get("btc_price"),
                    "final_ts": datetime.now(timezone.utc).isoformat(),
                    "latency_ms": int((_time.time() - submit_started) * 1000),
                },
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
        last_resp_compact = _compact_dict(
            outcome.get("last_response") if isinstance(outcome, dict) else None,
            ["status", "size_matched", "sizeMatched", "price", "avgPrice"],
        )

        if status == "matched":
            self.risk.record_order_success()
            self._window_confirmed_fill = True
            t["order_status"] = "matched"
            t["filled_shares"] = filled
            t["fill_details"] = {
                "order_id": order_id,
                "status": status,
                "filled_shares": filled,
                "original_shares": original_size,
                "latency_ms": int((_time.time() - submit_started) * 1000),
                "fee_bps_ceiling": t.get("fee_bps_ceiling"),
                "submit_book": submit_book,
                "submit_price": submit_price,
                "response": _compact_dict(
                    outcome.get("last_response") if isinstance(outcome, dict) else None,
                    [
                        "status",
                        "size_matched",
                        "sizeMatched",
                        "price",
                        "avgPrice",
                        "takingAmount",
                        "makingAmount",
                        "transactionHash",
                        "transactionHashes",
                    ],
                ),
            }
            self.persistence.record_order_final(
                market_id=t.get("market_id", "unknown"),
                payload={
                    "trade_id": t.get("id"),
                    "decision_id": t.get("decision_id"),
                    "order_attempt_id": order_attempt_id,
                    "order_id": order_id,
                    "final_status": status,
                    "filled_shares": filled,
                    "original_shares": original_size,
                    "btc_price": t.get("btc_price"),
                    "last_response": last_resp_compact,
                    "final_ts": datetime.now(timezone.utc).isoformat(),
                    "latency_ms": int((_time.time() - submit_started) * 1000),
                },
            )
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
            self._window_confirmed_fill = True
            t["filled_shares"] = filled
            t["fill_details"] = {
                "order_id": order_id,
                "status": status,
                "filled_shares": filled,
                "original_shares": original_size,
                "latency_ms": int((_time.time() - submit_started) * 1000),
                "fee_bps_ceiling": t.get("fee_bps_ceiling"),
                "submit_book": submit_book,
                "submit_price": submit_price,
                "response": _compact_dict(
                    outcome.get("last_response") if isinstance(outcome, dict) else None,
                    [
                        "status",
                        "size_matched",
                        "sizeMatched",
                        "price",
                        "avgPrice",
                        "takingAmount",
                        "makingAmount",
                        "transactionHash",
                        "transactionHashes",
                    ],
                ),
            }
            self.persistence.record_order_final(
                market_id=t.get("market_id", "unknown"),
                payload={
                    "trade_id": t.get("id"),
                    "decision_id": t.get("decision_id"),
                    "order_attempt_id": order_attempt_id,
                    "order_id": order_id,
                    "final_status": status,
                    "filled_shares": filled,
                    "original_shares": original_size,
                    "btc_price": t.get("btc_price"),
                    "last_response": last_resp_compact,
                    "final_ts": datetime.now(timezone.utc).isoformat(),
                    "latency_ms": int((_time.time() - submit_started) * 1000),
                },
            )
            logger.warning(
                f"[LIVE] Order PARTIAL fill: id={order_id} "
                f"filled={filled}/{original_size} "
                f"size_usd=${t['size_usd']:.2f}"
            )
            return t

        # Any other terminal state: rejected, cancelled, or timeout.
        last_resp = outcome.get("last_response")
        if status in ("timeout", "cancelled"):
            if not last_resp:
                reason = RejectReason.UNKNOWN
                msg = (
                    f"order {status} with no status payload id={order_id} "
                    "(network/CLOB error vs price-cross unknown)"
                )
            else:
                reason = RejectReason.PRICE_CROSSED
                msg = f"order {status} (likely price crossed) id={order_id}"
        else:
            msg = f"order {status} id={order_id} resp={last_resp}"
            reason = _classify_reject_reason(msg)
        self.risk.record_order_rejection(reason=reason, message=msg)
        # Do not release the window after a submitted live order reaches a
        # terminal no-fill state. A second same-window signal can straddle the
        # first decision and corrupt paired paper/live attribution.
        self._already_traded_this_window = True
        # Also persist the classified reject so we can analyze execution failure rates later.
        self.persistence.record_order_final(
            market_id=t.get("market_id", "unknown"),
            payload={
                "trade_id": t.get("id"),
                "decision_id": t.get("decision_id"),
                "order_attempt_id": order_attempt_id,
                "order_id": order_id,
                "final_status": status,
                "filled_shares": filled,
                "original_shares": original_size,
                "last_response": last_resp_compact,
                "reject_reason": reason.value,
                "reject_msg": msg[:800],
                "btc_price": t.get("btc_price"),
                "final_ts": datetime.now(timezone.utc).isoformat(),
                "latency_ms": int((_time.time() - submit_started) * 1000),
            },
        )
        logger.warning(
            f"[LIVE] Order NOT filled (status={status}): id={order_id}. "
            f"No position recorded. Window remains closed for re-entry."
        )
        return None

    async def _await_order_fill_hard_timeout(
        self, order_id: str, original_size: float, timeout_seconds: float
    ) -> dict:
        """Poll order status with a hard wall-clock timeout (used for exits)."""
        if self._polymarket is None:
            raise RuntimeError("live polling requires polymarket client")

        start = _time.time()
        last_resp: dict | None = None
        while True:
            if _time.time() - start >= timeout_seconds:
                try:
                    await self._polymarket.cancel_order(order_id)
                except Exception as e:
                    logger.warning(f"[LIVE] cancel_order({order_id}) on timeout failed: {e}")
                filled = 0.0
                if isinstance(last_resp, dict):
                    try:
                        filled = float(last_resp.get("size_matched", last_resp.get("sizeMatched", 0)) or 0)
                    except (TypeError, ValueError):
                        filled = 0.0
                if filled > 0 and filled + 1e-9 < original_size:
                    return {"status": "partial", "size_matched": filled, "last_response": last_resp}
                return {"status": "timeout", "size_matched": filled, "last_response": last_resp}

            try:
                resp = await self._polymarket.get_order_status(order_id)
            except Exception as e:
                logger.warning(f"[LIVE] get_order_status({order_id}) failed: {e}")
                await asyncio.sleep(self._order_poll_interval_s)
                continue

            last_resp = resp if isinstance(resp, dict) else {}
            status = (last_resp.get("status") or "").strip().lower()
            try:
                filled = float(last_resp.get("size_matched", last_resp.get("sizeMatched", 0)) or 0)
            except (TypeError, ValueError):
                filled = 0.0

            if status == "matched":
                if filled <= 0:
                    filled = original_size
                if filled + 1e-9 < original_size:
                    try:
                        await self._polymarket.cancel_order(order_id)
                    except Exception as e:
                        logger.warning(f"[LIVE] cancel_order({order_id}) on partial exit failed: {e}")
                    return {"status": "partial", "size_matched": filled, "last_response": last_resp}
                return {"status": "matched", "size_matched": filled, "last_response": last_resp}
            if status in ("cancelled", "canceled"):
                return {"status": "cancelled", "size_matched": filled, "last_response": last_resp}
            if status in ("rejected", "failed"):
                return {"status": "rejected", "size_matched": filled, "last_response": last_resp}

            await asyncio.sleep(self._order_poll_interval_s)

    async def _maybe_take_profit(self) -> None:
        if self.cfg.paper.enabled:
            return
        if self._polymarket is None:
            return
        if not bool(getattr(self.cfg.risk, "take_profit_enabled", False)):
            return

        bid_threshold = float(getattr(self.cfg.risk, "take_profit_best_bid_threshold", 0.95))
        sell_fraction = float(getattr(self.cfg.risk, "take_profit_sell_fraction", 1.0))
        min_bid_size = float(getattr(self.cfg.risk, "take_profit_min_best_bid_size_shares", 0.0))
        min_unrealized_usd = float(getattr(self.cfg.risk, "take_profit_min_unrealized_usd", 0.0))
        exit_slip = float(getattr(self.cfg.risk, "take_profit_exit_slippage_usd", 0.005))
        timeout_s = float(getattr(self.cfg.risk, "take_profit_order_timeout_seconds", 20))
        min_shares = float(getattr(self.cfg.risk, "min_shares", 5.0))
        # Closes #23: don't fire in last N seconds before window close. When
        # <60s remain, the winning side is already decided; selling at 95-99c
        # costs us 1-5c/share vs holding to $1 at resolution with no real
        # reversal risk. Effective only when >= 0; set to 0 to preserve
        # previous behavior.
        min_seconds_remaining = float(getattr(self.cfg.risk, "take_profit_min_seconds_remaining", 0.0))

        # We only ever have a handful of open positions; linear scan is fine.
        for t in list(self._open_trades):
            if t.get("exited"):
                continue
            token_id = t.get("token_id")
            if not token_id:
                continue
            # Window-timing gate: skip if too close to resolution.
            if min_seconds_remaining > 0 and self.current_window is not None:
                end = getattr(self.current_window, "end_time", None)
                if end is not None:
                    try:
                        secs_remaining = (end - datetime.now(timezone.utc)).total_seconds()
                    except Exception:
                        secs_remaining = min_seconds_remaining
                    if secs_remaining < min_seconds_remaining:
                        continue
            book = self.poly_ws.get_book(token_id)
            if book is None or book.best_bid <= 0:
                continue
            best_bid = float(book.best_bid)
            best_bid_size = float(getattr(book, "best_bid_size", 0.0) or 0.0)
            if best_bid < bid_threshold:
                continue
            if min_bid_size and best_bid_size < min_bid_size:
                continue

            shares = float(t.get("shares") or 0.0)
            if shares <= 0:
                continue

            shares_to_sell = shares * sell_fraction
            if best_bid_size > 0:
                shares_to_sell = min(shares_to_sell, best_bid_size)
            # Clamp to actual on-chain balance: Polymarket rejects SELL
            # orders larger than the ERC-1155 balance we hold (which is
            # often slightly less than intended size after partial fills).
            if self._redeemer is not None:
                try:
                    onchain = await self._redeemer.get_position_balance(token_id)
                    onchain_shares = float(onchain) / 1e6  # USDC decimals
                    if onchain_shares > 0:
                        shares_to_sell = min(shares_to_sell, onchain_shares)
                except Exception as e:
                    logger.warning("[TAKE_PROFIT] balance probe failed: %s", e)
            if shares_to_sell + 1e-9 < min_shares:
                continue

            entry_px = float(t.get("token_price") or 0.0)
            if entry_px <= 0:
                continue
            unrealized = shares_to_sell * (best_bid - entry_px)
            if unrealized < min_unrealized_usd:
                continue

            sell_price = max(0.01, min(best_bid - exit_slip, 0.99))
            if sell_price <= 0:
                continue

            market_id = str(t.get("market_id") or "")
            now = _time.time()
            if market_id and (now - self._last_take_profit_ts_by_market.get(market_id, 0.0) < 10.0):
                continue

            logger.warning(
                "[TAKE_PROFIT] Trigger: market=%s token=%s bid=%.3f bid_sz=%.2f "
                "entry_px=%.3f sell_px=%.3f shares=%.2f->%.2f unrealized≈$%.2f",
                market_id,
                str(token_id)[:16],
                best_bid,
                best_bid_size,
                entry_px,
                sell_price,
                shares,
                shares_to_sell,
                unrealized,
            )

            try:
                result = await self._polymarket.place_order(
                    token_id=token_id,
                    side="SELL",
                    price=sell_price,
                    size=shares_to_sell,
                )
            except Exception as e:
                logger.error(f"[TAKE_PROFIT] SELL place_order failed: {e}")
                continue

            order_id = result.get("orderID") or result.get("orderId") or result.get("id")
            if not order_id:
                logger.error(f"[TAKE_PROFIT] SELL returned no order id: {result}")
                continue

            self._last_take_profit_ts_by_market[market_id] = now
            outcome = await self._await_order_fill_hard_timeout(
                order_id=str(order_id),
                original_size=float(shares_to_sell),
                timeout_seconds=timeout_s,
            )
            status = outcome["status"]
            filled = float(outcome["size_matched"])
            if filled <= 0:
                logger.warning(f"[TAKE_PROFIT] SELL not filled status={status} id={order_id}")
                continue

            proceeds = filled * sell_price
            fee = taker_fee_usd(sell_price, proceeds)
            realized_pnl = filled * (sell_price - entry_px) - fee
            self.balance += proceeds - fee

            # Reduce or close the position.
            remaining = shares - filled
            if remaining <= 1e-6:
                t["exited"] = True
                # Prevent resolution/redeem bookkeeping for this market.
                self.resolver.resolved_ids.add(market_id)
                try:
                    self._open_trades.remove(t)
                except ValueError:
                    pass
            else:
                ratio = remaining / shares if shares > 0 else 0.0
                t["shares"] = remaining
                t["size_usd"] = float(t.get("size_usd") or 0.0) * ratio

            self.persistence.record_exit(
                self._strategy.name,
                t,
                exit_details={
                    "order_id": str(order_id),
                    "status": status,
                    "filled_shares": filled,
                    "sell_price": sell_price,
                    "proceeds_usd": proceeds,
                    "fee_usd": fee,
                    "realized_pnl_usd": realized_pnl,
                    "best_bid": best_bid,
                    "best_bid_size": best_bid_size,
                },
            )
            logger.warning(
                "[TAKE_PROFIT] Realized on exit: filled=%.2f proceeds=$%.2f fee=$%.2f pnl≈$%.2f balance=$%.2f",
                filled,
                proceeds,
                fee,
                realized_pnl,
                self.balance,
            )

    async def _maybe_stop_loss(self) -> None:
        if self.cfg.paper.enabled:
            return
        if self._polymarket is None:
            return
        if not bool(getattr(self.cfg.risk, "stop_loss_enabled", False)):
            return

        bid_threshold = float(getattr(self.cfg.risk, "stop_loss_best_bid_threshold", 0.15))
        sell_fraction = float(getattr(self.cfg.risk, "stop_loss_sell_fraction", 1.0))
        min_bid_size = float(getattr(self.cfg.risk, "stop_loss_min_best_bid_size_shares", 0.0))
        exit_slip = float(getattr(self.cfg.risk, "stop_loss_exit_slippage_usd", 0.01))
        timeout_s = float(getattr(self.cfg.risk, "stop_loss_order_timeout_seconds", 20))
        min_shares = float(getattr(self.cfg.risk, "min_shares", 5.0))

        for t in list(self._open_trades):
            if t.get("exited"):
                continue
            token_id = t.get("token_id")
            if not token_id:
                continue
            book = self.poly_ws.get_book(token_id)
            if book is None or book.best_bid <= 0:
                continue
            best_bid = float(book.best_bid)
            best_bid_size = float(getattr(book, "best_bid_size", 0.0) or 0.0)
            if best_bid > bid_threshold:
                continue
            if min_bid_size and best_bid_size < min_bid_size:
                continue

            shares = float(t.get("shares") or 0.0)
            if shares <= 0:
                continue

            shares_to_sell = shares * sell_fraction
            if best_bid_size > 0:
                shares_to_sell = min(shares_to_sell, best_bid_size)
            # Clamp to actual on-chain balance: Polymarket rejects SELL
            # orders larger than the ERC-1155 balance we hold.
            if self._redeemer is not None:
                try:
                    onchain = await self._redeemer.get_position_balance(token_id)
                    onchain_shares = float(onchain) / 1e6
                    if onchain_shares > 0:
                        shares_to_sell = min(shares_to_sell, onchain_shares)
                except Exception as e:
                    logger.warning("[STOP_LOSS] balance probe failed: %s", e)
            if shares_to_sell + 1e-9 < min_shares:
                continue

            entry_px = float(t.get("token_price") or 0.0)
            if entry_px <= 0:
                continue

            sell_price = max(0.01, min(best_bid - exit_slip, 0.99))
            if sell_price <= 0:
                continue

            unrealized = shares_to_sell * (sell_price - entry_px)

            market_id = str(t.get("market_id") or "")
            now = _time.time()
            if market_id and (now - self._last_stop_loss_ts_by_market.get(market_id, 0.0) < 10.0):
                continue

            logger.warning(
                "[STOP_LOSS] Trigger: market=%s token=%s bid=%.3f bid_sz=%.2f "
                "entry_px=%.3f sell_px=%.3f shares=%.2f->%.2f unrealized≈$%.2f",
                market_id,
                str(token_id)[:16],
                best_bid,
                best_bid_size,
                entry_px,
                sell_price,
                shares,
                shares_to_sell,
                unrealized,
            )

            try:
                result = await self._polymarket.place_order(
                    token_id=token_id,
                    side="SELL",
                    price=sell_price,
                    size=shares_to_sell,
                )
            except Exception as e:
                logger.error(f"[STOP_LOSS] SELL place_order failed: {e}")
                continue

            order_id = result.get("orderID") or result.get("orderId") or result.get("id")
            if not order_id:
                logger.error(f"[STOP_LOSS] SELL returned no order id: {result}")
                continue

            self._last_stop_loss_ts_by_market[market_id] = now
            outcome = await self._await_order_fill_hard_timeout(
                order_id=str(order_id),
                original_size=float(shares_to_sell),
                timeout_seconds=timeout_s,
            )
            status = outcome["status"]
            filled = float(outcome["size_matched"])
            if filled <= 0:
                logger.warning(f"[STOP_LOSS] SELL not filled status={status} id={order_id}")
                continue

            proceeds = filled * sell_price
            fee = taker_fee_usd(sell_price, proceeds)
            realized_pnl = filled * (sell_price - entry_px) - fee
            self.balance += proceeds - fee

            remaining = shares - filled
            if remaining <= 1e-6:
                t["exited"] = True
                self.resolver.resolved_ids.add(market_id)
                try:
                    self._open_trades.remove(t)
                except ValueError:
                    pass
            else:
                ratio = remaining / shares if shares > 0 else 0.0
                t["shares"] = remaining
                t["size_usd"] = float(t.get("size_usd") or 0.0) * ratio

            self.persistence.record_exit(
                self._strategy.name,
                t,
                exit_details={
                    "order_id": str(order_id),
                    "status": status,
                    "filled_shares": filled,
                    "sell_price": sell_price,
                    "proceeds_usd": proceeds,
                    "fee_usd": fee,
                    "realized_pnl_usd": realized_pnl,
                    "best_bid": best_bid,
                    "best_bid_size": best_bid_size,
                    "exit_reason": "stop_loss",
                },
            )
            logger.warning(
                "[STOP_LOSS] Realized on exit: filled=%.2f proceeds=$%.2f fee=$%.2f pnl≈$%.2f balance=$%.2f",
                filled,
                proceeds,
                fee,
                realized_pnl,
                self.balance,
            )

    async def _reconcile_live_balance(self) -> None:
        """Compare on-chain USDC vs our internal bookkeeping.

        Anchors to the on-chain balance captured at startup (+ cumulative
        realized PnL), NOT to cfg starting_balance. This tolerates the
        wallet being funded with a different amount than the config says.
        """
        if self.cfg.paper.enabled:
            return
        if self._polymarket is None:
            return
        if len(self._open_trades) > 0:
            return
        try:
            result = await self._polymarket.get_balance_allowance()
        except Exception as e:
            logger.warning(f"Live balance reconciliation failed: {e}")
            return
        actual = float(result.get("balance_usdc", 0.0))
        # First call anchors both ledgers. Subsequent calls compare only
        # changes since this anchor, so historical event_log P&L drift does
        # not immediately re-trip the breaker after restart.
        if self._live_usdc_anchor is None or self._live_internal_balance_anchor is None:
            self._live_usdc_anchor = actual
            self._live_internal_balance_anchor = self.balance
            logger.info(
                f"LIVE reconciliation anchor set: on-chain USDC=${actual:.2f}, "
                f"internal balance=${self.balance:.2f}. "
                f"Future drift checks measure deviations from this baseline."
            )
            return
        internal_delta = self.balance - self._live_internal_balance_anchor
        expected = self._live_usdc_anchor + internal_delta
        diff = abs(actual - expected)
        if diff > self._drift_threshold_usd:
            logger.warning(
                f"LIVE BALANCE DRIFT: actual=${actual:.2f} "
                f"expected=${expected:.2f} (anchor=${self._live_usdc_anchor:.2f} "
                f"+ internal_delta=${internal_delta:+.2f}) "
                f"diff=${diff:.2f} (threshold=${self._drift_threshold_usd:.2f}). "
                f"Halting entries."
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
        resolved = await self.resolver.resolve_trades(self._open_trades)
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
            resolution_source = (
                "push" if trade.get("pending_push_resolution") else "poll"
            )
            self.persistence.record_resolution(
                self._strategy.name,
                trade,
                res,
                resolved_dir,
                source=resolution_source,
            )

            if self._redeemer is not None:
                await self._redeem_winning_trade(trade)

        self._open_trades = self.resolver.prune_resolved(self._open_trades)

    async def _redeem_winning_trade(self, trade: dict) -> None:
        """Call CTF.redeemPositions for any resolved live trade (win or loss).

        Losers are redeemed too so the UI + on-chain state stay tidy; the
        balance precheck below short-circuits if there are no tokens held
        (e.g. take-profit already sold the winning tokens pre-resolution).

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

    def _query_open_entries(self) -> list[dict]:
        """Read event_log for entries without a matching resolution.

        An "open entry" is any `entry` event whose `market_id` (window_id)
        does not appear in a `resolution` event. Returns a list of the
        decoded entry-event `details` payloads (one dict per open entry).
        """
        import json as _json
        try:
            rows = self.memory.conn.execute(
                """
                SELECT e.window_id, e.details
                FROM event_log e
                WHERE e.event_type = 'entry'
                  AND NOT EXISTS (
                      SELECT 1 FROM event_log r
                      WHERE r.event_type = 'resolution'
                        AND r.window_id = e.window_id
                  )
                ORDER BY e.id
                """
            ).fetchall()
        except Exception as exc:
            logger.warning(f"_query_open_entries: SQL failed: {exc}")
            return []
        out: list[dict] = []
        for window_id, details in rows:
            if not details:
                continue
            try:
                d = _json.loads(details)
            except Exception:
                continue
            if not isinstance(d, dict):
                continue
            d.setdefault("market_id", window_id)
            out.append(d)
        return out

    async def _startup_reconciliation(self) -> list[dict]:
        """Catch up on resolutions the bot may have missed while offline.

        Walks every `entry` in event_log with no matching `resolution`,
        asks Gamma whether the market is resolved, and if so writes the
        resolution event (and queues a CTF redeem for winners). Leaves
        still-active markets alone.

        Returns a list of trades that were newly marked resolved AND won
        (so the caller can sweep their token balances). Resolution
        records are written in-place to event_log; this method never
        mutates or deletes existing records.
        """
        if self.cfg.paper.enabled:
            return []

        open_entries = self._query_open_entries()
        if not open_entries:
            logger.info("[RECONCILE] No open entries in event_log; nothing to reconcile.")
            return []

        logger.info(
            "[RECONCILE] Scanning %d open entries from event_log for "
            "missed resolutions...",
            len(open_entries),
        )

        won_trades: list[dict] = []
        n_checked = 0
        n_marked = 0
        n_won = 0
        n_lost = 0
        for trade in open_entries:
            market_id = trade.get("market_id")
            if not market_id:
                continue
            n_checked += 1
            try:
                result = await self.resolver.check_resolution(market_id)
            except Exception as e:
                logger.warning(
                    "[RECONCILE] check_resolution(%s) failed: %s",
                    market_id,
                    e,
                )
                continue
            if not result:
                continue

            winning = result["winning_outcome"]
            resolved_dir = "UP" if winning == "Yes" else "DOWN"

            # Stamp conditionId onto trade dict for downstream sweep.
            if result.get("condition_id") and not trade.get("condition_id"):
                trade["condition_id"] = result["condition_id"]

            record = PaperTradeRecord(
                trade_id=trade.get("id", trade.get("timestamp", market_id)),
                market_id=market_id,
                direction=trade["direction"],
                token_price=float(trade["token_price"]),
                size_usd=float(trade["size_usd"]),
                shares=float(trade["shares"]),
            )
            res = resolve_paper_trade(record, resolved_dir)
            self.persistence.record_resolution(
                self._strategy.name,
                trade,
                res,
                resolved_dir,
                source="reconcile",
            )
            self.resolver.resolved_ids.add(market_id)
            self.balance += float(trade["size_usd"]) + res.pnl_usd
            self.risk.record_trade_result(res.pnl_usd)
            self.risk.update_peak_balance(self.balance)
            n_marked += 1
            if res.won:
                n_won += 1
                won_trades.append(trade)
            else:
                n_lost += 1

        logger.info(
            "[RECONCILE] Reconciled %d entries. %d marked resolved "
            "(%d won, %d lost). %d redemptions queued.",
            n_checked,
            n_marked,
            n_won,
            n_lost,
            len(won_trades),
        )
        if n_marked:
            self._open_trades = self.resolver.prune_resolved(self._open_trades)
        return won_trades

    async def _startup_redemption_sweep(
        self, won_trades: list[dict]
    ) -> list[dict]:
        """Sweep the wallet for un-redeemed winning positions on startup.

        Combines:
          1. Positions just marked won by `_startup_reconciliation`.
          2. All historical `resolution` events in event_log where won=True
             (so earlier sessions that failed to redeem get another shot).

        For each (condition_id, token_id) with a non-zero ERC-1155
        balance, calls `redeemPositions`. Paper mode is a no-op.

        Returns the list of per-condition sweep results from CTFRedeemer.
        """
        if self.cfg.paper.enabled or self._redeemer is None:
            return []

        import json as _json
        positions: list[dict] = []
        seen_conditions: set[str] = set()

        for t in won_trades:
            cid = t.get("condition_id")
            if not cid:
                continue
            cid_norm = cid.lower()
            if cid_norm in seen_conditions:
                continue
            seen_conditions.add(cid_norm)
            positions.append({"condition_id": cid, "token_id": t.get("token_id")})

        try:
            rows = self.memory.conn.execute(
                "SELECT details FROM event_log WHERE event_type = 'resolution' "
                "ORDER BY id DESC LIMIT 500"
            ).fetchall()
        except Exception as e:
            logger.warning(f"[SWEEP] event_log read failed: {e}")
            rows = []
        for (details,) in rows:
            if not details:
                continue
            try:
                d = _json.loads(details)
            except Exception:
                continue
            if not isinstance(d, dict) or not d.get("won"):
                continue
            inner = d.get("trade") or {}
            cid = inner.get("condition_id")
            if not cid:
                continue
            cid_norm = cid.lower()
            if cid_norm in seen_conditions:
                continue
            seen_conditions.add(cid_norm)
            positions.append(
                {"condition_id": cid, "token_id": inner.get("token_id")}
            )

        if not positions:
            logger.info("[SWEEP] No winning condition ids found; nothing to sweep.")
            return []

        logger.info(
            "[SWEEP] Probing %d candidate winning positions for un-redeemed balances...",
            len(positions),
        )
        try:
            results = await self._redeemer.sweep_wallet(
                positions,
                allow_blind_redeem_without_balance_check=self._redeem_blind_when_token_missing,
            )
        except Exception as e:
            logger.error(f"[SWEEP] sweep_wallet threw: {e}")
            return []

        n_success = sum(1 for r in results if r.get("status") == "success")
        n_skipped = sum(1 for r in results if r.get("status") == "skipped")
        n_errored = sum(1 for r in results if r.get("status") in ("failed", "timeout", "error"))
        logger.info(
            "[SWEEP] Sweep complete: %d redeemed, %d skipped (zero balance), "
            "%d errored.",
            n_success,
            n_skipped,
            n_errored,
        )
        return results

    async def _maybe_periodic_redemption_sweep(self) -> None:
        if self.cfg.paper.enabled or self._redeemer is None:
            return
        if self._redeem_sweep_interval_s <= 0:
            return
        now = _time.time()
        if self._last_redeem_sweep_ts and (
            now - self._last_redeem_sweep_ts < self._redeem_sweep_interval_s
        ):
            return
        self._last_redeem_sweep_ts = now
        logger.info(
            "[SWEEP] Periodic sweep tick (interval=%ss, blind_when_token_missing=%s)",
            self._redeem_sweep_interval_s,
            self._redeem_blind_when_token_missing,
        )
        try:
            await self._startup_redemption_sweep([])
        except Exception as e:
            logger.warning(f"[SWEEP] periodic sweep failed: {e}")

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

        # Push paths (parallel to polling). Start only in live mode.
        user_ws_task: asyncio.Task | None = None
        resolution_ws_task: asyncio.Task | None = None
        if self._polymarket is not None:
            try:
                creds = self._polymarket.get_api_creds()
            except Exception as e:
                creds = None
                logger.warning(f"User WS: failed to read L2 creds: {e}")
            if creds:
                self._user_ws = PolymarketUserWS(
                    api_key=creds["apiKey"],
                    api_secret=creds["secret"],
                    api_passphrase=creds["passphrase"],
                    on_event=self._on_user_ws_event,
                )
                user_ws_task = asyncio.create_task(self._user_ws.connect())
                logger.info(
                    "User WS enabled (push path active; REST polling "
                    "remains as fallback)"
                )
            else:
                logger.warning(
                    "User WS disabled: no L2 creds resolved. "
                    "Order fills will only be detected via REST polling."
                )

            ws_url = os.environ.get("POLYGON_WS_URL", "")
            if not ws_url:
                # Fall back to Config default (also env-driven).
                ws_url = getattr(Config(), "POLYGON_WS_URL", "") or ""
            if ws_url:
                self._resolution_ws = ResolutionWatcher(
                    ws_url=ws_url,
                    on_resolution=self._on_resolution_push,
                )
                resolution_ws_task = asyncio.create_task(
                    self._resolution_ws.connect()
                )
                logger.info(
                    "Resolution WS enabled (push path active; Gamma "
                    "polling remains as fallback)"
                )
            else:
                logger.warning(
                    "Resolution WS disabled: POLYGON_WS_URL unset. "
                    "Resolutions detected via Gamma polling only."
                )

        logger.info(
            f"BTC Sniper started | Paper: {self.cfg.paper.enabled} | "
            f"Balance: ${self.balance:.2f} | "
            f"Strategy: {self._strategy.name} {self._strategy.params}"
        )
        await self.slack.notify_startup(
            self.balance, self._strategy.params.get("move", 0.08), self.cfg.paper.enabled
        )

        # Startup catch-up (live mode only). Paper mode skips: there is
        # no on-chain reality to reconcile against.
        if not self.cfg.paper.enabled:
            try:
                newly_won = await self._startup_reconciliation()
            except Exception as e:
                logger.error(f"[RECONCILE] startup reconciliation failed: {e}")
                newly_won = []
            try:
                await self._startup_redemption_sweep(newly_won)
            except Exception as e:
                logger.error(f"[SWEEP] startup redemption sweep failed: {e}")

        tick_count = 0
        start_time = _time.time()
        try:
            while self._running:
                try:
                    await self._scan_for_window()

                    for t in self._check_entry():
                        if not self.cfg.paper.enabled:
                            if self._paired_paper_enabled:
                                self.persistence.record_paper_shadow_entry(
                                    self._strategy.name,
                                    t,
                                )
                            trade = await self._submit_and_confirm_live_order(t)
                            if trade is None:
                                # Order never filled (timeout / cancel / reject).
                                # Do NOT record the entry: event_log must
                                # only contain trades that actually filled
                                # on-chain.
                                continue
                            t = trade
                        else:
                            if bool(getattr(self.cfg.risk, "paper_livelike_enabled", False)):
                                trade = await self._simulate_paper_live_like_fill(t)
                                if trade is None:
                                    continue
                                t = trade
                        self._open_trades.append(t)
                        # Fill-confirmed: persist the entry. Paper mode
                        # always "fills" on append; live mode only reaches
                        # here after _submit_and_confirm_live_order reported
                        # matched or partial.
                        self.persistence.record_entry(self._strategy.name, t)
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
                    # Lightweight periodic check; the sweep itself is time-gated.
                    if tick_count % 20 == 0:
                        await self._maybe_periodic_redemption_sweep()
                        await self._maybe_take_profit()
                        await self._maybe_stop_loss()
                    if tick_count % 3000 == 0:
                        await self._reconcile_live_balance()
                    if tick_count % 600 == 0:
                        await self._process_resolutions()
                        total = len(self._open_trades) + len(self.resolver.resolved_ids)

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

                        user_ws_connected = bool(
                            self._user_ws is not None and self._user_ws.connected
                        )
                        resolution_ws_connected = bool(
                            self._resolution_ws is not None
                            and self._resolution_ws.connected
                        )
                        self.health.update(
                            balance=self.balance, trades_total=total,
                            trades_resolved=len(self.resolver.resolved_ids),
                            p_up=None,
                            binance_connected=binance.seconds_since_last_message < 10,
                            binance_last_msg_age_s=round(binance.seconds_since_last_message, 1),
                            current_window=self.current_window.question if self.current_window else None,
                            user_ws_connected=user_ws_connected,
                            resolution_ws_connected=resolution_ws_connected,
                        )
                        logger.info(
                            f"Status: Balance=${self.balance:.2f} | "
                            f"Trades={len(self._open_trades)} | "
                            f"Resolved={len(self.resolver.resolved_ids)}"
                        )

                    if tick_count % 36000 == 0:
                        uptime_h = (_time.time() - start_time) / 3600
                        total = len(self._open_trades) + len(self.resolver.resolved_ids)
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
            if self._user_ws is not None:
                try:
                    await self._user_ws.close()
                except Exception as e:
                    logger.warning(f"User WS close failed: {e}")
            if user_ws_task is not None:
                user_ws_task.cancel()
            if self._resolution_ws is not None:
                try:
                    await self._resolution_ws.close()
                except Exception as e:
                    logger.warning(f"Resolution WS close failed: {e}")
            if resolution_ws_task is not None:
                resolution_ws_task.cancel()
            if self._scanner:
                await self._scanner.close()
            await self.resolver.close()
            await self.slack.close()

    async def stop(self) -> None:
        self._running = False
        self.persistence.close()
