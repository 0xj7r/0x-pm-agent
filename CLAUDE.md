# CLAUDE.md - Polymarket BTC Sniper

## Core Principle: Test-Driven Development (TDD)

Write tests FIRST, code SECOND. Failing test → minimal code → refactor → full suite green.

## Rules

1. **Tests first, then code.** Every commit must pass `python -m pytest tests/ -v`.
2. **Syntax check changed files.**
3. **`from __future__ import annotations` on line 1** or after a single docstring.
4. **Type hints on all function signatures.**
5. **Mock ALL external APIs in tests.**
6. **Do NOT `git push` unless explicitly told to.**
7. **Files under 400 lines.** Split by responsibility if growing.

## Testing

```bash
python -m pytest tests/ -v
python -m pytest tests/test_btc_engine.py -v
```

## Key Patterns

- **Signal engine uses `set_state` (snapshot)** — not accumulate
- **Asymmetric Kelly** for cheap tokens (2-5c) with capped downside
- **Balance accounting**: `cost + pnl` at resolution
- **Health endpoint** on :8080 for remote monitoring
- **Slack notifications** on trades, resolutions, errors
