# Bonereaper tinylive live notes

Date: 2026-05-12
Service: `polymarket-exec@bonereaper_tinylive.service`
Host binary family: `polymarket-exec-bonereaper-lane-fix-*`

## Current intent

Run a tiny-live Bonereaper-style BTC 5m strategy:

- `paired-core`: small, bounded paired market-making baseline only while both legs are plausibly pairable.
- `late-fav`: reactive favorite loading when BTC/price-to-beat/fair-value signal confirms the favorite.
- `cheap-tail`: small convex opposite-side hedge, only sized from existing/expected favorite exposure.
- `merge`: recycle confirmed paired-core inventory, but do not consume protected late-fav directional residual.
- `pUSD`: live order collateral should use pUSD; any USDC.e returned by merge/redeem should be wrapped back.

## Observed failure modes

### 1. Full paired-core ladder caused one-sided cheap inventory

Observed on markets including `btc-updown-5m-1778598000`, `btc-updown-5m-1778598600`, and `btc-updown-5m-1778599200`.

Pattern:

- Paired-core submitted a broad ladder on both legs near/after open.
- Cheap side filled across many levels, e.g. UP `20c-48c`, 5 shares each.
- Opposite leg did not fill.
- Market then became clearly favorite DOWN.
- The bot held one-sided cheap UP inventory that consumed risk budget and blocked late-favorite DOWN.

Root cause:

- Paired-core was allowed to emit too many levels at tinylive size.
- Pairability gating was based on broad price band, not actual lane transition/risk budget.
- Cancellation after the market moved was too late; the cheap fills had already happened.

Mitigations applied:

- Reduced paired-core in live profile to 5 levels.
- Reduced paired-core clip base to 1 share.
- Reduced ladder span to 0.20.
- Added ask-side pairability guard: if either leg ask is above paired-core max band, paired-core pauses.
- Raised per-market net cap to leave room for late-fav, but kept per-order cap small.

Remaining required fix:

- Add lane-aware budget in code: paired-core must reserve only a small portion of market net exposure, leaving explicit late-fav headroom.
- Paired-core should stage in smaller batches rather than submit all levels at once.
- Pair-core repair must distinguish true paired-core imbalance from late-fav directional residual.

### 2. Late-favorite fired but was blocked by risk

Observed on `btc-updown-5m-1778599200`.

Pattern:

- Favorite DOWN signal passed.
- Late-fav attempted DOWN bids around `80c/79c/78c`.
- Risk rejected them with `MarketNetExposureTooLarge` because earlier paired-core UP exposure consumed the cap.

Mitigations applied:

- Increased `max_net_notional_per_market_usd` from 25 to 75 in the live profile.
- Kept late-fav `max_load_usd` at 25 and per-order max at 5.

Remaining required fix:

- Risk should have lane budgets, not just market-wide net exposure.
- Paired-core budget should be small and separate from late-fav budget.

### 3. Late-favorite was too shallow

Observed behavior:

- Late-fav fired, but did not ladder high enough across multiple price levels.

Changes applied:

- Favorite load levels now scale by favorite ask:
  - `70-75c`: 1 level
  - `75-80c`: 2 levels
  - `80-90c`: 3 levels
  - `90c+`: 4 levels
  - `90c+` in final 120s: 5 levels
- Still passive-only: levels stack below ask.
- Still capped by tinylive `max_load_usd`.

Remaining required fix:

- Verify live order-store shows multiple late-fav levels after next eligible market.
- Ensure pending/working late-fav orders reserve cap immediately.

### 4. Late-fav cap accounting lagged

Observed logs showed repeated `cumulative=0.00/25.00` even after late-fav orders had been submitted.

Change applied:

- Added short-lived in-strategy reservation ledger for submitted directional notional.
- Included open late-fav/cheap-tail exposure in directional exposure accounting.

Remaining required fix:

- Confirm live logs no longer repeatedly show `cumulative=0.00/25.00` for repeated late-fav attempts in the same market.

### 5. Merge consumed or attempted to consume wrong inventory

Observed:

- Merge intents appeared after directional/paired interactions.
- Some merge intents had negative expected net.
- Directional residual was logged but not always protected from merge planning.

Changes applied:

- Runtime now tracks `late-fav` and `cheap-tail` as directional lanes.
- Merge planner clips/skips merge quantity that would consume protected directional residual.

Remaining required fix:

- Add explicit negative-EV merge stop: do not merge when `expected_net_gain_usd < 0` unless there is a deliberate emergency reason.
- Confirm merge commands include `condition_id` and do not recycle pUSD-critical collateral incorrectly.

### 6. Cheap-tail sizing can erode late-favorite upside

Observed:

- Early cheap-tail was either absent or, once enabled, risked being reasoned
  about as a raw percentage of favorite notional.
- That is not sufficient when favorite is bought at `90c-99c`: the win-upside
  is only `1 - favorite_price`, so a notional-percentage hedge can consume too
  much of the late-fav edge.

Causes:

- Cheap-tail was not tracked as its own filled lane.
- Working convex exposure previously mixed late-fav and cheap-tail orders.
- Tail cap was based on favorite exposure fraction, not favorite win-upside.

Code/profile status:

- Runtime now tracks `cheap-tail` filled inventory separately from `late-fav`.
- Cheap-tail working exposure now counts cheap-tail only.
- Tail cap is now the minimum of:
  - `convex_tail.max_load_usd`
  - `filled_late_fav_notional * max_favorite_exposure_fraction`
  - `late_fav_win_upside * max_win_edge_spend_fraction`
- The same BTC regime/reversal multiplier used for late-fav sizing scales tail cap in choppy/reversal conditions.

Caution:

- Cheap-tail is not the root fix for paired-core runaway.
- It should only fire after favorite exposure exists; it must not become another broad cheap-side ladder.

### 7. pUSD / USDC.e confusion

Observed/user concern:

- Orders appeared to be sending USDC.e to Polymarket rather than pUSD.
- Merge/redeem can return USDC.e, while CLOB orders should use pUSD.

Changes applied:

- pUSD auto-wrap env enabled on host.
- Added pUSD auto-wrap hook after merge accepted.
- Added pUSD auto-wrap hook after user websocket merge/redeem events.

Remaining required fix:

- Inspect live adapter collateral token path and confirm order collateral address is pUSD, not USDC.e.
- Auto-wrap only converts balances; it does not guarantee order construction uses pUSD if adapter config points at USDC.e.

## Current live profile posture

At latest update:

- Paired-core levels: 5
- Paired-core span: 0.20
- Paired-core base clip: 1 share
- Paired-core band: 0.20 to 0.70 with ask-side pause guard in code
- Max order notional: 5 USD
- Max net notional per market: 75 USD
- Late-fav max load: 35 USD, dynamically scaled down in whipsaw/flat/trending-volatile/reversal regimes
- Late-fav clip: 7 USD, further scaled by momentum confidence and regime
- Cheap-tail max load: 5 USD, additionally capped by favorite notional and favorite win-upside

## Operational lessons

- Never copy local macOS release binary to the Linux host; build on host or use a Linux artifact.
- Always deploy from a versioned host binary and symlink `~/.local/bin/polymarket-exec`.
- Profile-only changes require service restart to reload.
- Code changes require host build and service restart.
- Live log interpretation must classify by `quote_level_tag`:
  - `paired-core:*` = paired-core/repair
  - `late-fav-*` = directional favorite load
  - `cheap-tail` = convex hedge lane
- UI activity alone is insufficient; always verify with order store and journal.

## Next fixes to prioritize

1. Confirm CLOB order collateral is pUSD.
2. Add lane-aware risk budgets: paired-core, late-fav, cheap-tail.
3. Add negative-EV merge stop.
4. Stage paired-core instead of full-ladder submission at open.
5. Confirm late-fav multi-level laddering on an eligible live market.
6. Verify cheap-tail cap logs show favorite exposure, win-upside, and regime multiplier.
