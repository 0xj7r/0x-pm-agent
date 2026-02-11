# CLAUDE.md - Coding Guidelines for polymarket-agent

## Rules (MUST follow)

1. **Run tests before committing.** Every commit must pass `python -m pytest tests/ -v`. If tests fail, fix them before committing.

2. **Syntax check all changed files.** Before committing, run `python -c "import py_compile; py_compile.compile('path/to/file.py', doraise=True)"` on every modified file.

3. **`from __future__ import annotations` must be on line 1 or directly after a SINGLE module docstring.** Never duplicate docstrings. Never put code before future imports.

4. **Do not change trading logic without explicit approval.** Config values, thresholds, and strategy behavior are tuned. Only change if the task specifically asks for it.

5. **Type hints on all function signatures.** No exceptions.

6. **Test any new functionality.** Add tests in `tests/` for new modules/functions. At minimum: import tests, unit tests for pure functions, integration tests for API-calling code (mocked).

7. **Do NOT `git push` unless explicitly told to.**

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
└── tests/               # Test suite (pytest)
```

## Testing

```bash
# Run all tests
python -m pytest tests/ -v

# Run specific test
python -m pytest tests/test_weather.py -v

# Quick syntax check
python -c "import main"
```

## Common Mistakes to Avoid

- Duplicate module docstrings (breaks `from __future__` imports)
- Forgetting to handle negative temperatures in parsing
- Weather market slugs need the year: `{city}-on-{month}-{day}-{year}`
- Weather markets use °C for non-US cities — must convert to °F for NOAA
- Token IDs must be passed on Signal objects for weather markets (not in standard market cache)
- Copy trading leaderboard API returns 405 — handle gracefully
