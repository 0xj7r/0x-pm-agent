# Preregistration: {{ utc_timestamp }}-{{ coin }}-{{ purpose }}

> This file is a sealed plan. Once committed, it MUST NOT be edited.
> If the plan changes, write a new file in a new commit and reference
> this one as the parent.

## 1. Question

State in one sentence what hypothesis this run is testing.

> Example: "Does a logistic regression on existing signal-fired
> booleans plus 5-minute realized volatility produce a forecaster
> that beats the market-implied baseline on BTC 5-min markets after
> fees?"

## 2. Coin

`btc` or `eth` (one per preregistration; cross-coin runs require
two preregistrations).

## 3. Dataset fingerprint

- `dataset_fingerprint`: SHA256 of the manifest at the time of
  preregistration. Compute via:
  `python3 -c "from autoresearch.partitions import ...; print(...)"`
- `partition_function_commit`: git rev-parse HEAD on
  `autoresearch/partitions.py` at the time of preregistration.
- `n_markets`: total number of markets in the manifest at this
  fingerprint.
- `manifest_time_range`: earliest to latest market start_time.

## 4. Forecaster family

Name of the family from `autoresearch/forecasters/`. Currently
supported in v1:

- `baseline_market_implied`
- `wave1_logistic_stack` (logistic over existing signals as
  features plus engineered features)

State the exact import path.

## 5. Parameter grid

A finite list of concrete parameter combinations. The grid must
contain at most 100 cells per family. Specify as YAML inside a code
block:

```yaml
forecaster_family: wave1_logistic_stack
grid:
  - C: 0.1
    feature_set: signals_only
  - C: 1.0
    feature_set: signals_only
  - C: 1.0
    feature_set: signals_plus_microstructure
```

## 6. Calibration method

One of: `none`, `isotonic`, `platt`. Document the choice and why.

## 7. Sizing policy

Reference to a SizingPolicy literal. Defaults are usually correct
for v1; document any deviation.

```python
SizingPolicy(
    kelly_fraction=0.25,
    shrink_alpha=0.5,
    max_bet_fraction=0.05,
)
```

## 8. Edge policy

Reference to an EdgePolicy literal.

```python
EdgePolicy(
    spread_multiplier=0.5,
    sigma_multiplier=2.0,
)
```

## 9. Validation methodology

- `cpcv_partitions`: number of CPCV groups (default 10)
- `cpcv_k_test`: groups per test fold (default 2)
- `cpcv_embargo`: number of markets to embargo on each side of a
  test fold (default 12 for ~1 hour at 5min cadence)
- `bootstrap_block_length`: declared up front, derived from the
  ACF of TRAIN per-trade returns under the baseline forecaster
- `bootstrap_n_resamples`: default 1000
- `bootstrap_confidence_level`: default 0.95

## 10. Multiple-testing budget

- `max_pbo`: 0.40 (default). If PBO across the grid exceeds this,
  the run is logged as PBO-failed and no candidate is promoted.

## 11. Decision rule for promotion to holdout

Concrete inequality. Example:

> "The candidate with the highest CV log_growth_per_trade is
> promoted to holdout testing iff:
> 
> - bootstrap lower CI on log_growth_per_trade > 0
> - bootstrap lower CI exceeds market_implied_baseline's CV
>   log_growth_per_trade by at least 0.0005 (5 basis points per
>   trade)
> - PBO across the grid < 0.40
> - aggregate_oos.profitable_folds >= num_folds - 1
> - calibration audit returns no slice with |z| > 2.5"

## 12. Holdout pass criteria

Concrete numbers. Example:

> "The candidate passes holdout iff:
> 
> - holdout log_growth_per_trade > 0
> - holdout calibration component < 0.02 (Murphy)
> - holdout slice-conditional reliability shows no slice with
>   |z| > 2.5"

## 13. Pre-registration receipt

Leave blank when drafting; fill in with the commit hash AFTER you
commit this file. The Researcher agent verifies the commit hash
matches `git rev-parse HEAD` of this file before running.

`commit_hash`:

## 14. Notes

Free text. Anything the Red Team should know when reviewing the
candidate. Anything that wouldn't fit a template field. The why
behind the question, the prior result that motivated it, the
specific failure mode this run is meant to rule in or out.
