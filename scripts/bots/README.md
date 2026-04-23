# scripts/bots

Runnable strategy bots and execution-style utilities:

- BTC whale-pair and carry variants (`whale_pair_live_bot.py`, `whale_pair_paper_bot.py`, `paired_carry_paper_bot.py`)
- Weather/BTC bots (`weather_paper_bot.py`, `weather_live_scout.py`, `penny_paper_bot.py`)
- Kelly family helpers (`kelly_walkforward.py`, `kelly_counterfactual.py`)

Legacy entrypoints remain at repository root and route through
`scripts/_legacy_script_dispatch.sh`.

The dispatch layer now resolves canonical scripts automatically by scanning
the grouped script subdirectories, so adding a new bot file under `scripts/bots`
typically needs no extra top-level wrapper.
