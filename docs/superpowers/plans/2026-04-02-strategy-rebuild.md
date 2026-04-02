# Multi-Coin Latency Arb: Codebase Rebuild & Deployment

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Clean up the Polymarket trading bot, replace the dead Bayesian engine with proven threshold strategies, port data to Supabase, deploy autoresearch loop on Hetzner, and set up a live data collector.

**Architecture:** Simple threshold detection per coin (BTC 0.08%, ETH 0.15%, SOL 0.08%). Buy directional token when price move detected and book hasn't repriced (entry <= $0.55). Strategy params stored in JSON, loaded at runtime. Data in Supabase for cross-machine access. Autoresearch runs continuously on Hetzner, proposes mutations, human approves before deployment.

**Tech Stack:** Python 3.14, asyncio, httpx, websockets, py-clob-client, SQLite (local) + Supabase (remote), Docker (Hetzner)

---

## File Structure

### New files to create
```
shared/
  __init__.py              # Package init
  fees.py                  # Canonical taker_fee() function (~20 lines)
  db.py                    # DB connection, schema init, loaders (~80 lines)
  constants.py             # API URLs, keys, coin configs (~30 lines)

strategies/
  threshold.py             # ThresholdStrategy class (~80 lines)
  registry.py              # Strategy dispatch map (~50 lines)

backtesting/
  precompute.py            # PrecomputedMarket + feature extraction (~180 lines)
  simulator.py             # simulate_strategy + grid builder (~200 lines)
  research.py              # run_autoresearch orchestrator (~120 lines)
  fetcher.py               # CoinDataFetcher class (~200 lines)
  projection.py            # MonteCarloSimulator class (~250 lines)

collector/
  __init__.py
  snapshot_recorder.py     # Live data collector (~150 lines)

tests/
  test_threshold.py        # ThresholdStrategy tests
  test_fees.py             # Fee calculation tests
  test_simulator.py        # Backtest simulator tests
```

### Files to delete
```
strategies/btc_sniper.py         # Old Bayesian engine
autoresearch/                    # Entire directory (old mutation loop)
researcher/                      # Entire directory (stubs)
backtesting/btc_backtest.py      # Old Bayesian backtester
backtesting/validate_configs.py  # Old Bayesian config validator
backtesting/historical_data.py   # Merged into fetcher.py
backtesting/bulk_fetch.py        # Merged into fetcher.py
backtesting/fetch_multicoin.py   # Merged into fetcher.py
backtesting/fast_fetch.py        # Merged into fetcher.py
backtesting/kelly_projection.py  # Merged into projection.py
backtesting/stochastic_projection.py  # Merged into projection.py
backtesting/autoresearch.py      # Split into precompute + simulator + research
tests/test_btc_signal.py         # Tests old Bayesian engine
tests/test_researcher.py         # Tests dead researcher stubs
tests/test_event_log.py          # Tests removed event log
```

### Files to modify
```
core/engine.py              # Replace BayesianSignalEngine with ThresholdStrategy
main.py                     # Remove Bayesian imports, load per-coin configs
strategies/strategy_config.py  # Replace SignalConfig with ThresholdConfig
config.py                   # Remove dead toggles
strategy_config.json        # New schema with per-coin thresholds
core/btc_resolution.py      # Use shared.fees
core/risk.py                # Use shared.fees
clients/binance_ws.py       # Parameterize symbol
clients/market_scanner.py   # Support ETH/SOL slug patterns
backtesting/visualize.py    # Update imports
backtesting/run_all_research.py  # Update imports
backtesting/run_backtest_all.py  # Use registry, update imports
backtesting/validate_latency_arb.py  # Use shared.fees/db
```

---

## Phase 1: Shared Utilities

### Task 1: Create shared/fees.py

**Files:**
- Create: `shared/__init__.py`
- Create: `shared/fees.py`
- Create: `tests/test_fees.py`

- [ ] **Step 1: Write the failing test**

```python
# tests/test_fees.py
from shared.fees import taker_fee, taker_fee_usd


def test_taker_fee_at_half():
    assert abs(taker_fee(0.50) - 0.018) < 0.0001


def test_taker_fee_at_zero():
    assert taker_fee(0.0) == 0.0


def test_taker_fee_at_one():
    assert taker_fee(1.0) == 0.0


def test_taker_fee_symmetric():
    assert abs(taker_fee(0.3) - taker_fee(0.7)) < 0.0001


def test_taker_fee_usd():
    fee = taker_fee_usd(0.50, 100.0)
    assert abs(fee - 1.80) < 0.01
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cd /Users/jackreid/go/polymarket-agent && python -m pytest tests/test_fees.py -v`
Expected: FAIL with "ModuleNotFoundError: No module named 'shared'"

- [ ] **Step 3: Write implementation**

```python
# shared/__init__.py
# (empty)
```

```python
# shared/fees.py
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
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cd /Users/jackreid/go/polymarket-agent && python -m pytest tests/test_fees.py -v`
Expected: PASS (5 tests)

- [ ] **Step 5: Commit**

```bash
cd /Users/jackreid/go/polymarket-agent && git add shared/ tests/test_fees.py && git commit -m "feat: add shared fee calculation module"
```

---

### Task 2: Create shared/constants.py

**Files:**
- Create: `shared/constants.py`

- [ ] **Step 1: Write constants file**

```python
# shared/constants.py
"""Shared constants for all modules."""
from pathlib import Path

PROJECT_ROOT = Path(__file__).parent.parent
BACKTESTING_DIR = PROJECT_ROOT / "backtesting"

POLYBACKTEST_API_BASE = "https://api.polybacktest.com"
POLYBACKTEST_API_KEYS = {
    "btc": "***POLYBACKTEST_KEY_REMOVED***",
    "eth": "***POLYBACKTEST_KEY_REMOVED***",
    "sol": "***POLYBACKTEST_KEY_REMOVED***",
}
RATE_LIMIT_DELAY = 0.20

COINS = ["btc", "eth", "sol"]

COIN_CONFIGS = {
    "btc": {"move_threshold": 0.08, "max_entry": 0.55, "binance_symbol": "btcusdt"},
    "eth": {"move_threshold": 0.15, "max_entry": 0.55, "binance_symbol": "ethusdt"},
    "sol": {"move_threshold": 0.08, "max_entry": 0.55, "binance_symbol": "solusdt"},
}

SLUG_PATTERNS = {
    "btc": "btc-updown-5m-{ts}",
    "eth": "eth-updown-5m-{ts}",
    "sol": "sol-updown-5m-{ts}",
}


def db_path(coin: str) -> Path:
    """Return the SQLite DB path for a given coin."""
    return BACKTESTING_DIR / f"{coin}.db"
```

- [ ] **Step 2: Verify import works**

Run: `cd /Users/jackreid/go/polymarket-agent && python -c "from shared.constants import COIN_CONFIGS; print(COIN_CONFIGS)"`
Expected: prints the config dict

- [ ] **Step 3: Commit**

```bash
cd /Users/jackreid/go/polymarket-agent && git add shared/constants.py && git commit -m "feat: add shared constants module"
```

---

### Task 3: Create shared/db.py

**Files:**
- Create: `shared/db.py`

- [ ] **Step 1: Write DB helper**

```python
# shared/db.py
"""Database connection and schema management."""
from __future__ import annotations

import sqlite3
from pathlib import Path


def get_connection(db_path: Path) -> sqlite3.Connection:
    """Create a connection with standard settings."""
    conn = sqlite3.connect(str(db_path))
    conn.execute("PRAGMA journal_mode=WAL")
    conn.execute("PRAGMA synchronous=NORMAL")
    conn.row_factory = sqlite3.Row
    return conn


def init_coin_db(db_path: Path) -> sqlite3.Connection:
    """Create or open a coin DB with the standard schema."""
    conn = get_connection(db_path)
    conn.execute("""
        CREATE TABLE IF NOT EXISTS markets (
            market_id TEXT PRIMARY KEY,
            slug TEXT, market_type TEXT,
            start_time TEXT, end_time TEXT,
            price_start REAL, price_end REAL,
            winner TEXT, final_volume REAL, final_liquidity REAL
        )
    """)
    conn.execute("""
        CREATE TABLE IF NOT EXISTS snapshots (
            market_id TEXT, time TEXT,
            price REAL, price_up REAL, price_down REAL,
            PRIMARY KEY (market_id, time)
        )
    """)
    conn.commit()
    return conn


def load_markets(db_path: Path) -> list[dict]:
    """Load all resolved markets from a coin DB."""
    conn = get_connection(db_path)
    rows = conn.execute(
        "SELECT * FROM markets WHERE winner IS NOT NULL ORDER BY start_time"
    ).fetchall()
    result = [dict(r) for r in rows]
    conn.close()
    return result


def load_snapshots(market_id: str, db_path: Path) -> list[dict]:
    """Load snapshots for a specific market."""
    conn = get_connection(db_path)
    rows = conn.execute(
        "SELECT * FROM snapshots WHERE market_id = ? ORDER BY time",
        (market_id,),
    ).fetchall()
    result = [dict(r) for r in rows]
    conn.close()
    return result


def snapshot_price_col(db_path: Path) -> str:
    """Detect whether snapshots use 'price' or 'btc_price' column."""
    conn = get_connection(db_path)
    cols = [c[1] for c in conn.execute("PRAGMA table_info(snapshots)").fetchall()]
    conn.close()
    return "price" if "price" in cols else "btc_price"


def market_start_col(db_path: Path) -> str:
    """Detect whether markets use 'price_start' or 'btc_price_start' column."""
    conn = get_connection(db_path)
    cols = [c[1] for c in conn.execute("PRAGMA table_info(markets)").fetchall()]
    conn.close()
    return "price_start" if "price_start" in cols else "btc_price_start"
```

- [ ] **Step 2: Verify it works**

Run: `cd /Users/jackreid/go/polymarket-agent && python -c "from shared.db import get_connection; print('OK')"`
Expected: prints "OK"

- [ ] **Step 3: Commit**

```bash
cd /Users/jackreid/go/polymarket-agent && git add shared/db.py && git commit -m "feat: add shared DB helper module"
```

---

## Phase 2: Delete Dead Code

### Task 4: Remove old Bayesian engine and dead modules

**Files:**
- Delete: `strategies/btc_sniper.py`
- Delete: `autoresearch/run_loop.py`
- Delete: `autoresearch/run_backtest.py`
- Delete: `autoresearch/__init__.py` (if exists)
- Delete: `researcher/researcher.py`
- Delete: `researcher/run_research.py`
- Delete: `researcher/__init__.py` (if exists)
- Delete: `backtesting/btc_backtest.py`
- Delete: `backtesting/validate_configs.py`
- Delete: `tests/test_btc_signal.py`
- Delete: `tests/test_researcher.py`
- Delete: `tests/test_event_log.py`

- [ ] **Step 1: Delete files**

```bash
cd /Users/jackreid/go/polymarket-agent
rm -f strategies/btc_sniper.py
rm -rf autoresearch/
rm -rf researcher/
rm -f backtesting/btc_backtest.py backtesting/validate_configs.py
rm -f tests/test_btc_signal.py tests/test_researcher.py tests/test_event_log.py
```

- [ ] **Step 2: Verify no import breaks in remaining files**

Run: `cd /Users/jackreid/go/polymarket-agent && python -c "from shared.fees import taker_fee; from shared.constants import COIN_CONFIGS; from shared.db import get_connection; print('shared OK')"`
Expected: "shared OK"

- [ ] **Step 3: Commit**

```bash
cd /Users/jackreid/go/polymarket-agent && git add -A && git commit -m "chore: remove dead code (Bayesian engine, old autoresearch, researcher stubs)"
```

---

## Phase 3: New Threshold Strategy

### Task 5: Create strategies/threshold.py

**Files:**
- Create: `strategies/threshold.py`
- Create: `tests/test_threshold.py`

- [ ] **Step 1: Write the failing test**

```python
# tests/test_threshold.py
import json
from pathlib import Path
from strategies.threshold import ThresholdStrategy


def test_signal_up():
    s = ThresholdStrategy("btc", move_threshold=0.08, max_entry=0.55)
    result = s.check_signal(price_move_pct=0.10, token_price_up=0.50, token_price_down=0.52)
    assert result == "Up"


def test_signal_down():
    s = ThresholdStrategy("btc", move_threshold=0.08, max_entry=0.55)
    result = s.check_signal(price_move_pct=-0.10, token_price_up=0.52, token_price_down=0.50)
    assert result == "Down"


def test_no_signal_below_threshold():
    s = ThresholdStrategy("btc", move_threshold=0.08, max_entry=0.55)
    result = s.check_signal(price_move_pct=0.05, token_price_up=0.50, token_price_down=0.50)
    assert result is None


def test_skip_when_entry_too_expensive():
    s = ThresholdStrategy("btc", move_threshold=0.08, max_entry=0.55)
    result = s.check_signal(price_move_pct=0.10, token_price_up=0.70, token_price_down=0.32)
    assert result == "SKIP"


def test_from_config():
    s = ThresholdStrategy.from_config("btc", {"move_threshold": 0.08, "max_entry": 0.55})
    assert s.coin == "btc"
    assert s.move_threshold == 0.08


def test_from_strategy_results():
    results_path = Path("backtesting/strategy_results.json")
    if not results_path.exists():
        return
    data = json.loads(results_path.read_text())
    for coin, info in data.items():
        params = info["best_strategy"]["params"]
        s = ThresholdStrategy.from_config(coin, {
            "move_threshold": params.get("move", 0.08),
            "max_entry": params.get("max_entry", 0.55),
        })
        assert s.move_threshold > 0
        assert s.max_entry > 0
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cd /Users/jackreid/go/polymarket-agent && python -m pytest tests/test_threshold.py -v`
Expected: FAIL

- [ ] **Step 3: Write implementation**

```python
# strategies/threshold.py
"""Simple threshold strategy for latency arbitrage.

Detects when a coin's price moves > threshold% and the Polymarket
order book hasn't repriced yet (directional token still cheap).
"""
from __future__ import annotations

from dataclasses import dataclass


@dataclass
class ThresholdStrategy:
    """Buy directional token when price move exceeds threshold and entry is cheap."""

    coin: str
    move_threshold: float
    max_entry: float

    def check_signal(
        self,
        price_move_pct: float,
        token_price_up: float,
        token_price_down: float,
    ) -> str | None:
        """Check if entry conditions are met.

        Returns:
            "Up" or "Down": direction to trade
            "SKIP": threshold crossed but entry too expensive (stop scanning)
            None: threshold not yet crossed (keep watching)
        """
        if abs(price_move_pct) < self.move_threshold:
            return None

        if price_move_pct > 0:
            direction, entry = "Up", token_price_up
        else:
            direction, entry = "Down", token_price_down

        if entry <= 0 or entry > self.max_entry:
            return "SKIP"

        return direction

    @classmethod
    def from_config(cls, coin: str, config: dict) -> ThresholdStrategy:
        return cls(
            coin=coin,
            move_threshold=config.get("move_threshold", 0.08),
            max_entry=config.get("max_entry", 0.55),
        )
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cd /Users/jackreid/go/polymarket-agent && python -m pytest tests/test_threshold.py -v`
Expected: PASS (6 tests)

- [ ] **Step 5: Commit**

```bash
cd /Users/jackreid/go/polymarket-agent && git add strategies/threshold.py tests/test_threshold.py && git commit -m "feat: add ThresholdStrategy replacing Bayesian engine"
```

---

### Task 6: Update strategy_config.py and strategy_config.json

**Files:**
- Modify: `strategies/strategy_config.py`
- Modify: `strategy_config.json`

- [ ] **Step 1: Rewrite strategy_config.py**

Replace the entire `SignalConfig` class with `ThresholdConfig`. Keep `ExecutionConfig`, `RiskConfig`, `PaperConfig`, `StrategyConfig` but update `StrategyConfig.signal` to be `ThresholdConfig`. Read `strategies/strategy_config.py` first, then replace `SignalConfig` with:

```python
@dataclass
class ThresholdConfig:
    move_threshold: float = 0.08
    max_entry: float = 0.55
```

Update `StrategyConfig` to use `ThresholdConfig` instead of `SignalConfig`. Update `load_strategy_config()` to handle the new `coins` key in JSON.

- [ ] **Step 2: Rewrite strategy_config.json**

```json
{
    "version": 3,
    "coins": {
        "btc": {"move_threshold": 0.08, "max_entry": 0.55},
        "eth": {"move_threshold": 0.15, "max_entry": 0.55},
        "sol": {"move_threshold": 0.08, "max_entry": 0.55}
    },
    "risk": {
        "max_position_usd": 50,
        "max_position_pct": 0.10,
        "daily_loss_limit_pct": 0.25,
        "kill_balance_usd": 10,
        "max_concurrent_positions": 20,
        "loss_cooldown_trades": 5,
        "loss_cooldown_seconds": 300,
        "kelly_multiplier": 0.25
    },
    "paper": {
        "enabled": true,
        "starting_balance": 100.0
    }
}
```

- [ ] **Step 3: Update test_strategy_config.py**

Read `tests/test_strategy_config.py`, update to test `ThresholdConfig` instead of `SignalConfig`.

- [ ] **Step 4: Run tests**

Run: `cd /Users/jackreid/go/polymarket-agent && python -m pytest tests/test_strategy_config.py -v`
Expected: PASS

- [ ] **Step 5: Commit**

```bash
cd /Users/jackreid/go/polymarket-agent && git add strategies/strategy_config.py strategy_config.json tests/test_strategy_config.py && git commit -m "feat: replace SignalConfig with ThresholdConfig, per-coin strategy params"
```

---

### Task 7: Wire ThresholdStrategy into core/engine.py

**Files:**
- Modify: `core/engine.py`
- Modify: `main.py`
- Modify: `core/btc_resolution.py` (rename concept, use shared.fees)

- [ ] **Step 1: Read core/engine.py fully**

Understand the complete flow: `run()` → connects Binance WS → `_on_binance_trade()` updates signal → `_check_entry()` decides trade → `_execute_paper_trade()`.

- [ ] **Step 2: Replace BayesianSignalEngine with ThresholdStrategy**

In `core/engine.py`:
- Remove `from strategies.btc_sniper import BayesianSignalEngine`
- Add `from strategies.threshold import ThresholdStrategy`
- Replace `self._signal = BayesianSignalEngine(cfg.signal)` with `self._strategy = ThresholdStrategy.from_config("btc", cfg.coins.get("btc", {}))`
- Simplify `_on_binance_trade()`: just track `self._window_open_price` and current BTC price. Remove log-odds, OFI, microprice, acceleration updates.
- Simplify `_check_entry()`: compute `move_pct = (current - open) / open * 100`, call `self._strategy.check_signal(move_pct, price_up, price_down)`. If "Up"/"Down", proceed with Kelly sizing. If "SKIP" or None, skip.
- Remove all references to `engine.set_state()`, `engine.p_up`, `engine.confident`, `engine.direction`.

- [ ] **Step 3: Update main.py**

Remove `from strategies.btc_sniper import BayesianSignalEngine`. Update config reload handler to reload `ThresholdStrategy` from config.

- [ ] **Step 4: Update core/btc_resolution.py to use shared.fees**

Replace the local `_taker_fee()` and `TAKER_FEE_RATE` with `from shared.fees import taker_fee_usd`.

- [ ] **Step 5: Run existing tests**

Run: `cd /Users/jackreid/go/polymarket-agent && python -m pytest tests/test_btc_resolution.py tests/test_btc_risk.py -v`
Expected: PASS (some may need import fixes)

- [ ] **Step 6: Commit**

```bash
cd /Users/jackreid/go/polymarket-agent && git add core/ main.py && git commit -m "feat: wire ThresholdStrategy into engine, remove Bayesian signal"
```

---

## Phase 4: Consolidate Backtesting

### Task 8: Create backtesting/precompute.py (extract from autoresearch.py)

**Files:**
- Create: `backtesting/precompute.py`

- [ ] **Step 1: Extract PrecomputedMarket and precompute_market() from autoresearch.py**

Move lines defining `PrecomputedMarket` dataclass, `precompute_market()` function, and `load_and_precompute()` function into `backtesting/precompute.py`. Update `load_and_precompute()` to use `shared.db` for connection management.

- [ ] **Step 2: Verify import works**

Run: `cd /Users/jackreid/go/polymarket-agent && python -c "from backtesting.precompute import load_and_precompute; print('OK')"`

- [ ] **Step 3: Commit**

```bash
cd /Users/jackreid/go/polymarket-agent && git add backtesting/precompute.py && git commit -m "refactor: extract precompute module from autoresearch"
```

---

### Task 9: Create backtesting/simulator.py (extract from autoresearch.py)

**Files:**
- Create: `backtesting/simulator.py`

- [ ] **Step 1: Extract simulate_strategy() and strategy grid builder**

Move `simulate_strategy()`, `taker_fee()` (replace with `from shared.fees import taker_fee`), and the strategy definition functions into `backtesting/simulator.py`. Create a `build_strategy_grid()` function that returns the list of `(name, params, fn)` tuples.

- [ ] **Step 2: Verify import works**

Run: `cd /Users/jackreid/go/polymarket-agent && python -c "from backtesting.simulator import simulate_strategy, build_strategy_grid; print('OK')"`

- [ ] **Step 3: Commit**

```bash
cd /Users/jackreid/go/polymarket-agent && git add backtesting/simulator.py && git commit -m "refactor: extract simulator module from autoresearch"
```

---

### Task 10: Create backtesting/research.py and backtesting/fetcher.py

**Files:**
- Create: `backtesting/research.py`
- Create: `backtesting/fetcher.py`

- [ ] **Step 1: Create research.py**

Slim version of `run_autoresearch()` that imports from `precompute` and `simulator`. Keep the CLI `main()` function. Should be ~120 lines.

- [ ] **Step 2: Create fetcher.py**

Merge `historical_data.py`, `bulk_fetch.py`, `fetch_multicoin.py`, `fast_fetch.py` into a single `CoinDataFetcher` class using `shared.db` and `shared.constants`. Key methods: `fetch_headers()`, `fetch_snapshots()`, `fetch_all()`. Include 429 retry logic. Should be ~200 lines.

- [ ] **Step 3: Verify both work**

Run: `cd /Users/jackreid/go/polymarket-agent && python -c "from backtesting.research import run_autoresearch; from backtesting.fetcher import CoinDataFetcher; print('OK')"`

- [ ] **Step 4: Delete old files**

```bash
cd /Users/jackreid/go/polymarket-agent
rm -f backtesting/autoresearch.py backtesting/historical_data.py backtesting/bulk_fetch.py
rm -f backtesting/fetch_multicoin.py backtesting/fast_fetch.py
rm -f backtesting/kelly_projection.py backtesting/stochastic_projection.py
```

- [ ] **Step 5: Update imports in remaining files**

Update `backtesting/run_all_research.py`, `backtesting/run_backtest_all.py`, `backtesting/visualize.py`, `backtesting/validate_latency_arb.py` to import from the new modules.

- [ ] **Step 6: Commit**

```bash
cd /Users/jackreid/go/polymarket-agent && git add -A && git commit -m "refactor: consolidate backtesting into focused modules"
```

---

### Task 11: Create backtesting/projection.py

**Files:**
- Create: `backtesting/projection.py`

- [ ] **Step 1: Merge stochastic_projection.py and kelly_projection.py**

Create a `MonteCarloSimulator` class with methods: `extract_trades()`, `run()`, `generate_report()`. Use `shared.fees` for fee calculation. Include slippage model. Should be ~250 lines.

- [ ] **Step 2: Update visualize.py to import from projection.py**

- [ ] **Step 3: Verify**

Run: `cd /Users/jackreid/go/polymarket-agent && python -c "from backtesting.projection import MonteCarloSimulator; print('OK')"`

- [ ] **Step 4: Commit**

```bash
cd /Users/jackreid/go/polymarket-agent && git add backtesting/projection.py backtesting/visualize.py && git commit -m "refactor: merge Monte Carlo into projection module"
```

---

## Phase 5: Paper Trading Setup

### Task 12: Update engine for multi-coin paper trading

**Files:**
- Modify: `core/engine.py`
- Modify: `clients/binance_ws.py`
- Modify: `clients/market_scanner.py`

- [ ] **Step 1: Parameterize binance_ws.py for multi-coin**

Read `clients/binance_ws.py`. Add a `symbol` parameter to `BinanceWSClient.__init__()` (default "btcusdt"). Replace hardcoded "btcusdt" with `self.symbol`. The WS URL construction should use the symbol.

- [ ] **Step 2: Generalize market_scanner.py for ETH/SOL**

Read `clients/market_scanner.py`. Update slug generation to use `shared.constants.SLUG_PATTERNS`. Add a `coin` parameter to `MarketWindowScanner.__init__()`.

- [ ] **Step 3: Update engine.py to accept coin parameter**

The engine should be instantiatable per coin: `BTCTradingEngine(config, coin="btc")`. It creates the appropriate `BinanceWSClient(symbol=...)`, `MarketWindowScanner(coin=...)`, and `ThresholdStrategy(coin=...)`.

- [ ] **Step 4: Update main.py to optionally run multi-coin**

Add `--coin` CLI arg. Default to "btc" for backward compatibility.

- [ ] **Step 5: Test paper trading mode**

Run: `cd /Users/jackreid/go/polymarket-agent && python main.py --config strategy_config.json --coin btc` (should start and connect, Ctrl+C to stop)

- [ ] **Step 6: Commit**

```bash
cd /Users/jackreid/go/polymarket-agent && git add -A && git commit -m "feat: multi-coin support for paper trading"
```

---

### Task 13: Order placement pipeline test

**Files:**
- Modify: `tests/test_order_pipeline.py`

- [ ] **Step 1: Review and update test_order_pipeline.py**

Read `tests/test_order_pipeline.py`. Update the market scanner import to use the generalized version. Ensure it tests: config check, client init, market discovery, order book fetch, balance check, and (with --live flag) place+cancel test order.

- [ ] **Step 2: Run dry-run test**

Run: `cd /Users/jackreid/go/polymarket-agent && python tests/test_order_pipeline.py`
Expected: passes config and connection checks (may fail on balance/order if no keys configured locally)

- [ ] **Step 3: Commit**

```bash
cd /Users/jackreid/go/polymarket-agent && git add tests/test_order_pipeline.py && git commit -m "test: update order pipeline test for multi-coin"
```

---

## Phase 6: Data Collector

### Task 14: Build live snapshot collector

**Files:**
- Create: `collector/__init__.py`
- Create: `collector/snapshot_recorder.py`

- [ ] **Step 1: Write the collector**

```python
# collector/snapshot_recorder.py
"""Live snapshot recorder for building historical data.

Connects to Binance WS and Polymarket CLOB WS, records price
snapshots for every active 5-minute market as it happens.

Usage:
    python collector/snapshot_recorder.py --coin btc
    python collector/snapshot_recorder.py --coin eth --coin sol
"""
from __future__ import annotations

import asyncio
import logging
import sys
import time
from pathlib import Path

sys.path.insert(0, str(Path(__file__).parent.parent))

from shared.constants import COIN_CONFIGS, db_path
from shared.db import init_coin_db
from clients.market_scanner import MarketWindowScanner
from config import Config

logger = logging.getLogger(__name__)


class SnapshotRecorder:
    """Records live market snapshots to SQLite for backtesting."""

    def __init__(self, coin: str, config: Config):
        self.coin = coin
        self.config = config
        self.db = init_coin_db(db_path(coin))
        self.scanner = MarketWindowScanner(config, coin=coin)
        self._running = False

    async def record_loop(self, interval: float = 0.5):
        """Poll active markets and record snapshots."""
        self._running = True
        logger.info(f"[{self.coin.upper()}] Starting snapshot recorder")

        while self._running:
            try:
                windows = await self.scanner.find_active_windows()
                for w in windows:
                    await self._record_window(w)
            except Exception as e:
                logger.error(f"[{self.coin.upper()}] Error: {e}")

            await asyncio.sleep(interval)

    async def _record_window(self, window):
        """Record a single snapshot for a market window."""
        # Store market metadata
        self.db.execute(
            """INSERT OR IGNORE INTO markets
               (market_id, slug, market_type, start_time, end_time,
                price_start, price_end, winner, final_volume, final_liquidity)
               VALUES (?, ?, '5m', ?, ?, NULL, NULL, NULL, NULL, NULL)""",
            (window.market_id, window.slug, window.start_time, window.end_time),
        )

        # Record current prices
        self.db.execute(
            """INSERT OR IGNORE INTO snapshots
               (market_id, time, price, price_up, price_down)
               VALUES (?, ?, ?, ?, ?)""",
            (window.market_id, time.strftime("%Y-%m-%dT%H:%M:%S.%fZ"),
             None, window.price_up, window.price_down),
        )
        self.db.commit()

    def stop(self):
        self._running = False
        self.db.close()


async def main():
    import argparse
    logging.basicConfig(level=logging.INFO, format="%(asctime)s %(levelname)s %(message)s")
    parser = argparse.ArgumentParser()
    parser.add_argument("--coin", action="append", default=["btc"])
    args = parser.parse_args()

    config = Config()
    recorders = [SnapshotRecorder(c, config) for c in args.coin]

    try:
        await asyncio.gather(*(r.record_loop() for r in recorders))
    except KeyboardInterrupt:
        for r in recorders:
            r.stop()


if __name__ == "__main__":
    asyncio.run(main())
```

Note: This is a skeleton. The full implementation needs Binance WS integration for live BTC/ETH/SOL prices, and Polymarket CLOB WS for token prices. Adapt based on reading the existing `clients/binance_ws.py` and `clients/polymarket_ws.py`.

- [ ] **Step 2: Commit**

```bash
cd /Users/jackreid/go/polymarket-agent && git add collector/ && git commit -m "feat: add live snapshot collector for building historical data"
```

---

## Phase 7: Supabase Migration

### Task 15: Add Supabase client for shared data access

**Files:**
- Create: `shared/supabase_client.py`

- [ ] **Step 1: Install supabase-py**

```bash
cd /Users/jackreid/go/polymarket-agent && pip install supabase
```

- [ ] **Step 2: Write Supabase client**

Create `shared/supabase_client.py` that wraps Supabase operations for markets and snapshots tables. Mirror the SQLite schema. Key methods: `upsert_market()`, `upsert_snapshots()`, `load_markets()`, `load_snapshots()`. Read Supabase URL and key from env vars `SUPABASE_URL` and `SUPABASE_KEY`.

- [ ] **Step 3: Create Supabase tables**

SQL to create the schema in Supabase dashboard:
```sql
CREATE TABLE markets (
    market_id TEXT PRIMARY KEY,
    coin TEXT NOT NULL,
    slug TEXT, market_type TEXT,
    start_time TIMESTAMPTZ, end_time TIMESTAMPTZ,
    price_start FLOAT, price_end FLOAT,
    winner TEXT, final_volume FLOAT, final_liquidity FLOAT
);

CREATE TABLE snapshots (
    market_id TEXT,
    time TIMESTAMPTZ,
    price FLOAT, price_up FLOAT, price_down FLOAT,
    PRIMARY KEY (market_id, time)
);

CREATE TABLE strategy_results (
    coin TEXT PRIMARY KEY,
    strategy_name TEXT,
    params JSONB,
    train_win_rate FLOAT,
    test_win_rate FLOAT,
    total_trades INT,
    updated_at TIMESTAMPTZ DEFAULT NOW()
);
```

- [ ] **Step 4: Add sync script**

Create a script that syncs local SQLite data to Supabase: `python shared/sync_to_supabase.py --coin btc`

- [ ] **Step 5: Commit**

```bash
cd /Users/jackreid/go/polymarket-agent && git add shared/supabase_client.py && git commit -m "feat: add Supabase client for shared data access"
```

---

## Phase 8: Hetzner Deployment

### Task 16: Update Dockerfile and deploy

**Files:**
- Modify: `Dockerfile` (if exists)
- Create: `docker-compose.yml` (if needed)

- [ ] **Step 1: Read existing Dockerfile**

Check what's already there. The handoff mentions Docker is already configured on Hetzner (188.34.177.202).

- [ ] **Step 2: Update Dockerfile for new structure**

Ensure it includes the `shared/`, `strategies/`, `collector/` directories. Update entrypoint to support `--coin` parameter.

- [ ] **Step 3: Create autoresearch deployment config**

Write a `docker-compose.yml` or a deploy script that runs:
- Paper trading engine (one per coin, or combined)
- Snapshot collector
- Autoresearch loop (periodic, results saved to Supabase for review)

- [ ] **Step 4: Deploy to Hetzner**

```bash
ssh -i ~/.ssh/polymarket_hetzner root@188.34.177.202 "cd /root/polymarket-btc-sniper && git pull && docker compose up -d"
```

- [ ] **Step 5: Verify health**

```bash
ssh -i ~/.ssh/polymarket_hetzner root@188.34.177.202 "curl -s localhost:8080/health"
```

- [ ] **Step 6: Commit**

```bash
cd /Users/jackreid/go/polymarket-agent && git add Dockerfile docker-compose.yml && git commit -m "feat: update deployment for multi-coin + autoresearch"
```

---

## Phase 9: Autoresearch Loop

### Task 17: Build continuous autoresearch runner

**Files:**
- Create: `autoresearch/runner.py`
- Create: `autoresearch/program.md`

- [ ] **Step 1: Write the new program.md**

Replace the old Bayesian-focused program.md with one targeting threshold strategy optimization. The agent should:
- Read current `strategy_results.json`
- Propose new threshold/max_entry values or feature filters
- Backtest against the latest data
- Save results as "candidates" for human review (not auto-deploy)
- Use `backtesting/research.py` and `backtesting/projection.py`

- [ ] **Step 2: Write runner.py**

A script that:
- Loads current best strategy per coin
- Runs autoresearch with expanded grid (including higher thresholds for volatile coins)
- Compares results to current best
- If improved, saves candidate to `autoresearch/candidates/` with timestamp
- Posts summary to Slack via `core/notifier.py`
- Intended to be run via Claude trigger on Hetzner

- [ ] **Step 3: Commit**

```bash
cd /Users/jackreid/go/polymarket-agent && git add autoresearch/ && git commit -m "feat: continuous autoresearch loop with candidate review"
```

---

## Phase 10: Final Cleanup

### Task 18: Clean up config.py and remaining dead code

**Files:**
- Modify: `config.py`
- Modify: `core/engine.py` (any remaining references)
- Delete: any remaining dead test files

- [ ] **Step 1: Remove dead config toggles from config.py**

Read `config.py`. Remove: `ENABLE_WEATHER`, `ENABLE_ARBITRAGE`, `ENABLE_COPY_TRADING`, `ENABLE_TREND_DETECTION`, `WEATHER_CITIES`, `COPY_TRADING_WALLETS`, and any other dead references.

- [ ] **Step 2: Run full test suite**

Run: `cd /Users/jackreid/go/polymarket-agent && python -m pytest tests/ -v`
Expected: all remaining tests pass

- [ ] **Step 3: Verify paper trading starts**

Run: `cd /Users/jackreid/go/polymarket-agent && timeout 10 python main.py --config strategy_config.json --coin btc 2>&1 || true`
Expected: starts up, connects to Binance WS, begins scanning for markets

- [ ] **Step 4: Final commit**

```bash
cd /Users/jackreid/go/polymarket-agent && git add -A && git commit -m "chore: final cleanup, remove dead config toggles"
```

---

## Summary

| Phase | Tasks | Purpose |
|-------|-------|---------|
| 1 | 1-3 | Extract shared utilities (fees, DB, constants) |
| 2 | 4 | Delete dead code (~1,700 lines) |
| 3 | 5-7 | New threshold strategy + wire into engine |
| 4 | 8-11 | Consolidate backtesting modules |
| 5 | 12-13 | Paper trading with order pipeline test |
| 6 | 14 | Live data collector |
| 7 | 15 | Supabase migration |
| 8 | 16 | Hetzner deployment |
| 9 | 17 | Autoresearch loop |
| 10 | 18 | Final cleanup |
