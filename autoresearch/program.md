# Threshold Autoresearch Program

Objective: improve the threshold-based latency arbitrage configuration without auto-deploying changes.

Rules:
- Read `backtesting/strategy_results.json` first.
- Search for better `move_threshold`, `max_entry`, and feature-filter combinations per coin.
- Use `backtesting/research.py`, `backtesting/simulator.py`, and `backtesting/projection.py`.
- Save only reviewable candidates; do not overwrite production config automatically.
- Prefer changes that improve held-out test PnL per trade, not just train performance.
- Produce a compact summary for Slack and human review.

Outputs:
- Candidate JSON files in `autoresearch/candidates/`
- Console summary of current best vs candidate best
- Optional Slack notification when a better candidate is found
