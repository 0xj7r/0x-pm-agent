"""Statistical methods used by the autoresearch framework.

Each module in this package is a pure function library with unit tests.
Modules do not import from agents, the orchestrator, or anything that
talks to a database. They take numpy arrays in and return numpy arrays
or scalar dicts out. This separation lets the methods be tested with
synthetic data and reused outside the framework if needed.

Modules:
- bootstrap: stationary block bootstrap, ACF-derived block length
- calibration: Brier, Murphy decomposition, reliability diagrams, isotonic and Platt
- edge: edge after costs, market-implied baseline, MIN_EDGE policy
- sizing: shrinkage, fractional Kelly, MAX_BET_FRACTION cap
- cpcv: combinatorial purged cross-validation (deferred until phase 2 second wave)
- pbo: probability of backtest overfitting (deferred until phase 2 second wave)
- simulator: forward simulator with best_ask entry, fees, slippage (deferred)
"""
