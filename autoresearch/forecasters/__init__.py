"""Forecaster implementations for the autoresearch framework.

A forecaster is a callable that takes a MarketSlice and a snapshot
index and returns q ∈ [0, 1], the model's estimate of P(YES wins).
The framework uses this q to compute edge against the live ask,
size positions via fractional Kelly on a shrunk q, and run honest
log-growth simulations against held-out data.

Each forecaster implementation here:
- Conforms to the ForecasterFn signature in autoresearch/methods/simulator.py
- Documents what features it requires from the MarketSlice
- Provides a fit(...) method or constructor that takes only TRAIN data
- Provides a predict(...) method or __call__ that produces q given
  a single (market, index) input
- Is independently testable with synthetic data

The included forecasters as of phase B:

baseline.py            Market-implied "the market is right" forecaster.
                       q = best_ask_yes / (best_ask_yes + best_ask_no).
                       The mandatory baseline every research run must beat.

logistic.py            Pure-numpy L2-regularised logistic regression
                       fitter. No sklearn dependency. Used as the
                       building block for direction-conditional models.

regime_gated.py        Dual-direction forecaster: separate logistics
                       trained on UP-move and DOWN-move slices, each
                       with its own isotonic recalibration. Designed
                       to capture the direction-asymmetric regime
                       pattern observed in the Apr 4-7 ETH paper trades.
"""
