# BTC 5-Minute Sniper — Phase 1 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Build a paper-trading Polymarket agent that snipes cheap tokens on Bitcoin Up/Down 5-minute markets using a Bayesian signal engine fed by Binance real-time data.

**Architecture:** Binance WebSocket feeds BTC trades into a Bayesian log-odds signal engine. When confidence exceeds threshold and cheap tokens (<=5c) are available on the Polymarket book, the agent sweeps them. Paper trading mode simulates fills. All trades persisted to SQLite. Runs as a resilient async process with automatic reconnection.

**Tech Stack:** Python 3.13, asyncio, websockets, httpx, py-clob-client, sqlite3, pytest

---

## File Structure

```
polymarket-agent/
├── config.py                          # MODIFY: add BTC sniper config params
├── models/
│   ├── market.py                      # MODIFY: add MarketWindow dataclass
│   └── trade.py                       # MODIFY: add CRYPTO signal source
├── clients/
│   ├── polymarket.py                  # MODIFY: add BTC market discovery
│   └── binance_ws.py                  # CREATE: Binance WebSocket client
├── strategies/
│   ├── base.py                        # NO CHANGE
│   └── btc_sniper.py                  # CREATE: Bayesian signal engine + sniper strategy
├── core/
│   ├── engine.py                      # NO CHANGE (we build a new entry point)
│   ├── risk.py                        # MODIFY: add asymmetric Kelly for cheap tokens
│   ├── portfolio.py                   # NO CHANGE
│   └── memory.py                      # MODIFY: add event_log table
├── btc_main.py                        # CREATE: entry point for BTC sniper agent
├── strategy_config.json               # CREATE: tunable parameters
└── tests/
    ├── test_binance_ws.py             # CREATE
    ├── test_btc_signal.py             # CREATE
    ├── test_btc_market_discovery.py   # CREATE
    ├── test_btc_risk.py               # CREATE
    ├── test_btc_engine.py             # CREATE
    └── test_strategy_config.py        # CREATE
```

---

### Task 1: Strategy Config Loading

**Files:**
- Create: `strategy_config.json`
- Create: `tests/test_strategy_config.py`
- Create: `strategies/strategy_config.py`

- [ ] **Step 1: Write the failing test**

```python
# tests/test_strategy_config.py
"""Tests for strategy_config loading and validation."""
from __future__ import annotations

import json
import tempfile
from pathlib import Path

import pytest

from strategies.strategy_config import StrategyConfig, load_strategy_config


def test_load_default_config():
    cfg = StrategyConfig()
    assert cfg.signal.w1_order_flow == 0.3
    assert cfg.signal.w2_microprice == 0.2
    assert cfg.signal.w3_price_delta == 0.4
    assert cfg.signal.w4_acceleration == 0.1
    assert cfg.signal.confidence_threshold == 0.85
    assert cfg.execution.max_entry_price == 0.05
    assert cfg.risk.kelly_multiplier == 0.25
    assert cfg.risk.cheap_token_multiplier == 2.0
    assert cfg.paper.enabled is True
    assert cfg.paper.starting_balance == 100.0


def test_load_from_file():
    data = {
        "version": 2,
        "signal": {
            "w1_order_flow": 0.5,
            "w2_microprice": 0.1,
            "w3_price_delta": 0.3,
            "w4_acceleration": 0.1,
            "confidence_threshold": 0.90,
            "prior": 0.5,
        },
        "execution": {
            "max_entry_price": 0.03,
            "entry_window_early": [0, 60],
            "entry_window_late": [270, 295],
            "enable_early_snipe": True,
            "enable_late_snipe": False,
        },
        "risk": {
            "max_position_usd": 100,
            "max_position_pct": 0.15,
            "daily_loss_limit_pct": 0.30,
            "kill_balance_usd": 5,
            "max_concurrent_positions": 30,
            "loss_cooldown_trades": 10,
            "loss_cooldown_seconds": 60,
            "kelly_multiplier": 0.5,
            "cheap_token_multiplier": 3.0,
        },
        "paper": {"enabled": True, "starting_balance": 500.0},
    }
    with tempfile.NamedTemporaryFile(mode="w", suffix=".json", delete=False) as f:
        json.dump(data, f)
        f.flush()
        cfg = load_strategy_config(f.name)

    assert cfg.signal.w1_order_flow == 0.5
    assert cfg.execution.max_entry_price == 0.03
    assert cfg.execution.enable_late_snipe is False
    assert cfg.risk.kelly_multiplier == 0.5
    assert cfg.paper.starting_balance == 500.0


def test_load_missing_file_returns_defaults():
    cfg = load_strategy_config("/nonexistent/path.json")
    assert cfg.signal.confidence_threshold == 0.85
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cd /Users/jackreid/go/polymarket-agent && python -m pytest tests/test_strategy_config.py -v`
Expected: FAIL — `ModuleNotFoundError: No module named 'strategies.strategy_config'`

- [ ] **Step 3: Create strategy_config.json**

```json
{
    "version": 1,
    "promoted_at": "2026-03-31T12:00:00Z",
    "signal": {
        "w1_order_flow": 0.3,
        "w2_microprice": 0.2,
        "w3_price_delta": 0.4,
        "w4_acceleration": 0.1,
        "confidence_threshold": 0.85,
        "prior": 0.5
    },
    "execution": {
        "max_entry_price": 0.05,
        "entry_window_early": [0, 60],
        "entry_window_late": [270, 295],
        "enable_early_snipe": true,
        "enable_late_snipe": true
    },
    "risk": {
        "max_position_usd": 50,
        "max_position_pct": 0.10,
        "daily_loss_limit_pct": 0.25,
        "kill_balance_usd": 10,
        "max_concurrent_positions": 20,
        "loss_cooldown_trades": 5,
        "loss_cooldown_seconds": 300,
        "kelly_multiplier": 0.25,
        "cheap_token_multiplier": 2.0
    },
    "paper": {
        "enabled": true,
        "starting_balance": 100.0
    }
}
```

- [ ] **Step 4: Write minimal implementation**

```python
# strategies/strategy_config.py
"""Tunable strategy parameters loaded from JSON config file."""
from __future__ import annotations

import json
import logging
from dataclasses import dataclass, field
from pathlib import Path

logger = logging.getLogger(__name__)


@dataclass
class SignalConfig:
    w1_order_flow: float = 0.3
    w2_microprice: float = 0.2
    w3_price_delta: float = 0.4
    w4_acceleration: float = 0.1
    confidence_threshold: float = 0.85
    prior: float = 0.5


@dataclass
class ExecutionConfig:
    max_entry_price: float = 0.05
    entry_window_early: list[int] = field(default_factory=lambda: [0, 60])
    entry_window_late: list[int] = field(default_factory=lambda: [270, 295])
    enable_early_snipe: bool = True
    enable_late_snipe: bool = True


@dataclass
class RiskConfig:
    max_position_usd: float = 50.0
    max_position_pct: float = 0.10
    daily_loss_limit_pct: float = 0.25
    kill_balance_usd: float = 10.0
    max_concurrent_positions: int = 20
    loss_cooldown_trades: int = 5
    loss_cooldown_seconds: int = 300
    kelly_multiplier: float = 0.25
    cheap_token_multiplier: float = 2.0


@dataclass
class PaperConfig:
    enabled: bool = True
    starting_balance: float = 100.0


@dataclass
class StrategyConfig:
    version: int = 1
    promoted_at: str = ""
    signal: SignalConfig = field(default_factory=SignalConfig)
    execution: ExecutionConfig = field(default_factory=ExecutionConfig)
    risk: RiskConfig = field(default_factory=RiskConfig)
    paper: PaperConfig = field(default_factory=PaperConfig)


def load_strategy_config(path: str) -> StrategyConfig:
    """Load strategy config from JSON file. Returns defaults if file missing."""
    p = Path(path)
    if not p.exists():
        logger.warning(f"Config file not found at {path}, using defaults")
        return StrategyConfig()

    with open(p) as f:
        data = json.load(f)

    cfg = StrategyConfig(
        version=data.get("version", 1),
        promoted_at=data.get("promoted_at", ""),
    )
    if "signal" in data:
        cfg.signal = SignalConfig(**data["signal"])
    if "execution" in data:
        cfg.execution = ExecutionConfig(**data["execution"])
    if "risk" in data:
        cfg.risk = RiskConfig(**data["risk"])
    if "paper" in data:
        cfg.paper = PaperConfig(**data["paper"])
    return cfg
```

- [ ] **Step 5: Run tests to verify they pass**

Run: `cd /Users/jackreid/go/polymarket-agent && python -m pytest tests/test_strategy_config.py -v`
Expected: 3 passed

- [ ] **Step 6: Commit**

```bash
cd /Users/jackreid/go/polymarket-agent
git add strategy_config.json strategies/strategy_config.py tests/test_strategy_config.py
git commit -m "feat: strategy config loading from JSON with defaults"
```

---

### Task 2: Binance WebSocket Client

**Files:**
- Create: `clients/binance_ws.py`
- Create: `tests/test_binance_ws.py`

- [ ] **Step 1: Write the failing test**

```python
# tests/test_binance_ws.py
"""Tests for Binance WebSocket client."""
from __future__ import annotations

import asyncio
import json
from unittest.mock import AsyncMock, MagicMock, patch

import pytest

from clients.binance_ws import BinanceWSClient, TradeUpdate, OrderBookSnapshot


def test_trade_update_from_raw():
    raw = {
        "e": "trade",
        "E": 1711843200000,
        "s": "BTCUSDT",
        "p": "84350.50",
        "q": "0.123",
        "m": False,  # isBuyerMaker=False means buyer is taker (buy)
        "T": 1711843200000,
    }
    update = TradeUpdate.from_raw(raw)
    assert update.price == 84350.50
    assert update.quantity == 0.123
    assert update.is_buyer_maker is False
    assert update.is_buy is True


def test_trade_update_sell():
    raw = {
        "e": "trade",
        "E": 1711843200000,
        "s": "BTCUSDT",
        "p": "84350.50",
        "q": "0.05",
        "m": True,  # isBuyerMaker=True means seller is taker (sell)
        "T": 1711843200000,
    }
    update = TradeUpdate.from_raw(raw)
    assert update.is_buyer_maker is True
    assert update.is_buy is False


def test_order_book_snapshot_microprice():
    snap = OrderBookSnapshot(
        best_bid=84350.0,
        best_ask=84351.0,
        bid_size=2.0,
        ask_size=1.0,
        timestamp_ms=1711843200000,
    )
    # microprice = (bid_size * ask + ask_size * bid) / (bid_size + ask_size)
    # = (2.0 * 84351 + 1.0 * 84350) / 3.0 = (168702 + 84350) / 3 = 84350.6667
    assert abs(snap.microprice - 84350.6667) < 0.001
    assert snap.mid == 84350.5


@pytest.mark.asyncio
async def test_client_callback_invoked():
    """Verify that trade callbacks fire when messages arrive."""
    received = []

    async def on_trade(update: TradeUpdate):
        received.append(update)

    client = BinanceWSClient(on_trade=on_trade)

    raw_msg = json.dumps({
        "e": "trade",
        "E": 1711843200000,
        "s": "BTCUSDT",
        "p": "84500.00",
        "q": "0.5",
        "m": False,
        "T": 1711843200000,
    })

    await client._handle_message(raw_msg)
    assert len(received) == 1
    assert received[0].price == 84500.00


@pytest.mark.asyncio
async def test_client_ignores_non_trade_messages():
    received = []

    async def on_trade(update: TradeUpdate):
        received.append(update)

    client = BinanceWSClient(on_trade=on_trade)
    await client._handle_message(json.dumps({"e": "depthUpdate", "data": {}}))
    assert len(received) == 0
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cd /Users/jackreid/go/polymarket-agent && python -m pytest tests/test_binance_ws.py -v`
Expected: FAIL — `ModuleNotFoundError: No module named 'clients.binance_ws'`

- [ ] **Step 3: Write minimal implementation**

```python
# clients/binance_ws.py
"""Binance WebSocket client for real-time BTC/USDT trade and order book data.

Connects to Binance's public WebSocket streams. Provides trade-by-trade
updates with buyer/seller classification (isBuyerMaker field) and
order book snapshots with microprice calculation.

Reconnects automatically with exponential backoff on disconnect.
"""
from __future__ import annotations

import asyncio
import json
import logging
import time
from dataclasses import dataclass
from typing import Awaitable, Callable

logger = logging.getLogger(__name__)

BINANCE_WS_URL = "wss://stream.binance.com:9443/ws/btcusdt@trade"


@dataclass
class TradeUpdate:
    price: float
    quantity: float
    is_buyer_maker: bool
    timestamp_ms: int

    @property
    def is_buy(self) -> bool:
        return not self.is_buyer_maker

    @staticmethod
    def from_raw(raw: dict) -> TradeUpdate:
        return TradeUpdate(
            price=float(raw["p"]),
            quantity=float(raw["q"]),
            is_buyer_maker=raw["m"],
            timestamp_ms=raw.get("T", raw.get("E", 0)),
        )


@dataclass
class OrderBookSnapshot:
    best_bid: float
    best_ask: float
    bid_size: float
    ask_size: float
    timestamp_ms: int

    @property
    def mid(self) -> float:
        return (self.best_bid + self.best_ask) / 2

    @property
    def microprice(self) -> float:
        total = self.bid_size + self.ask_size
        if total == 0:
            return self.mid
        return (self.bid_size * self.best_ask + self.ask_size * self.best_bid) / total


class BinanceWSClient:
    """Async Binance WebSocket client with auto-reconnect."""

    def __init__(
        self,
        on_trade: Callable[[TradeUpdate], Awaitable[None]] | None = None,
        url: str = BINANCE_WS_URL,
    ):
        self._on_trade = on_trade
        self._url = url
        self._ws = None
        self._running = False
        self._last_message_time: float = 0.0
        self._reconnect_delay: float = 1.0
        self._max_reconnect_delay: float = 30.0

    async def connect(self) -> None:
        """Connect and start receiving messages. Reconnects on failure."""
        import websockets

        self._running = True
        while self._running:
            try:
                logger.info(f"Connecting to Binance WebSocket: {self._url}")
                async with websockets.connect(self._url) as ws:
                    self._ws = ws
                    self._reconnect_delay = 1.0
                    logger.info("Binance WebSocket connected")
                    async for message in ws:
                        self._last_message_time = time.time()
                        await self._handle_message(message)
            except Exception as e:
                if not self._running:
                    break
                logger.warning(
                    f"Binance WebSocket disconnected: {e}. "
                    f"Reconnecting in {self._reconnect_delay:.0f}s..."
                )
                await asyncio.sleep(self._reconnect_delay)
                self._reconnect_delay = min(
                    self._reconnect_delay * 2, self._max_reconnect_delay
                )

    async def _handle_message(self, raw_msg: str) -> None:
        try:
            data = json.loads(raw_msg)
        except json.JSONDecodeError:
            return

        event_type = data.get("e")
        if event_type == "trade" and self._on_trade:
            update = TradeUpdate.from_raw(data)
            await self._on_trade(update)

    @property
    def seconds_since_last_message(self) -> float:
        if self._last_message_time == 0:
            return float("inf")
        return time.time() - self._last_message_time

    async def close(self) -> None:
        self._running = False
        if self._ws:
            await self._ws.close()
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cd /Users/jackreid/go/polymarket-agent && python -m pytest tests/test_binance_ws.py -v`
Expected: 5 passed

- [ ] **Step 5: Commit**

```bash
cd /Users/jackreid/go/polymarket-agent
git add clients/binance_ws.py tests/test_binance_ws.py
git commit -m "feat: Binance WebSocket client with auto-reconnect"
```

---

### Task 3: Bayesian Signal Engine

**Files:**
- Create: `strategies/btc_sniper.py`
- Create: `tests/test_btc_signal.py`

- [ ] **Step 1: Write the failing test**

```python
# tests/test_btc_signal.py
"""Tests for the Bayesian signal engine."""
from __future__ import annotations

import math

import pytest

from strategies.btc_sniper import BayesianSignalEngine
from strategies.strategy_config import SignalConfig


def make_engine(
    w1: float = 0.3,
    w2: float = 0.2,
    w3: float = 0.4,
    w4: float = 0.1,
    threshold: float = 0.85,
) -> BayesianSignalEngine:
    cfg = SignalConfig(
        w1_order_flow=w1,
        w2_microprice=w2,
        w3_price_delta=w3,
        w4_acceleration=w4,
        confidence_threshold=threshold,
    )
    return BayesianSignalEngine(cfg)


def test_initial_state():
    engine = make_engine()
    assert engine.p_up == 0.5
    assert engine.log_odds == 0.0
    assert engine.direction is None
    assert engine.confident is False


def test_strong_up_signal():
    engine = make_engine(w3=1.0, w1=0.0, w2=0.0, w4=0.0, threshold=0.80)
    # Simulate a strong BTC price move up: delta = +0.5% (normalized)
    # log_odds += w3 * price_delta = 1.0 * 2.0 = 2.0
    engine.update(order_flow_imbalance=0.0, microprice_deviation=0.0,
                  price_delta=2.0, acceleration=0.0)
    assert engine.p_up > 0.80
    assert engine.direction == "UP"
    assert engine.confident is True


def test_strong_down_signal():
    engine = make_engine(w3=1.0, w1=0.0, w2=0.0, w4=0.0, threshold=0.80)
    engine.update(order_flow_imbalance=0.0, microprice_deviation=0.0,
                  price_delta=-2.0, acceleration=0.0)
    assert engine.p_up < 0.20
    assert engine.direction == "DOWN"
    assert engine.confident is True


def test_cumulative_updates():
    engine = make_engine(w3=0.5, w1=0.0, w2=0.0, w4=0.0, threshold=0.90)
    # Small moves that accumulate
    for _ in range(10):
        engine.update(order_flow_imbalance=0.0, microprice_deviation=0.0,
                      price_delta=0.3, acceleration=0.0)
    # 10 * 0.5 * 0.3 = 1.5 log-odds → sigmoid(1.5) ≈ 0.818
    assert engine.p_up > 0.80


def test_reset():
    engine = make_engine()
    engine.update(0.0, 0.0, 5.0, 0.0)
    assert engine.p_up != 0.5
    engine.reset()
    assert engine.p_up == 0.5
    assert engine.log_odds == 0.0


def test_order_flow_signal():
    engine = make_engine(w1=1.0, w2=0.0, w3=0.0, w4=0.0, threshold=0.70)
    engine.update(order_flow_imbalance=1.5, microprice_deviation=0.0,
                  price_delta=0.0, acceleration=0.0)
    assert engine.p_up > 0.70


def test_mixed_signals_cancel():
    engine = make_engine(w1=0.5, w3=0.5, w2=0.0, w4=0.0, threshold=0.90)
    # Order flow says UP, price says DOWN — cancel out
    engine.update(order_flow_imbalance=1.0, microprice_deviation=0.0,
                  price_delta=-1.0, acceleration=0.0)
    # 0.5*1.0 + 0.5*(-1.0) = 0.0 → p_up = 0.5
    assert abs(engine.p_up - 0.5) < 0.01
    assert engine.confident is False
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cd /Users/jackreid/go/polymarket-agent && python -m pytest tests/test_btc_signal.py -v`
Expected: FAIL — `ModuleNotFoundError: No module named 'strategies.btc_sniper'`

- [ ] **Step 3: Write minimal implementation**

```python
# strategies/btc_sniper.py
"""Bayesian signal engine for BTC 5-minute Up/Down markets.

Maintains a posterior probability P(UP) in log-odds space,
updated additively from real-time Binance data. When confidence
exceeds threshold, produces a directional signal for sniping
cheap tokens on Polymarket.
"""
from __future__ import annotations

import math
import logging

from strategies.strategy_config import SignalConfig

logger = logging.getLogger(__name__)


class BayesianSignalEngine:
    """Real-time Bayesian probability estimator for BTC direction."""

    def __init__(self, config: SignalConfig) -> None:
        self._w1 = config.w1_order_flow
        self._w2 = config.w2_microprice
        self._w3 = config.w3_price_delta
        self._w4 = config.w4_acceleration
        self._threshold = config.confidence_threshold
        self.log_odds: float = 0.0  # logit(0.5) = 0

    @property
    def p_up(self) -> float:
        return 1.0 / (1.0 + math.exp(-self.log_odds))

    @property
    def p_down(self) -> float:
        return 1.0 - self.p_up

    @property
    def direction(self) -> str | None:
        if self.p_up >= self._threshold:
            return "UP"
        if self.p_down >= self._threshold:
            return "DOWN"
        return None

    @property
    def confident(self) -> bool:
        return self.direction is not None

    def update(
        self,
        order_flow_imbalance: float,
        microprice_deviation: float,
        price_delta: float,
        acceleration: float,
    ) -> None:
        delta = (
            self._w1 * order_flow_imbalance
            + self._w2 * microprice_deviation
            + self._w3 * price_delta
            + self._w4 * acceleration
        )
        self.log_odds += delta

    def reset(self) -> None:
        self.log_odds = 0.0
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cd /Users/jackreid/go/polymarket-agent && python -m pytest tests/test_btc_signal.py -v`
Expected: 7 passed

- [ ] **Step 5: Commit**

```bash
cd /Users/jackreid/go/polymarket-agent
git add strategies/btc_sniper.py tests/test_btc_signal.py
git commit -m "feat: Bayesian signal engine for BTC direction"
```

---

### Task 4: BTC Market Discovery

**Files:**
- Modify: `models/market.py` — add `MarketWindow` dataclass
- Modify: `models/trade.py` — add `CRYPTO_SNIPER` to `SignalSource`
- Modify: `clients/polymarket.py` — add `get_btc_markets()` method
- Create: `tests/test_btc_market_discovery.py`

- [ ] **Step 1: Write the failing test**

```python
# tests/test_btc_market_discovery.py
"""Tests for BTC Up/Down 5-minute market discovery."""
from __future__ import annotations

import json
from datetime import datetime, timezone
from unittest.mock import AsyncMock, patch

import pytest

from models.market import MarketWindow


def test_market_window_time_remaining():
    now = datetime(2026, 3, 31, 7, 20, 0, tzinfo=timezone.utc)
    window = MarketWindow(
        market_id="abc123",
        question="Bitcoin Up or Down - March 31, 3:20AM-3:25AM ET",
        start_time=datetime(2026, 3, 31, 7, 20, 0, tzinfo=timezone.utc),
        end_time=datetime(2026, 3, 31, 7, 25, 0, tzinfo=timezone.utc),
        up_token_id="tok_up",
        down_token_id="tok_down",
        up_price=0.50,
        down_price=0.50,
    )
    remaining = window.time_remaining(now)
    assert remaining == 300.0  # 5 minutes


def test_market_window_is_active():
    start = datetime(2026, 3, 31, 7, 20, 0, tzinfo=timezone.utc)
    end = datetime(2026, 3, 31, 7, 25, 0, tzinfo=timezone.utc)
    window = MarketWindow(
        market_id="abc123",
        question="Bitcoin Up or Down",
        start_time=start,
        end_time=end,
        up_token_id="tok_up",
        down_token_id="tok_down",
        up_price=0.50,
        down_price=0.50,
    )
    during = datetime(2026, 3, 31, 7, 22, 0, tzinfo=timezone.utc)
    before = datetime(2026, 3, 31, 7, 19, 0, tzinfo=timezone.utc)
    after = datetime(2026, 3, 31, 7, 26, 0, tzinfo=timezone.utc)

    assert window.is_active(during) is True
    assert window.is_active(before) is False
    assert window.is_active(after) is False


def test_market_window_elapsed_seconds():
    start = datetime(2026, 3, 31, 7, 20, 0, tzinfo=timezone.utc)
    end = datetime(2026, 3, 31, 7, 25, 0, tzinfo=timezone.utc)
    window = MarketWindow(
        market_id="abc123",
        question="test",
        start_time=start,
        end_time=end,
        up_token_id="u",
        down_token_id="d",
        up_price=0.5,
        down_price=0.5,
    )
    at = datetime(2026, 3, 31, 7, 21, 30, tzinfo=timezone.utc)
    assert window.elapsed_seconds(at) == 90.0


def test_signal_source_has_crypto_sniper():
    from models.trade import SignalSource
    assert SignalSource.CRYPTO_SNIPER == "crypto_sniper"
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cd /Users/jackreid/go/polymarket-agent && python -m pytest tests/test_btc_market_discovery.py -v`
Expected: FAIL — `ImportError: cannot import name 'MarketWindow'`

- [ ] **Step 3: Add MarketWindow to models/market.py**

Append to the end of `models/market.py`:

```python
@dataclass
class MarketWindow:
    """A single 5-minute Bitcoin Up/Down market window."""
    market_id: str
    question: str
    start_time: datetime
    end_time: datetime
    up_token_id: str
    down_token_id: str
    up_price: float = 0.5
    down_price: float = 0.5

    def time_remaining(self, now: datetime) -> float:
        return max(0.0, (self.end_time - now).total_seconds())

    def elapsed_seconds(self, now: datetime) -> float:
        return max(0.0, (now - self.start_time).total_seconds())

    def is_active(self, now: datetime) -> bool:
        return self.start_time <= now < self.end_time
```

- [ ] **Step 4: Add CRYPTO_SNIPER to SignalSource in models/trade.py**

Add to the `SignalSource` enum:

```python
CRYPTO_SNIPER = "crypto_sniper"
```

- [ ] **Step 5: Run tests to verify they pass**

Run: `cd /Users/jackreid/go/polymarket-agent && python -m pytest tests/test_btc_market_discovery.py -v`
Expected: 4 passed

- [ ] **Step 6: Run full test suite to verify no regressions**

Run: `cd /Users/jackreid/go/polymarket-agent && python -m pytest tests/ -v --tb=short`
Expected: All existing tests still pass

- [ ] **Step 7: Commit**

```bash
cd /Users/jackreid/go/polymarket-agent
git add models/market.py models/trade.py tests/test_btc_market_discovery.py
git commit -m "feat: MarketWindow model and CRYPTO_SNIPER signal source"
```

---

### Task 5: Asymmetric Kelly Sizing for Cheap Tokens

**Files:**
- Modify: `core/risk.py` — add `asymmetric_kelly_size` method
- Create: `tests/test_btc_risk.py`

- [ ] **Step 1: Write the failing test**

```python
# tests/test_btc_risk.py
"""Tests for asymmetric Kelly sizing on cheap tokens."""
from __future__ import annotations

import pytest

from core.risk import RiskManager
from config import Config
from strategies.strategy_config import RiskConfig


def make_risk_manager(
    kelly_mult: float = 0.25,
    cheap_mult: float = 2.0,
    max_pos_usd: float = 50.0,
    max_pos_pct: float = 0.10,
) -> RiskManager:
    config = Config()
    config.MAX_POSITION_USD = max_pos_usd
    config.MAX_POSITION_PCT = max_pos_pct
    rm = RiskManager(config)
    return rm


def test_cheap_token_sizing_basic():
    rm = make_risk_manager()
    risk_cfg = RiskConfig(kelly_multiplier=0.25, cheap_token_multiplier=2.0,
                          max_position_usd=50.0, max_position_pct=0.10)
    # p=0.15, c=0.02 → edge=0.13, kelly=(0.13)/(1-0.02)=0.1327
    # adjusted = 0.1327 * 0.25 * 2.0 (cheap mult) = 0.0663
    # size = 0.0663 * 1000 = $66.33, capped at $50
    size = rm.asymmetric_kelly_size(
        p_win=0.15, token_price=0.02, bankroll=1000.0, risk_cfg=risk_cfg
    )
    assert size == 50.0  # capped at max_position_usd


def test_cheap_token_sizing_small_bankroll():
    rm = make_risk_manager()
    risk_cfg = RiskConfig(kelly_multiplier=0.25, cheap_token_multiplier=2.0,
                          max_position_usd=50.0, max_position_pct=0.10)
    # bankroll=100 → max_pos_pct=10% → $10 cap
    size = rm.asymmetric_kelly_size(
        p_win=0.15, token_price=0.02, bankroll=100.0, risk_cfg=risk_cfg
    )
    assert size == 10.0  # capped at 10% of bankroll


def test_no_edge_returns_zero():
    rm = make_risk_manager()
    risk_cfg = RiskConfig(kelly_multiplier=0.25, cheap_token_multiplier=2.0,
                          max_position_usd=50.0, max_position_pct=0.10)
    # p=0.02 (same as price) → edge=0 → kelly=0
    size = rm.asymmetric_kelly_size(
        p_win=0.02, token_price=0.02, bankroll=1000.0, risk_cfg=risk_cfg
    )
    assert size == 0.0


def test_negative_edge_returns_zero():
    rm = make_risk_manager()
    risk_cfg = RiskConfig(kelly_multiplier=0.25, cheap_token_multiplier=2.0,
                          max_position_usd=50.0, max_position_pct=0.10)
    size = rm.asymmetric_kelly_size(
        p_win=0.01, token_price=0.05, bankroll=1000.0, risk_cfg=risk_cfg
    )
    assert size == 0.0


def test_non_cheap_token_no_multiplier():
    rm = make_risk_manager()
    risk_cfg = RiskConfig(kelly_multiplier=0.25, cheap_token_multiplier=2.0,
                          max_position_usd=50.0, max_position_pct=0.10)
    # Token at 0.10 (above 0.05 threshold) → no cheap multiplier
    # p=0.20, c=0.10 → edge=0.10, kelly=0.10/0.90=0.1111
    # adjusted = 0.1111 * 0.25 = 0.0278
    # size = 0.0278 * 1000 = $27.78
    size = rm.asymmetric_kelly_size(
        p_win=0.20, token_price=0.10, bankroll=1000.0, risk_cfg=risk_cfg
    )
    assert 25.0 < size < 30.0  # no cheap multiplier applied


def test_minimum_size_floor():
    rm = make_risk_manager()
    risk_cfg = RiskConfig(kelly_multiplier=0.01, cheap_token_multiplier=1.0,
                          max_position_usd=50.0, max_position_pct=0.10)
    # Very small kelly → tiny position → should return 0 (below $1 floor)
    size = rm.asymmetric_kelly_size(
        p_win=0.06, token_price=0.05, bankroll=100.0, risk_cfg=risk_cfg
    )
    assert size == 0.0
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cd /Users/jackreid/go/polymarket-agent && python -m pytest tests/test_btc_risk.py -v`
Expected: FAIL — `AttributeError: 'RiskManager' object has no attribute 'asymmetric_kelly_size'`

- [ ] **Step 3: Add asymmetric_kelly_size to core/risk.py**

Add this method to the `RiskManager` class:

```python
    def asymmetric_kelly_size(
        self,
        p_win: float,
        token_price: float,
        bankroll: float,
        risk_cfg: "RiskConfig",
    ) -> float:
        """Position sizing for cheap binary tokens with asymmetric payoffs.

        For tokens priced at 2-5 cents, max loss is the token price per share
        but max gain is (1 - token_price). This bounded downside allows more
        aggressive sizing than standard Kelly.

        Args:
            p_win: estimated probability of the token resolving to $1
            token_price: current price of the token (0-1)
            bankroll: current total balance in USD
            risk_cfg: strategy-level risk parameters
        """
        if token_price <= 0 or token_price >= 1 or bankroll <= 0 or p_win <= 0:
            return 0.0

        edge = p_win - token_price
        if edge <= 0:
            return 0.0

        kelly_fraction = edge / (1.0 - token_price)
        adjusted = kelly_fraction * risk_cfg.kelly_multiplier

        if token_price <= 0.05:
            adjusted *= risk_cfg.cheap_token_multiplier

        max_by_pct = bankroll * risk_cfg.max_position_pct
        max_size = min(max_by_pct, risk_cfg.max_position_usd)
        position = min(adjusted * bankroll, max_size)

        if position < 1.0:
            return 0.0

        return round(position, 2)
```

Also add the import at the top of `core/risk.py`:

```python
from typing import TYPE_CHECKING

if TYPE_CHECKING:
    from strategies.strategy_config import RiskConfig
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cd /Users/jackreid/go/polymarket-agent && python -m pytest tests/test_btc_risk.py -v`
Expected: 6 passed

- [ ] **Step 5: Run full test suite**

Run: `cd /Users/jackreid/go/polymarket-agent && python -m pytest tests/ -v --tb=short`
Expected: All tests pass

- [ ] **Step 6: Commit**

```bash
cd /Users/jackreid/go/polymarket-agent
git add core/risk.py tests/test_btc_risk.py
git commit -m "feat: asymmetric Kelly sizing for cheap binary tokens"
```

---

### Task 6: Event Log Table in Memory Store

**Files:**
- Modify: `core/memory.py` — add `event_log` table and `save_event` method
- Create: `tests/test_event_log.py`

- [ ] **Step 1: Write the failing test**

```python
# tests/test_event_log.py
"""Tests for the event log table in MemoryStore."""
from __future__ import annotations

import json
import tempfile

import pytest

from core.memory import MemoryStore


@pytest.fixture
def store():
    with tempfile.NamedTemporaryFile(suffix=".db", delete=False) as f:
        s = MemoryStore(f.name)
        yield s
        s.close()


def test_save_and_retrieve_event(store: MemoryStore):
    store.save_event(
        window_id="win_001",
        event_type="signal_update",
        log_odds=1.5,
        p_up=0.818,
        btc_price=84350.0,
        details={"reason": "strong buy flow"},
    )
    events = store.get_events_for_window("win_001")
    assert len(events) == 1
    assert events[0]["event_type"] == "signal_update"
    assert events[0]["log_odds"] == 1.5
    assert events[0]["p_up"] == 0.818
    assert events[0]["btc_price"] == 84350.0
    details = json.loads(events[0]["details"])
    assert details["reason"] == "strong buy flow"


def test_multiple_events_for_window(store: MemoryStore):
    for i in range(5):
        store.save_event(
            window_id="win_002",
            event_type="signal_update",
            log_odds=float(i),
            p_up=0.5,
            btc_price=84000.0 + i,
        )
    events = store.get_events_for_window("win_002")
    assert len(events) == 5


def test_events_isolated_by_window(store: MemoryStore):
    store.save_event(window_id="win_A", event_type="entry_attempt")
    store.save_event(window_id="win_B", event_type="fill")
    assert len(store.get_events_for_window("win_A")) == 1
    assert len(store.get_events_for_window("win_B")) == 1
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cd /Users/jackreid/go/polymarket-agent && python -m pytest tests/test_event_log.py -v`
Expected: FAIL — `AttributeError: 'MemoryStore' object has no attribute 'save_event'`

- [ ] **Step 3: Add event_log table and methods to core/memory.py**

Add to the `SCHEMA` string:

```sql
CREATE TABLE IF NOT EXISTS event_log (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    window_id TEXT NOT NULL,
    timestamp TEXT DEFAULT CURRENT_TIMESTAMP,
    event_type TEXT NOT NULL,
    log_odds REAL,
    p_up REAL,
    btc_price REAL,
    details TEXT
);
```

Add these methods to the `MemoryStore` class:

```python
    def save_event(
        self,
        window_id: str,
        event_type: str,
        log_odds: float | None = None,
        p_up: float | None = None,
        btc_price: float | None = None,
        details: dict | None = None,
    ) -> None:
        self.conn.execute(
            """INSERT INTO event_log (window_id, event_type, log_odds, p_up, btc_price, details)
            VALUES (?, ?, ?, ?, ?, ?)""",
            (
                window_id,
                event_type,
                log_odds,
                p_up,
                btc_price,
                json.dumps(details) if details else None,
            ),
        )
        self.conn.commit()

    def get_events_for_window(self, window_id: str) -> list[dict]:
        rows = self.conn.execute(
            "SELECT * FROM event_log WHERE window_id = ? ORDER BY id",
            (window_id,),
        ).fetchall()
        return [dict(r) for r in rows]
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cd /Users/jackreid/go/polymarket-agent && python -m pytest tests/test_event_log.py -v`
Expected: 3 passed

- [ ] **Step 5: Run full test suite**

Run: `cd /Users/jackreid/go/polymarket-agent && python -m pytest tests/ -v --tb=short`
Expected: All tests pass

- [ ] **Step 6: Commit**

```bash
cd /Users/jackreid/go/polymarket-agent
git add core/memory.py tests/test_event_log.py
git commit -m "feat: event log table for high-frequency audit trail"
```

---

### Task 7: BTC Sniper Trading Engine

**Files:**
- Create: `core/btc_engine.py`
- Create: `tests/test_btc_engine.py`

This is the main orchestrator that ties together: Binance WS → Signal Engine → Market Discovery → Risk → Execution → Persistence.

- [ ] **Step 1: Write the failing test**

```python
# tests/test_btc_engine.py
"""Tests for the BTC sniper trading engine."""
from __future__ import annotations

import asyncio
import json
import tempfile
from datetime import datetime, timedelta, timezone
from unittest.mock import AsyncMock, MagicMock, patch

import pytest

from core.btc_engine import BTCTradingEngine
from clients.binance_ws import TradeUpdate
from models.market import MarketWindow
from strategies.strategy_config import StrategyConfig


def make_engine(paper: bool = True) -> BTCTradingEngine:
    cfg = StrategyConfig()
    cfg.paper.enabled = paper
    cfg.paper.starting_balance = 100.0
    cfg.signal.confidence_threshold = 0.80
    cfg.signal.w3_price_delta = 1.0
    cfg.signal.w1_order_flow = 0.0
    cfg.signal.w2_microprice = 0.0
    cfg.signal.w4_acceleration = 0.0
    cfg.execution.max_entry_price = 0.05
    cfg.risk.max_position_usd = 10.0
    cfg.risk.max_position_pct = 0.10

    with tempfile.NamedTemporaryFile(suffix=".db", delete=False) as f:
        db_path = f.name

    engine = BTCTradingEngine(cfg, db_path=db_path)
    return engine


def test_engine_initializes():
    engine = make_engine()
    assert engine.balance == 100.0
    assert engine.signal_engine.p_up == 0.5
    assert engine.current_window is None


def test_engine_resets_signal_on_new_window():
    engine = make_engine()
    engine.signal_engine.update(0, 0, 5.0, 0)
    assert engine.signal_engine.p_up != 0.5

    now = datetime.now(timezone.utc)
    window = MarketWindow(
        market_id="m1",
        question="BTC Up/Down",
        start_time=now,
        end_time=now + timedelta(minutes=5),
        up_token_id="tok_up",
        down_token_id="tok_down",
    )
    engine._on_new_window(window)
    assert engine.signal_engine.p_up == 0.5
    assert engine.current_window == window


@pytest.mark.asyncio
async def test_engine_processes_trade_updates():
    engine = make_engine()

    now = datetime.now(timezone.utc)
    engine.current_window = MarketWindow(
        market_id="m1",
        question="BTC Up/Down",
        start_time=now - timedelta(seconds=10),
        end_time=now + timedelta(minutes=4, seconds=50),
        up_token_id="tok_up",
        down_token_id="tok_down",
    )
    engine._window_open_price = 84000.0

    update = TradeUpdate(
        price=84100.0,  # +$100 → positive delta
        quantity=1.0,
        is_buyer_maker=False,
        timestamp_ms=int(now.timestamp() * 1000),
    )
    await engine._on_binance_trade(update)
    # With w3=1.0, a positive price delta should push p_up > 0.5
    assert engine.signal_engine.p_up > 0.5


@pytest.mark.asyncio
async def test_engine_generates_paper_trade():
    engine = make_engine()

    now = datetime.now(timezone.utc)
    engine.current_window = MarketWindow(
        market_id="m1",
        question="BTC Up/Down",
        start_time=now - timedelta(seconds=30),
        end_time=now + timedelta(minutes=4, seconds=30),
        up_token_id="tok_up",
        down_token_id="tok_down",
        up_price=0.02,
        down_price=0.98,
    )
    engine._window_open_price = 84000.0
    engine._already_traded_this_window = False

    # Force high confidence
    engine.signal_engine.log_odds = 3.0  # p_up ≈ 0.953

    trades = engine._check_entry()
    assert len(trades) == 1
    assert trades[0]["direction"] == "UP"
    assert trades[0]["token_price"] <= 0.05  # bought UP at 0.02


@pytest.mark.asyncio
async def test_engine_no_trade_when_no_cheap_tokens():
    engine = make_engine()

    now = datetime.now(timezone.utc)
    engine.current_window = MarketWindow(
        market_id="m1",
        question="BTC Up/Down",
        start_time=now - timedelta(seconds=30),
        end_time=now + timedelta(minutes=4, seconds=30),
        up_token_id="tok_up",
        down_token_id="tok_down",
        up_price=0.50,  # not cheap
        down_price=0.50,  # not cheap
    )
    engine._window_open_price = 84000.0
    engine._already_traded_this_window = False
    engine.signal_engine.log_odds = 3.0

    trades = engine._check_entry()
    assert len(trades) == 0  # no cheap tokens available


@pytest.mark.asyncio
async def test_engine_no_double_trade():
    engine = make_engine()

    now = datetime.now(timezone.utc)
    engine.current_window = MarketWindow(
        market_id="m1",
        question="BTC Up/Down",
        start_time=now - timedelta(seconds=30),
        end_time=now + timedelta(minutes=4, seconds=30),
        up_token_id="tok_up",
        down_token_id="tok_down",
        up_price=0.02,
        down_price=0.98,
    )
    engine._window_open_price = 84000.0
    engine._already_traded_this_window = True  # already traded
    engine.signal_engine.log_odds = 3.0

    trades = engine._check_entry()
    assert len(trades) == 0
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cd /Users/jackreid/go/polymarket-agent && python -m pytest tests/test_btc_engine.py -v`
Expected: FAIL — `ModuleNotFoundError: No module named 'core.btc_engine'`

- [ ] **Step 3: Write the BTC trading engine**

```python
# core/btc_engine.py
"""BTC 5-minute sniper trading engine.

Orchestrates: Binance WS → Signal Engine → Entry Check → Execution → Persistence.
Runs as a continuous async loop, one iteration per 100ms tick.
"""
from __future__ import annotations

import asyncio
import logging
import time
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

        # Order flow tracking
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

        mid = self._last_btc_price
        microprice_dev = 0.0  # requires LOB data; zero for now

        price_delta = (mid - self._window_open_price) / self._window_open_price * 100
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
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cd /Users/jackreid/go/polymarket-agent && python -m pytest tests/test_btc_engine.py -v`
Expected: 6 passed

- [ ] **Step 5: Run full test suite**

Run: `cd /Users/jackreid/go/polymarket-agent && python -m pytest tests/ -v --tb=short`
Expected: All tests pass

- [ ] **Step 6: Commit**

```bash
cd /Users/jackreid/go/polymarket-agent
git add core/btc_engine.py tests/test_btc_engine.py
git commit -m "feat: BTC sniper trading engine with paper trading"
```

---

### Task 8: Entry Point and Main Loop

**Files:**
- Create: `btc_main.py`

- [ ] **Step 1: Write the entry point**

```python
# btc_main.py
"""Entry point for the BTC 5-minute sniper agent.

Usage:
    python btc_main.py                    # paper trading (default)
    python btc_main.py --live             # live trading
    python btc_main.py --config path.json # custom config
"""
from __future__ import annotations

import argparse
import asyncio
import logging
import os
import signal
import sys
from pathlib import Path

from dotenv import load_dotenv

load_dotenv()


def setup_logging(level: str = "INFO") -> None:
    logging.basicConfig(
        level=getattr(logging, level.upper(), logging.INFO),
        format="%(asctime)s [%(levelname)s] %(name)s: %(message)s",
        datefmt="%Y-%m-%d %H:%M:%S",
    )


def write_pid() -> None:
    pid_file = Path("btc_agent.pid")
    if pid_file.exists():
        old_pid = int(pid_file.read_text().strip())
        try:
            os.kill(old_pid, 0)
            print(f"Agent already running (PID {old_pid}). Exiting.")
            sys.exit(1)
        except OSError:
            pass  # stale PID file
    pid_file.write_text(str(os.getpid()))


def cleanup_pid() -> None:
    pid_file = Path("btc_agent.pid")
    if pid_file.exists():
        pid_file.unlink()


async def main(config_path: str, live: bool) -> None:
    from strategies.strategy_config import load_strategy_config
    from core.btc_engine import BTCTradingEngine

    cfg = load_strategy_config(config_path)
    if live:
        cfg.paper.enabled = False

    db_path = os.getenv("BTC_DB_PATH", "btc_trades.db")
    engine = BTCTradingEngine(cfg, db_path=db_path)

    loop = asyncio.get_event_loop()

    def handle_shutdown(sig, frame):
        logging.info(f"Received signal {sig}, shutting down...")
        loop.create_task(engine.stop())

    signal.signal(signal.SIGINT, handle_shutdown)
    signal.signal(signal.SIGTERM, handle_shutdown)

    # Reload config on SIGHUP
    def handle_reload(sig, frame):
        logging.info("SIGHUP received, reloading strategy config...")
        new_cfg = load_strategy_config(config_path)
        engine.cfg = new_cfg
        engine.signal_engine = __import__(
            "strategies.btc_sniper", fromlist=["BayesianSignalEngine"]
        ).BayesianSignalEngine(new_cfg.signal)
        logging.info("Config reloaded")

    signal.signal(signal.SIGHUP, handle_reload)

    # Write heartbeat periodically
    async def heartbeat():
        while engine._running:
            Path("btc_heartbeat.txt").write_text(
                f"{__import__('datetime').datetime.now().isoformat()}"
            )
            await asyncio.sleep(30)

    asyncio.create_task(heartbeat())

    try:
        await engine.run()
    finally:
        await engine.stop()
        cleanup_pid()


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description="BTC 5-Minute Sniper Agent")
    parser.add_argument("--config", default="strategy_config.json", help="Path to strategy config JSON")
    parser.add_argument("--live", action="store_true", help="Enable live trading (default: paper)")
    parser.add_argument("--log-level", default="INFO", help="Log level")
    args = parser.parse_args()

    setup_logging(args.log_level)
    write_pid()

    try:
        asyncio.run(main(args.config, args.live))
    finally:
        cleanup_pid()
```

- [ ] **Step 2: Verify syntax**

Run: `cd /Users/jackreid/go/polymarket-agent && python -c "import py_compile; py_compile.compile('btc_main.py', doraise=True)"`
Expected: No output (clean compile)

- [ ] **Step 3: Commit**

```bash
cd /Users/jackreid/go/polymarket-agent
git add btc_main.py
git commit -m "feat: BTC sniper entry point with PID, heartbeat, SIGHUP reload"
```

---

### Task 9: Add websockets Dependency

**Files:**
- Modify: `requirements.txt`

- [ ] **Step 1: Add websockets to requirements.txt**

Add this line to `requirements.txt`:

```
websockets>=12.0
```

- [ ] **Step 2: Install dependencies**

Run: `cd /Users/jackreid/go/polymarket-agent && pip install websockets>=12.0`

- [ ] **Step 3: Verify import works**

Run: `cd /Users/jackreid/go/polymarket-agent && python -c "import websockets; print(websockets.__version__)"`
Expected: Version number printed

- [ ] **Step 4: Run full test suite**

Run: `cd /Users/jackreid/go/polymarket-agent && python -m pytest tests/ -v --tb=short`
Expected: All tests pass (including new BTC tests)

- [ ] **Step 5: Commit**

```bash
cd /Users/jackreid/go/polymarket-agent
git add requirements.txt
git commit -m "deps: add websockets for Binance real-time feed"
```

---

### Task 10: Integration Smoke Test

**Files:**
- Create: `tests/test_btc_integration.py`

A fast integration test that wires together all components without network calls.

- [ ] **Step 1: Write the integration test**

```python
# tests/test_btc_integration.py
"""Integration test: full pipeline from Binance trade to paper trade."""
from __future__ import annotations

import tempfile
from datetime import datetime, timedelta, timezone

import pytest

from clients.binance_ws import TradeUpdate
from core.btc_engine import BTCTradingEngine
from core.memory import MemoryStore
from models.market import MarketWindow
from strategies.strategy_config import StrategyConfig


@pytest.mark.asyncio
async def test_full_pipeline_paper_trade():
    """Simulate: BTC moves up strongly → signal fires → paper trade executed."""
    cfg = StrategyConfig()
    cfg.paper.enabled = True
    cfg.paper.starting_balance = 100.0
    cfg.signal.confidence_threshold = 0.80
    cfg.signal.w3_price_delta = 1.0
    cfg.signal.w1_order_flow = 0.0
    cfg.signal.w2_microprice = 0.0
    cfg.signal.w4_acceleration = 0.0
    cfg.execution.max_entry_price = 0.05
    cfg.risk.max_position_usd = 10.0
    cfg.risk.max_position_pct = 0.10
    cfg.risk.kelly_multiplier = 0.25
    cfg.risk.cheap_token_multiplier = 2.0

    with tempfile.NamedTemporaryFile(suffix=".db", delete=False) as f:
        db_path = f.name

    engine = BTCTradingEngine(cfg, db_path=db_path)

    now = datetime.now(timezone.utc)
    engine.current_window = MarketWindow(
        market_id="integration_test_001",
        question="Bitcoin Up or Down - Test",
        start_time=now - timedelta(seconds=30),
        end_time=now + timedelta(minutes=4, seconds=30),
        up_token_id="tok_up_001",
        down_token_id="tok_down_001",
        up_price=0.02,   # cheap UP token
        down_price=0.98,
    )
    engine._window_open_price = 84000.0
    engine._last_btc_price = 84000.0

    # Simulate 20 strong buy trades pushing BTC up
    for i in range(20):
        update = TradeUpdate(
            price=84000.0 + (i + 1) * 10,  # BTC climbing $10 per trade
            quantity=0.5,
            is_buyer_maker=False,  # buy
            timestamp_ms=int((now + timedelta(seconds=i)).timestamp() * 1000),
        )
        await engine._on_binance_trade(update)

    # Signal should be confident UP by now
    assert engine.signal_engine.p_up > 0.80, f"p_up={engine.signal_engine.p_up}"

    # Check entry
    trades = engine._check_entry()
    assert len(trades) == 1
    trade = trades[0]
    assert trade["direction"] == "UP"
    assert trade["token_price"] == 0.02
    assert trade["size_usd"] > 0

    # Verify event was logged
    events = engine.memory.get_events_for_window("integration_test_001")
    assert len(events) >= 1
    assert events[-1]["event_type"] == "entry"

    # Verify no double trade
    trades2 = engine._check_entry()
    assert len(trades2) == 0

    engine.memory.close()


@pytest.mark.asyncio
async def test_full_pipeline_no_trade_when_uncertain():
    """Simulate: BTC moves sideways → no signal → no trade."""
    cfg = StrategyConfig()
    cfg.paper.enabled = True
    cfg.signal.confidence_threshold = 0.90
    cfg.signal.w3_price_delta = 0.5
    cfg.signal.w1_order_flow = 0.0
    cfg.signal.w2_microprice = 0.0
    cfg.signal.w4_acceleration = 0.0
    cfg.execution.max_entry_price = 0.05

    with tempfile.NamedTemporaryFile(suffix=".db", delete=False) as f:
        db_path = f.name

    engine = BTCTradingEngine(cfg, db_path=db_path)

    now = datetime.now(timezone.utc)
    engine.current_window = MarketWindow(
        market_id="integration_test_002",
        question="Bitcoin Up or Down - Test Sideways",
        start_time=now - timedelta(seconds=30),
        end_time=now + timedelta(minutes=4, seconds=30),
        up_token_id="tok_up",
        down_token_id="tok_down",
        up_price=0.02,
        down_price=0.98,
    )
    engine._window_open_price = 84000.0
    engine._last_btc_price = 84000.0

    # Simulate sideways: alternating small up/down
    for i in range(20):
        direction = 1 if i % 2 == 0 else -1
        update = TradeUpdate(
            price=84000.0 + direction * 5,
            quantity=0.3,
            is_buyer_maker=(i % 2 == 1),
            timestamp_ms=int((now + timedelta(seconds=i)).timestamp() * 1000),
        )
        await engine._on_binance_trade(update)

    # Signal should NOT be confident
    assert not engine.signal_engine.confident

    trades = engine._check_entry()
    assert len(trades) == 0

    engine.memory.close()
```

- [ ] **Step 2: Run integration tests**

Run: `cd /Users/jackreid/go/polymarket-agent && python -m pytest tests/test_btc_integration.py -v`
Expected: 2 passed

- [ ] **Step 3: Run full test suite one final time**

Run: `cd /Users/jackreid/go/polymarket-agent && python -m pytest tests/ -v --tb=short`
Expected: All tests pass

- [ ] **Step 4: Commit**

```bash
cd /Users/jackreid/go/polymarket-agent
git add tests/test_btc_integration.py
git commit -m "test: integration smoke test for full BTC sniper pipeline"
```

---

## Summary

After completing all 10 tasks, you will have:

| Component | File | Status |
|-----------|------|--------|
| Strategy config | `strategies/strategy_config.py` + `strategy_config.json` | Tunable params from JSON |
| Binance WebSocket | `clients/binance_ws.py` | Real-time BTC trades with reconnect |
| Bayesian signal | `strategies/btc_sniper.py` | Log-odds P(UP) from 4 features |
| Market models | `models/market.py` | `MarketWindow` for 5-min markets |
| Asymmetric Kelly | `core/risk.py` | Aggressive sizing for cheap tokens |
| Event logging | `core/memory.py` | High-frequency audit trail |
| Trading engine | `core/btc_engine.py` | Full pipeline orchestrator |
| Entry point | `btc_main.py` | PID, heartbeat, SIGHUP, systemd-ready |
| Tests | `tests/test_btc_*.py` | 30+ tests covering all components |

**Not yet built (Phase 2+):**
- Gamma API market window discovery (currently manual/mock)
- Live order execution via CLOB
- Resolution checker for paper P&L
- Backtester for historical replay
- Autoresearch harness
- Autonomous researcher
- Hetzner deployment (systemd unit files)
