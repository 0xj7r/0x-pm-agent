# Polymarket BTC 5-Minute Sniper — System Design

## Overview

Three independent systems that communicate through files on disk and human gates:

1. **Trading Agent** — executes trades on Bitcoin Up/Down 5-minute markets
2. **Autoresearch Loop** — optimizes strategy parameters via backtesting
3. **Autonomous Researcher** — scans internet for new edges and inefficiencies

Each runs at its own cadence. No system directly triggers another.

```
[Researcher]  ──writes──→  research_insights/*.md  (async, daily)
                                    │
                          human reads, or LLM reads as context
                                    │
[Autoresearch] ──reads──→  research_insights/ + historical data
               ──mutates──→ strategy candidate
               ──backtests──→ results.tsv
                                    │
                          human reviews, decides to promote
                                    │
[Trading Agent] ──reads──→  strategy_config.json (frozen, changes on promotion only)
                ──executes──→ Polymarket CLOB
                ──logs──→ trades.db (feeds back as new historical data)
```

---

## System A: Trading Agent

### Core Strategy: Asymmetric Bayesian Sniping

Buy cheap tokens (2-5 cents) on Polymarket's Bitcoin Up/Down 5-minute markets when we detect the outcome from real-time Binance data before the Polymarket book reprices. Asymmetric payoff: max loss is the token price (2 cents), max gain is 98 cents per share (4,900% return).

Expected value math:
- At 10% hit rate: EV = 0.10 x 0.98 - 0.90 x 0.02 = $0.08/share (400% ROI)
- At 5% hit rate: EV = 0.05 x 0.98 - 0.95 x 0.02 = $0.03/share (150% ROI)
- Dynamic taker fee at 2 cents: 0.072 x 0.02 x 0.98 = 0.14% (negligible)

### Architecture

```
┌──────────────────────────────────────────────────────────────┐
│                      TRADING AGENT                           │
│                                                              │
│  ┌─────────────┐   ┌──────────────┐   ┌─────────────────┐   │
│  │ Data Feeds   │──→│ Signal Engine │──→│ Execution Engine│   │
│  │              │   │              │   │                 │   │
│  │ - Binance WS │   │ - Bayesian   │   │ - Order mgmt   │   │
│  │ - Polymarket │   │   posterior   │   │ - Kelly sizing │   │
│  │   CLOB       │   │ - Log-odds   │   │ - CLOB submit  │   │
│  │ - Gamma API  │   │   updating   │   │ - Fill monitor │   │
│  └─────────────┘   └──────────────┘   └─────────────────┘   │
│                                                              │
│  ┌─────────────┐   ┌──────────────┐   ┌─────────────────┐   │
│  │ Market       │   │ Risk Manager │   │ Persistence     │   │
│  │ Discovery    │   │              │   │                 │   │
│  │              │   │ - Daily loss │   │ - SQLite trades │   │
│  │ - Active     │   │ - Kill switch│   │ - Event log     │   │
│  │   window     │   │ - Cooldowns  │   │ - Snapshots     │   │
│  │ - Token IDs  │   │ - Max pos    │   │ - Audit trail   │   │
│  └─────────────┘   └──────────────┘   └─────────────────┘   │
│                                                              │
│  ┌──────────────────────────────────────────────────────────┐│
│  │ Supervisor (systemd) — restart on crash, heartbeat, PID  ││
│  └──────────────────────────────────────────────────────────┘│
└──────────────────────────────────────────────────────────────┘
```

### Signal Engine

**Data source**: Binance WebSocket — `btcusdt@trade` (individual trades) and `btcusdt@depth@100ms` (order book snapshots).

**Bayesian posterior** maintained in log-odds space, updated on every Binance trade:

```python
# Start of each 5-minute window
log_odds = 0.0  # logit(0.5) = 0, uninformative prior

# On each Binance trade/LOB update:
log_odds += w1 * order_flow_imbalance    # net buy vs sell (isBuyerMaker field)
log_odds += w2 * microprice_deviation    # LOB-weighted fair price vs mid
log_odds += w3 * price_delta             # BTC move since window open (strongest signal)
log_odds += w4 * acceleration            # second derivative of price

P_up = 1 / (1 + exp(-log_odds))
```

Weights w1-w4 are read from `strategy_config.json`. Initial values hand-tuned, then optimized by autoresearch loop.

**Why log-odds**: Additive updates, numerically stable, no clamping needed. Sequential Bayesian updating becomes simple addition.

### Execution Engine

**Market discovery**: Poll Gamma API every 30 seconds for active Bitcoin Up/Down 5-minute markets. Parse slug pattern (`bitcoin-up-or-down-{date}-{time}`). Cache token IDs (YES/NO) for the current and next window.

**Entry logic** (runs every 100ms when signal is active):

```
1. Check P_up (or P_down = 1 - P_up)
2. If max(P_up, P_down) > CONFIDENCE_THRESHOLD:
   a. Determine winning side (UP if P_up > threshold, DOWN otherwise)
   b. Fetch order book for the winning side's token
   c. Check if cheap orders exist (price <= MAX_ENTRY_PRICE, default 5 cents)
   d. If yes: calculate size via asymmetric Kelly, submit market order to sweep
   e. If no cheap orders: check if mid-price orders exist with sufficient edge
3. Log decision (including "no action" decisions) to event log
```

**Two entry windows** (configurable, autoresearch determines which works better):
- Early snipe (t=0 to t=60s): Catch the initial move before anyone reprices
- Late snipe (t=270 to t=295s): Direction locked in, sweep remaining cheap orders

**Asymmetric Kelly sizing**:

```python
# For buying token at price c with estimated probability p of winning:
edge = p - c
kelly_fraction = edge / (1 - c)  # standard binary Kelly
adjusted = kelly_fraction * KELLY_MULTIPLIER  # default 0.25 (quarter-Kelly)

# Asymmetric adjustment: downside is capped at c per share
# When c = 0.02, max loss = 2% of face value — can size more aggressively
if c <= 0.05:
    adjusted *= CHEAP_TOKEN_MULTIPLIER  # default 2.0 (half-Kelly for cheap tokens)

size_usd = min(adjusted * bankroll, MAX_POSITION_USD, available_liquidity_at_price)
```

**Order submission**: Use py-clob-client to place GTC limit orders at or slightly above the resting ask. For sweeping, walk up the book and take all shares up to MAX_ENTRY_PRICE.

### Risk Manager

Reuse existing risk framework from weather agent, adapted:

| Parameter | Default | Purpose |
|-----------|---------|---------|
| MAX_POSITION_USD | $50 | Max per single trade (paper), $500 live |
| MAX_POSITION_PCT | 10% | Max % of bankroll per trade |
| DAILY_LOSS_LIMIT_PCT | 25% | Stop trading if daily losses exceed this |
| KILL_BALANCE_USD | $10 | Kill switch — halt all trading |
| MAX_CONCURRENT_POSITIONS | 20 | Max open positions at once |
| LOSS_COOLDOWN_TRADES | 5 | Pause after N consecutive losses |
| LOSS_COOLDOWN_SECONDS | 300 | 5-minute cooldown (one market cycle) |
| CONFIDENCE_THRESHOLD | 0.85 | Min P(direction) to enter |
| MAX_ENTRY_PRICE | 0.05 | Only buy tokens at 5 cents or below |
| KELLY_MULTIPLIER | 0.25 | Quarter-Kelly for base sizing |
| CHEAP_TOKEN_MULTIPLIER | 2.0 | Boost for cheap tokens (capped loss) |

### Market Discovery

```python
class MarketWindow:
    market_id: str
    question: str          # "Bitcoin Up or Down - March 31, 3:20AM-3:25AM ET"
    start_time: datetime   # Window open
    end_time: datetime     # Window close (resolution)
    yes_token_id: str
    no_token_id: str
    yes_price: float
    no_price: float
    status: str            # "active" | "closed" | "resolved"
```

The agent maintains a sliding window:
- `current_window`: the 5-minute market currently tradeable
- `next_window`: pre-fetched for instant transition
- Transition happens at window close; immediately start processing next window

### Persistence (SQLite)

**Tables**:

```sql
-- Every trade attempt (including rejections)
CREATE TABLE trades (
    id INTEGER PRIMARY KEY,
    market_id TEXT NOT NULL,
    window_start TEXT NOT NULL,
    window_end TEXT NOT NULL,
    direction TEXT NOT NULL,  -- 'UP' or 'DOWN'
    token_id TEXT,
    side TEXT NOT NULL,       -- 'BUY' or 'SELL'
    price REAL NOT NULL,
    size_shares REAL NOT NULL,
    size_usd REAL NOT NULL,
    confidence REAL NOT NULL, -- P(direction) at time of entry
    signal_log_odds REAL NOT NULL,
    order_id TEXT,
    status TEXT NOT NULL,     -- 'submitted' | 'filled' | 'rejected' | 'cancelled'
    paper BOOLEAN NOT NULL DEFAULT TRUE,
    created_at TEXT NOT NULL
);

-- Resolution outcomes
CREATE TABLE results (
    id INTEGER PRIMARY KEY,
    trade_id INTEGER REFERENCES trades(id),
    resolved_direction TEXT NOT NULL,  -- actual outcome
    won BOOLEAN NOT NULL,
    pnl_usd REAL NOT NULL,
    resolved_at TEXT NOT NULL
);

-- Periodic snapshots
CREATE TABLE snapshots (
    id INTEGER PRIMARY KEY,
    balance REAL NOT NULL,
    invested REAL NOT NULL,
    realized_pnl REAL NOT NULL,
    unrealized_pnl REAL NOT NULL,
    win_count INTEGER NOT NULL,
    loss_count INTEGER NOT NULL,
    win_rate REAL NOT NULL,
    total_trades INTEGER NOT NULL,
    created_at TEXT NOT NULL
);

-- Every signal engine decision (high-frequency audit log)
CREATE TABLE event_log (
    id INTEGER PRIMARY KEY,
    window_id TEXT NOT NULL,
    timestamp TEXT NOT NULL,
    event_type TEXT NOT NULL,  -- 'signal_update' | 'entry_attempt' | 'fill' | 'error'
    log_odds REAL,
    p_up REAL,
    btc_price REAL,
    details TEXT  -- JSON blob for event-specific data
);
```

### Resilience & Process Management

**The agent must not die.** Design principles:

1. **Supervisor process**: systemd unit on Hetzner with `Restart=always`, `RestartSec=5`
2. **Exception isolation**: Every component wrapped in try/except. Errors logged, never propagated to kill the main loop. Pattern from claude-code-ts analytics sink — failed events are persisted for retry, not dropped.
3. **WebSocket reconnection**: Exponential backoff with jitter. Sleep/wake detection (if gap between messages > 60s, assume connection dropped, force reconnect). Adapted from claude-code-ts WebSocketTransport pattern.
4. **Heartbeat file**: Agent writes timestamp to `heartbeat.txt` every cycle. External cron checks staleness; alerts if > 2 minutes stale.
5. **PID file**: Prevents duplicate instances.
6. **Graceful degradation**: If Binance WS drops, pause trading (don't trade blind). If Gamma API is down, continue with cached market data. If CLOB is down, queue orders for retry.
7. **Circular buffer for market data**: Keep last 1000 trades in memory (claude-code-ts CircularBuffer pattern). If WS reconnects, we have recent history to rebuild state.

**Startup sequence**:
```
1. Check PID file — exit if another instance running
2. Write PID file
3. Load strategy_config.json
4. Initialize SQLite (create tables if needed)
5. Connect Binance WebSocket (block until connected)
6. Connect Polymarket CLOB
7. Discover current market window
8. Enter main loop
```

**Main loop** (runs continuously):
```
while True:
    try:
        window = discover_current_window()  # or use cached

        # Process Binance data, update signal
        # Check entry conditions
        # Submit orders if conditions met
        # Check fills and resolutions
        # Take snapshot every N cycles

        await asyncio.sleep(0.1)  # 100ms tick
    except Exception as e:
        log_error(e)
        await asyncio.sleep(1)  # brief pause, then continue
```

### Configuration

All parameters in `strategy_config.json` (read at startup, reloaded on SIGHUP):

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

---

## System B: Autoresearch Loop

### Purpose

Optimize the trading agent's strategy parameters by proposing mutations, backtesting them, and keeping improvements. Follows the Karpathy autoresearch pattern.

### Architecture

```
┌─────────────────────────────────────────────────┐
│              AUTORESEARCH LOOP                   │
│                                                  │
│  ┌───────────┐  ┌────────────┐  ┌────────────┐  │
│  │ LLM Agent │  │ Backtester │  │ Evaluator  │  │
│  │ (Claude)  │  │            │  │            │  │
│  │           │  │ - Replay   │  │ - EV/trade │  │
│  │ - Read    │  │   history  │  │ - Hit rate │  │
│  │   results │  │ - Simulate │  │ - Sharpe   │  │
│  │ - Read    │  │   entries  │  │ - Max DD   │  │
│  │   research│  │ - Score    │  │ - Compare  │  │
│  │ - Propose │  │            │  │   baseline │  │
│  │   mutation│  │            │  │            │  │
│  └───────────┘  └────────────┘  └────────────┘  │
│                                                  │
│  Loop: mutate → backtest → evaluate → keep/reset │
└─────────────────────────────────────────────────┘
```

### How It Works

```
program.md defines the research strategy (human-written, natural language)

Loop:
  1. LLM reads: current strategy_config.json, results.tsv, research_insights/
  2. LLM proposes a mutation (change weights, thresholds, entry timing, etc.)
  3. Write mutation to strategy_candidate.json
  4. Backtester replays last N days of 5-min market data:
     a. For each historical window: fetch BTC price history from Binance API
     b. Simulate signal engine with candidate params
     c. Check if entry conditions would have been met
     d. Check if cheap tokens were available (from historical order book snapshots)
     e. Record simulated trade outcome
  5. Evaluator scores: EV per trade, hit rate, Sharpe, max drawdown
  6. If score > baseline: keep mutation, update baseline, log to results.tsv
  7. If score <= baseline: discard mutation
  8. Repeat
```

### Backtesting Data

Historical data sources:
- **Binance API**: Historical klines (1-second or 1-minute) for each 5-min window
- **Polymarket Gamma API**: Resolved markets with outcomes
- **PolyBackTest API**: Historical order book snapshots (if available)
- **Agent's own trades.db**: Real execution data once we have paper trading history

The backtester MUST account for:
- Liquidity constraints (can't buy 29K shares if book only had 1K at that price)
- Execution latency (add 200ms delay to simulate Hetzner round-trip)
- Fees (dynamic taker fee formula)
- Slippage (2-4 cents per token based on research)

### Output

```
results.tsv:
timestamp | mutation_description | ev_per_trade | hit_rate | sharpe | max_drawdown | kept
```

### Promotion Process (Human Gate)

The autoresearch loop produces candidates. A human:
1. Reviews `results.tsv` — checks the mutation log
2. Compares candidate vs current live config
3. If satisfied: copies `strategy_candidate.json` to `strategy_config.json`
4. Sends SIGHUP to the trading agent (reloads config without restart)

---

## System C: Autonomous Researcher

### Purpose

Continuously scan external sources for new trading edges, strategy ideas, market microstructure changes, and competitive intelligence. Output structured findings that humans and the autoresearch LLM can read.

### Architecture

```
┌──────────────────────────────────────────────────┐
│           AUTONOMOUS RESEARCHER                   │
│                                                   │
│  ┌────────────┐  ┌────────────┐  ┌─────────────┐ │
│  │ Scanners   │  │ Analyzer   │  │ Writer      │ │
│  │            │  │ (LLM)      │  │             │ │
│  │ - Twitter  │  │            │  │ - Markdown  │ │
│  │ - GitHub   │  │ - Filter   │  │   reports   │ │
│  │ - Poly     │  │ - Classify │  │ - Append to │ │
│  │   leaders  │  │ - Assess   │  │   feed      │ │
│  │ - On-chain │  │   novelty  │  │ - Alert if  │ │
│  │ - Reddit   │  │            │  │   urgent    │ │
│  └────────────┘  └────────────┘  └─────────────┘ │
│                                                   │
│  Cadence: daily scan, or on-demand                │
└──────────────────────────────────────────────────┘
```

### Scan Sources

1. **Twitter/X**: Search for "polymarket bot", "polymarket strategy", "polymarket arbitrage", "up or down", wallet analysis threads. Follow key accounts (@Dan1ro0, quant traders discussing Polymarket).
2. **GitHub**: New repos matching "polymarket", "prediction market bot", "binary options market maker". Track stars/forks on known repos for activity spikes.
3. **Polymarket Leaderboard**: Top wallets on crypto Up/Down markets. Track which wallets are consistently profitable. Analyze their trading patterns (timing, sizing, win rate).
4. **On-chain**: Monitor Polygon for large trades on BTC Up/Down markets. Detect new bot wallets entering the space.
5. **Fee/Rules Changes**: Monitor Polymarket docs and announcements for fee structure changes, new market types, rule updates.

### Output Format

Each scan produces a file in `research_insights/`:

```markdown
# Research Insight: {title}
Date: 2026-03-31
Source: Twitter / GitHub / Leaderboard / On-chain
Priority: high | medium | low

## Finding
{What was discovered}

## Trading Implication
{How this affects our strategy}

## Suggested Action
{What the autoresearch loop should try}

## Raw Data
{Links, screenshots, wallet addresses, code snippets}
```

### Implementation

Use gpt-researcher or a custom Claude-based agent with web search tools. Deploy as a cron job on Hetzner (daily at 6 AM UTC). Can also be triggered manually.

---

## Infrastructure & Deployment (Hetzner)

### Server Setup

```
Hetzner VPS ($5/mo):
├── systemd services:
│   ├── polymarket-agent.service    (always running)
│   ├── polymarket-researcher.timer (daily cron)
│   └── polymarket-heartbeat.timer  (every 2 min, checks agent health)
├── /opt/polymarket-agent/
│   ├── main.py
│   ├── strategy_config.json
│   ├── trades.db
│   ├── heartbeat.txt
│   ├── agent.log
│   └── ...
├── /opt/polymarket-autoresearch/
│   ├── program.md
│   ├── backtest.py
│   ├── results.tsv
│   └── ...
├── /opt/polymarket-researcher/
│   ├── researcher.py
│   ├── research_insights/
│   └── ...
└── .env (credentials, never committed)
```

### Monitoring

- Heartbeat check: cron every 2 minutes, alert via Telegram/email if stale
- Daily P&L summary: cron at midnight, posts to Telegram
- Error alerting: any ERROR-level log line triggers Telegram notification
- Disk/memory monitoring: standard Hetzner alerts

### Security

- Private key in .env, never in code or git
- .env permissions: 600 (owner-only read)
- No inbound ports except SSH (key-only, no password)
- Agent runs as unprivileged user
- SQLite WAL mode for crash safety

---

## Development Phases

### Phase 1: Signal Engine + Paper Trading
- Binance WebSocket client with reconnection
- Bayesian signal engine (P_up from log-odds)
- Market discovery from Gamma API
- Paper trade execution (simulated orders)
- SQLite persistence
- Basic risk manager
- Deploy to Hetzner, run paper trading

### Phase 2: Backtester + Autoresearch
- Historical data fetcher (Binance klines + Gamma resolved markets)
- Backtest engine (replay signal against history)
- Autoresearch harness (LLM proposes mutations, backtest evaluates)
- program.md defining the research strategy
- results.tsv tracking

### Phase 3: Autonomous Researcher
- Web search scanner (Twitter, GitHub, leaderboard)
- LLM-based analysis and classification
- Structured output to research_insights/
- Cron deployment on Hetzner

### Phase 4: Live Trading
- Switch from paper to live (human decision)
- Increase position limits gradually
- Monitor for 1 week minimum before scaling
- Add market-making mode (Approach B) if signal proves reliable

---

## What We Reuse From Existing Agent

From `polymarket-agent/`:
- `clients/polymarket.py` — CLOB client wrapper (adapt for crypto markets)
- `core/risk.py` — Risk manager framework (adjust params)
- `core/portfolio.py` — Position tracking
- `core/memory.py` — SQLite persistence patterns
- `models/trade.py` — Signal, Trade, TradeResult dataclasses (extend)
- `models/market.py` — Market, OrderBook models (extend)
- `config.py` — Environment variable loading pattern
- Test infrastructure and TDD workflow

From `claude-code-ts` patterns:
- Sink-based event routing for multi-backend logging
- WebSocket reconnection with sleep/wake detection
- Circular buffer for trade data windows
- Failed event persistence with retry

### What We Build New

- Binance WebSocket client (new data source)
- Bayesian signal engine (entirely new)
- 5-minute market window management (new timing logic)
- Asymmetric Kelly sizing (new math)
- Autoresearch harness (new system)
- Backtester for 5-min crypto markets (new)
- Autonomous researcher (new system)
