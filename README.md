# Polymarket Trading Agent

Autonomous trading agent that finds mispricings on [Polymarket](https://polymarket.com) prediction markets using data-driven strategies. Currently focused on weather temperature markets with ensemble forecast models.

## Architecture

```
main.py                     # Entry point, PID file management, signal handling
├── core/
│   ├── engine.py           # Main trading loop — scan → evaluate → execute → sleep
│   ├── portfolio.py        # Position tracking, balance management
│   ├── risk.py             # Kelly sizing, daily loss limits, cooldowns, kill switch
│   ├── analysis.py         # Post-trade analysis and P&L reporting
│   └── memory.py           # Learning storage (what worked, what didn't)
├── strategies/
│   ├── base.py             # Abstract Strategy interface
│   ├── weather.py          # NOAA ensemble vs Polymarket temperature buckets
│   ├── arbitrage.py        # Cross-market price discrepancy detection
│   └── copy_trading.py     # Follow known profitable wallets
├── clients/
│   ├── polymarket.py       # Gamma API: event discovery, bucket parsing, order execution
│   ├── weather.py          # Open-Meteo GEFS ensemble forecasts (30 members)
│   └── claude_client.py    # LLM reasoning for complex signals
├── backtesting/
│   ├── engine.py           # Historical replay engine
│   ├── paper.py            # Paper trade execution and P&L tracking
│   └── data.py             # Historical data fetching
├── models/
│   └── trade.py            # Trade, Signal, Outcome dataclasses
└── tests/                  # 101 tests — see Testing section
```

## How It Works

### Weather Strategy (primary)

The core edge: NOAA's Global Ensemble Forecast System (GEFS) has 30 independent weather model runs. We compare their probability distribution against Polymarket's crowd-sourced prices.

**Flow:**
1. **Discover** — Scan Gamma API for active weather temperature markets (13 cities, today + tomorrow)
2. **Forecast** — Fetch 30-member GEFS ensemble from Open-Meteo for each city's target date
3. **Compare** — For each temperature bucket (e.g. "4°C", "36-37°F", "46°F or higher"):
   - Calculate what fraction of ensemble members predict a high temp in that bucket
   - Compare against the market's YES price
   - If `|ensemble_prob - market_price| > MIN_EDGE_THRESHOLD` → signal
4. **Size** — Kelly criterion with configurable max position and portfolio limits
5. **Execute** — Place trade (paper or live), deduplicate by market_id + outcome
6. **Track** — Log to SQLite with full rationale, monitor resolution

**Bucket probability calculation:**
- Polymarket buckets are integer degrees ("4°C", "36-37°F", "6°C or higher")
- Each ensemble member's max temp is converted to the bucket's native unit (°C or °F)
- Rounded to nearest integer (matches Weather Underground resolution source)
- Counted: `probability = members_in_bucket / total_members`

**Cities:** Seoul, London, Toronto, NYC, Atlanta, Ankara, Chicago, Dallas, Miami, Seattle, Auckland, Buenos Aires + configurable US cities

### Arbitrage Strategy

Scans for price discrepancies across related markets. Currently finds few signals — markets are efficient.

### Copy Trading Strategy

Follows profitable wallets on Polymarket. Requires manual wallet addresses via `COPY_TRADING_WALLETS` env var (Gamma API leaderboard endpoint returns 405).

## Risk Management

- **Kelly sizing** — position size based on edge magnitude and confidence
- **Max position** — per-trade cap (`MAX_POSITION_USD`, default $2)
- **Portfolio limits** — max concurrent positions, max % of balance per trade
- **Daily loss limit** — stops trading if daily losses exceed threshold
- **Cooldown** — pauses after N consecutive losses
- **Kill switch** — halts all trading if balance drops below `KILL_BALANCE_USD`
- **Trade deduplication** — same market_id + outcome can only be traded once

## Data Flow

```
Open-Meteo (GEFS ensemble, 30 members)
    ↓ sequential requests, 500ms delay, exponential backoff retry
Weather Strategy
    ↓ ensemble probability vs market price
Gamma API (Polymarket event/market data)
    ↓ bucket parsing, price extraction
Trading Engine
    ↓ risk checks, Kelly sizing
SQLite (trades.db)
    ├── trades        — 18 columns incl. reasoning, market_id, condition_id
    ├── results       — resolution tracking (trade_id, resolved, won, pnl_usd)
    ├── portfolio_snapshots — balance over time
    └── learnings     — strategy lessons
```

## Configuration

Key `.env` variables:

```bash
# Mode
PAPER_TRADE=true                    # Paper mode (no real money)

# Risk
MIN_EDGE_THRESHOLD=0.15            # 15% minimum edge to trade
MAX_POSITION_PCT=0.06              # Max 6% of balance per trade
MAX_POSITION_USD=2.0               # Hard cap per trade
KILL_BALANCE_USD=5.0               # Stop-loss kill switch
DAILY_LOSS_LIMIT_PCT=0.20          # Max 20% daily drawdown
MAX_CONCURRENT_POSITIONS=10
LOSS_COOLDOWN_TRADES=3             # Pause after 3 consecutive losses
LOSS_COOLDOWN_SECONDS=1800         # 30min cooldown

# Scanning
SCAN_INTERVAL_SECONDS=600          # 10 min between cycles
EXIT_THRESHOLD=0.45                # Sell above this price

# Strategies
ENABLE_WEATHER=true
ENABLE_ARBITRAGE=true
ENABLE_COPY_TRADING=true
ENABLE_TREND_DETECTION=true

# Weather
WEATHER_CITIES=New York,Chicago,Seoul,London,...
```

## Setup

```bash
python -m venv .venv
source .venv/bin/activate
pip install -r requirements.txt
cp .env.example .env  # Edit with your config

# Paper trading (default)
python main.py

# Live trading (requires funded Polymarket wallet)
python main.py --live
```

The agent writes a PID file (`agent.pid`) for process management and cleans up on shutdown.

## Testing

101 tests covering:

```bash
python -m pytest tests/ -v

# Test files:
tests/test_weather_parsing.py    # Bucket parsing, °C/°F conversion, ensemble probability
tests/test_weather_signals.py    # Signal generation, city support, distribution math
tests/test_deduplication.py      # Trade dedup by market_id + outcome
tests/test_resolution.py         # Market resolution and P&L calculation
tests/test_portfolio.py          # Balance tracking, position management
tests/test_portfolio_pnl.py      # P&L math, win/loss scenarios
tests/test_risk.py               # Kelly sizing, loss limits, cooldowns, kill switch
tests/test_api_resilience.py     # Rate limiting, retry logic, error handling
tests/test_config.py             # Configuration validation
tests/test_imports.py            # Module import checks
```

**TDD is enforced.** Write failing tests first, then implement. See `CLAUDE.md` for coding guidelines.

## Monitoring

The agent logs to `agent.log` and stores all trades in `trades.db`. A companion [dashboard](https://github.com/0xj7r/polymarket-dashboard) (Next.js) reads the database for visualization.

## Known Limitations

- **Rate limiting:** Open-Meteo free tier has daily limits. Sequential requests with 500ms delay + exponential backoff.
- **Resolution checker:** Disabled pending reimplementation (needs to only check markets past close date).
- **Copy trading:** Polymarket leaderboard API returns 405; requires manual wallet addresses.
- **No 24/7 uptime:** Runs on local machine; dies when Mac sleeps.

## License

Private — not for redistribution.
