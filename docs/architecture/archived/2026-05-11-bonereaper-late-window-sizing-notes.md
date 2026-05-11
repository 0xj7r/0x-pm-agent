# Bonereaper Late-Favorite Sizing Notes (Q23)

## Conclusion

Bonereaper’s late-favorite loading is **not** per-bet Kelly sizing.
Observed late-window load size rises as favorite price rises, which is opposite to what a
simple Kelly fraction would do.

Observed late-window averages (Q23):

- 0–5s: avg px 0.978, 249 shares
- 5–15s: avg px 0.980, 133 shares
- 15–30s: avg px 0.975, 114 shares
- 30–60s: avg px 0.969, 99 shares
- 60–120s: avg px 0.966, 87 shares
- 120–300s: avg px 0.947, 83 shares

At high late prices (e.g. 0.98), stand-alone EV on those fresh shares is negative
with ~95.5% directional accuracy.

The strategy works because the late-leg loads are absorbed into a much lower
average fav-side cost basis carried from earlier mid-band buys (median fav-side
avg ≈ 0.79), then loaded in late-window bursts.

## Practical interpretation for engine design

- Keep late-favorite sizing as a **deterministic shape + hard cap** rule.
  - Price-distance / time-to-end ramping is the primary allocator.
  - `max_load_usd` and `clip_usd` are the hard risk controls in that bucket.
- Treat Kelly-style sizing as a **separate residual-cap concept** for unmatched
  directional exposure, not as per-fill share logic.
- Avoid implementing per-leg, per-fill Kelly in the late-favorite module.

This aligns with the whale-pattern evidence and keeps behavior explainable:
large late-window notional is a strategy-level directional residual bet, not a
fresh standalone EV-optimized bet at the ask.
