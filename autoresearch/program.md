# Polymarket Autoresearch Program

This repo uses an agent to improve trading strategies, but the evaluation loop is deterministic Python.

## Objective

Improve strategy quality for recurring Polymarket market families without overfitting and without auto-deploying.

The agent is responsible for:
- choosing search directions
- refining deterministic Python search code
- inspecting experiment outputs
- proposing candidates for review

The deterministic Python layer is responsible for:
- enumerating candidate strategies
- evaluating candidates on stored datasets
- running walk-forward, holdout, robustness, and stress validation
- logging experiment records

## Ground Rules

- Treat `backtesting/validate_strategy_readiness.py` as the canonical evaluator.
- Treat `autoresearch/runner.py` as the canonical orchestration entrypoint.
- Treat `strategy_config.json` as the only canonical strategy config.
- Never auto-promote a candidate into production profiles.
- Candidate promotion must go through `autoresearch/promote_candidate.py` or an equivalent human-reviewed action.

## Safe Edit Surface

Default editable files:
- `autoresearch/*.py`
- `strategies/registry.py`
- `tests/test_autoresearch_rigor.py`

Only edit evaluator files when necessary and preserve the evaluation contract:
- `backtesting/validate_strategy_readiness.py`
- `backtesting/evaluator.py`

Do not casually edit runtime trading files during autoresearch work.

## Standard Research Loop

1. Inspect the current promoted strategy and recent candidate artifacts.
2. Decide on a bounded search direction.
3. If needed, refine deterministic Python search code.
4. Run:
   `python3 autoresearch/runner.py --coin <coin>`
5. Inspect:
   - `autoresearch/experiments.jsonl`
   - `autoresearch/candidates/*.json`
6. Keep changes that improve the research engine or produce better validated candidates.
7. Do not promote automatically.

## Acceptance Standard

A candidate is only interesting if it:
- beats the current baseline on the canonical score
- survives walk-forward validation
- survives final holdout
- is not rejected by robustness / stress checks
- is simple enough to justify keeping

## Philosophy

- Simpler is better.
- Deterministic evaluation beats clever stories.
- Search breadth is useful, but only with strong validation.
- Cross-market generalization is more valuable than winning one tiny slice.
