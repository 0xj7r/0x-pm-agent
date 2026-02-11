# CLAUDE.md - Coding Guidelines for polymarket-agent

## Core Principle: Test-Driven Development (TDD)

**Write tests FIRST, code SECOND.** Every change follows this flow:

1. **Write a failing test** that defines the expected behavior
2. **Write the minimum code** to make it pass
3. **Refactor** if needed, keeping tests green
4. **Run the full suite** — ALL tests must pass before committing

If you're fixing a bug: write a test that reproduces the bug FIRST, then fix it.
If you're adding a feature: write tests for the expected behavior FIRST, then implement.

**No exceptions.** Code without tests is incomplete code.

## Rules (MUST follow)

1. **Tests first, then code.** Write or update tests before implementing changes. Every commit must pass `python -m pytest tests/ -v`.

2. **Syntax check all changed files.** Before committing, run `python -c "import py_compile; py_compile.compile('path/to/file.py', doraise=True)"` on every modified file.

3. **`from __future__ import annotations` must be on line 1 or directly after a SINGLE module docstring.** Never duplicate docstrings. Never put code before future imports.

4. **Do not change trading logic without explicit approval.** Config values, thresholds, and strategy behavior are tuned. Only change if the task specifically asks for it.

5. **Type hints on all function signatures.** No exceptions.

6. **Mock ALL external APIs in tests.** No real HTTP requests. Use `unittest.mock` and temporary SQLite databases.

7. **Do NOT `git push` unless explicitly told to.**

## Test Categories

```
tests/
├── test_imports.py          # Every module imports cleanly
├── test_config.py           # Config defaults verified
├── test_weather_parsing.py  # Bucket parsing, ensemble probability
├── test_weather_signals.py  # Edge calc, direction, city coverage
├── test_risk.py             # Risk filters, Kelly sizing, limits
├── test_portfolio.py        # Trade recording, snapshots
├── test_portfolio_pnl.py    # P&L tracking, win rate, balance
├── test_deduplication.py    # Same market can't be traded twice
├── test_resolution.py       # Only closed markets resolve, correct P&L math
├── test_engine_cycle.py     # Full cycle integration, dedup across cycles
└── test_api_resilience.py   # 429 retry, 500 handling, timeouts
```

## Project Structure

```
polymarket-agent/
├── main.py              # Entry point
├── config.py            # All config from env vars
├── clients/             # External API clients
├── core/                # Engine, portfolio, risk, memory
├── strategies/          # Trading strategies (pluggable)
├── models/              # Data models (dataclasses)
├── backtesting/         # Backtesting engine
└── tests/               # Test suite (pytest, 100+ tests)
```

## Testing Commands

```bash
# Run all tests (MUST do before every commit)
python -m pytest tests/ -v

# Run specific test file
python -m pytest tests/test_deduplication.py -v

# Run with short traceback
python -m pytest tests/ -v --tb=short

# Quick syntax check
python -c "import main"
```

## Common Mistakes to Avoid

- **Duplicate trades**: Always check `memory.get_open_trades()` before executing
- **False resolutions**: Only resolve markets where Gamma API returns `closed=True`
- **Double-counting**: A trade must only be resolved ONCE — check `results` table
- **Duplicate module docstrings** (breaks `from __future__` imports)
- **Negative temperatures**: Toronto can have `-4°C or below` — parser must handle
- **Weather slugs need year**: `{city}-on-{month}-{day}-{year}`
- **°C vs °F**: Non-US cities use °C buckets — convert to °F for NOAA comparison
- **Token IDs**: Must be on Signal objects for weather markets
- **Copy trading API**: Leaderboard endpoint returns 405 — handle gracefully
- **Rate limiting**: Open-Meteo 429s — use sequential requests with 0.5s delay + cache
