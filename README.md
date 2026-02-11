# Polymarket Trading Agent

Autonomous trading agent that scans Polymarket prediction markets, finds mispricings using data-driven strategies, and executes trades.

The agent pays for its own Claude API inference from trading profits. If the balance hits zero, the agent dies.

## Architecture

```
                        +------------------+
                        |     main.py      |
                        |   CLI entry      |
                        | --live/--paper/  |
                        |   --backtest     |
                        +--------+---------+
                                 |
                        +--------v---------+
                        |   TradingEngine  |
                        |   core/engine.py |
                        +--------+---------+
                                 |
              +------------------+------------------+
              |                  |                  |
     +--------v-------+ +-------v--------+ +-------v--------+
     |  Market Scan   | |   Strategies   | |  Risk Manager  |
     |  (Gamma API)   | |  (evaluate)    | |  (Kelly + kill)|
     +----------------+ +-------+--------+ +-------+--------+
                                |                   |
                    +-----------+-----------+       |
                    |           |           |       |
              +-----v---+ +----v----+ +----v----+  |
              | Weather | |  Arb    | |  Copy   |  |
              | NOAA +  | | YES+NO | | Whale   |  |
              | Claude  | | < $1   | | Mirror  |  |
              +---------+ +---------+ +---------+  |
                                                    |
                        +---------------------------v--+
                        |        Execute Trade         |
                        |     (CLOB API / Paper)       |
                        +-------------+----------------+
                                      |
                        +-------------v----------------+
                        |     SQLite + MEMORY.md       |
                        |   Trade log + AI learning    |
                        +------------------------------+
```

### Main Loop (every 10 minutes)

1. **Scan** - Fetch 500-1000 active markets from Polymarket Gamma API
2. **Evaluate** - Each strategy analyses markets and produces `Signal` objects
3. **Filter** - Risk manager checks minimum edge threshold (8%) and confidence
4. **Size** - Kelly Criterion calculates position size (max 6% of bankroll)
5. **Execute** - Place orders via CLOB API (or simulate in paper mode)
6. **Record** - Log trades to SQLite, sync learnings to MEMORY.md
7. **Check** - If balance < kill threshold, agent shuts down

## Project Structure

```
polymarket-agent/
├── main.py                  # CLI entry point
├── config.py                # Settings from .env
├── requirements.txt
├── .env.example
│
├── core/                    # Engine + business logic
│   ├── engine.py            # Main trading loop
│   ├── portfolio.py         # Balance, positions, P&L tracking
│   ├── risk.py              # Kelly Criterion, exposure limits, kill switch
│   └── memory.py            # SQLite persistence + MEMORY.md sync
│
├── clients/                 # External API integrations
│   ├── polymarket.py        # CLOB (trading) + Gamma (market discovery)
│   ├── weather.py           # NOAA GEFS ensemble via Open-Meteo
│   └── claude_client.py     # Claude Opus for fair value estimation
│
├── strategies/              # Alpha generators (pluggable)
│   ├── base.py              # Abstract Strategy interface
│   ├── weather.py           # NOAA ensemble probability vs market price
│   ├── arbitrage.py         # Binary complement (YES+NO < $1)
│   └── copy_trading.py      # Mirror high win-rate wallets
│
├── backtesting/             # Strategy validation
│   ├── engine.py            # Replay resolved markets
│   ├── data.py              # PolyBackTest + Gamma historical data
│   └── paper.py             # Simulated fills with slippage model
│
└── models/                  # Data structures
    ├── market.py             # Market, OrderBook, PricePoint
    └── trade.py              # Signal, Trade, Position, TradeResult
```

## Strategies

### 1. Weather (highest conviction)

The edge: NOAA runs 21 independent weather model simulations (GEFS ensemble). Most Polymarket traders bet on weather based on gut feeling. The bot has government satellites.

```
NOAA GEFS (21 ensemble members)
    → Open-Meteo API (clean JSON, no GRIB parsing)
    → Probability distribution per temperature bucket
    → Compare against Polymarket price
    → Blend: 70% ensemble data + 30% Claude refinement
    → Trade when edge > threshold
```

Example: 18/21 ensemble members predict NYC > 80F. That's 86% probability. Polymarket prices it at 62%. Edge = 24%. Buy YES.

### 2. Binary Complement Arbitrage (risk-free)

If the best ask for YES + best ask for NO on the same market totals less than $1.00, buying both guarantees a profit. One side must resolve to $1.00.

The bot scans all markets and emits paired signals (buy YES + buy NO) when the complement cost is below $1.00.

### 3. Whale Copy Trading (smart money)

Discovers wallets with high win rates from the Polymarket leaderboard, monitors their trades, and mirrors BUY positions with position sizing scaled by the whale's historical win rate.

## Risk Management

- **Kelly Criterion**: Position size = f(edge, odds, confidence). Fractional Kelly (never more than half Kelly) for safety
- **Max position**: 6% of bankroll per trade (configurable)
- **Minimum edge**: 8% mispricing required before trading (configurable)
- **Minimum confidence**: 30% model confidence floor
- **Kill switch**: Agent shuts down if balance drops below threshold (default $5)
- **API cost tracking**: Claude inference costs are deducted from P&L

## Prerequisites

- **Python 3.11+** (uses `match` statements, `|` union types)
- **An Anthropic API key** for Claude Opus (the agent's brain)
- **A Polymarket account** with API credentials (for live trading)
- **USDC on Polygon** (for live trading - not needed for paper trading or backtesting)

## Setup

### 1. Clone and install

```bash
git clone https://github.com/0xj7r/polymarket-agent.git
cd polymarket-agent

# Create virtual environment (recommended)
python -m venv .venv
source .venv/bin/activate  # or .venv\Scripts\activate on Windows

# Install dependencies
pip install -r requirements.txt
```

### 2. Configure environment

```bash
cp .env.example .env
```

Edit `.env` with your credentials:

```bash
# Required for all modes
ANTHROPIC_API_KEY=sk-ant-...

# Required for live trading only
POLYMARKET_PRIVATE_KEY=0x...
```

### 3. Wallet setup (for live trading)

Polymarket is a decentralized exchange on the Polygon network. To trade with real money:

1. **Create a wallet** - Use MetaMask or any Ethereum wallet. Export the private key and put it in `.env`
2. **Get USDC on Polygon** - You can:
   - Bridge USDC from Ethereum mainnet to Polygon via [Polygon Bridge](https://portal.polygon.technology/bridge)
   - Buy USDC directly on Polygon via an exchange (Coinbase, Binance) and withdraw to your wallet address
   - Use a fiat onramp like MoonPay or Transak
3. **Get POL for gas** - You need a small amount of POL (Polygon's native token) for transaction fees. ~$1 worth is plenty. Most exchanges let you withdraw POL directly to Polygon
4. **Polymarket API credentials** - Visit [Polymarket](https://polymarket.com), connect your wallet, and the bot will derive API credentials from your private key automatically

### 4. Verify setup

```bash
# Paper trading mode (no wallet needed, uses real market data)
python main.py --log-level DEBUG
```

You should see the agent scanning markets and producing signals without placing real orders.

## Usage

```bash
# Paper trading (default - no real money, real market data)
python main.py

# Backtest against historical resolved markets
python main.py --backtest

# Live trading (real money - requires funded wallet)
python main.py --live

# Debug logging
python main.py --log-level DEBUG
```

### Recommended workflow

1. **Backtest first** - Run `python main.py --backtest` to validate strategy edge on historical data
2. **Paper trade** - Run `python main.py` for a few days to verify real-time signal quality. Check `trades.db` for results
3. **Go live small** - Start with $50-100: `python main.py --live`. The kill switch will shut down the agent if balance drops below $5
4. **Monitor** - Watch logs for trade execution, check portfolio snapshots in SQLite
5. **Tune** - Adjust `MIN_EDGE_THRESHOLD`, `MAX_POSITION_PCT`, and strategy toggles based on what's working

### Running on a VPS

For 24/7 operation on a cheap VPS ($4-5/month):

```bash
# Using screen or tmux
screen -S polymarket
python main.py --live
# Ctrl+A, D to detach

# Or using systemd (create /etc/systemd/system/polymarket-agent.service)
# Or using nohup
nohup python main.py --live > agent.log 2>&1 &
```

## Configuration

All settings are in `.env`. Key parameters:

| Variable                | Default                    | Description                                     |
| ----------------------- | -------------------------- | ----------------------------------------------- |
| `PAPER_TRADE`           | `true`                     | Paper trading mode (no real execution)          |
| `MAX_POSITION_PCT`      | `0.06`                     | Max position size as fraction of bankroll       |
| `MIN_EDGE_THRESHOLD`    | `0.08`                     | Minimum mispricing to trade                     |
| `KILL_BALANCE_USD`      | `5.0`                      | Shut down if balance drops below this           |
| `SCAN_INTERVAL_SECONDS` | `600`                      | Time between market scans (10 min)              |
| `ENABLE_WEATHER`        | `true`                     | Enable weather strategy                         |
| `ENABLE_ARBITRAGE`      | `true`                     | Enable arbitrage strategy                       |
| `ENABLE_COPY_TRADING`   | `true`                     | Enable copy trading strategy                    |
| `WEATHER_CITIES`        | `New York,Los Angeles,...` | Comma-separated cities for weather strategy     |
| `CLAUDE_MODEL`          | `claude-opus-4-5-20250514` | Claude model for fair value estimation          |
| `DB_PATH`               | `trades.db`                | SQLite database path for trades/results         |
| `MEMORY_PATH`           | ``                         | Optional `MEMORY.md` path (empty disables sync) |

## Learning System

The agent learns from its trades across sessions:

- **SQLite** (`trades.db`) stores all trades, results, and portfolio snapshots
- **MEMORY.md** gets synced every 10 cycles with per-strategy win rates, PnL, and key learnings
- Future Claude Code sessions can read MEMORY.md to understand what worked and what didn't

## Adding New Strategies

Create a new file in `strategies/` implementing the `Strategy` interface:

```python
from strategies.base import Strategy
from models.market import Market
from models.trade import Signal

class MyStrategy(Strategy):
    @property
    def name(self) -> str:
        return "my_strategy"

    async def evaluate(self, markets: list[Market]) -> list[Signal]:
        signals = []
        # Your alpha logic here
        # Return Signal objects for markets where you see an edge
        return signals
```

Then register it in `main.py`:

```python
engine.register_strategy(MyStrategy())
```

## Planned Features

- OpenClaw integration for Telegram trade alerts and remote control
- Catalyst momentum strategy (fast repricing after breaking news)
- Favorite compounder (grind high-probability outcomes)
- Correlation hedging across linked markets
