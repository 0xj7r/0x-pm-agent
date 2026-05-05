# Polymarket `paired_mm` / `btc_5m_mm` implementation guide

Last updated: 2026-05-05

This doc describes how the active BTC 5m strategy should behave in
`polymarket-exec`, what to check when it misbehaves, and which historical bug
patterns have already cost us money.

The current production intent is **not** pure pair-cost arbitrage and not pure
directional momentum. It is:

```text
paired market making early/mid window
  + maker-rebate capture
  + controlled stranded-inventory handling
  + late-window convex favorite/tail accumulation
  + batched merges when cash pressure allows
```

Whale reference points:

- Unlawful-shear: paired MM + convex/cheap-leg accumulation on BTC 5m.
- Bonereaper: two-sided accumulation in most BTC 5m windows, maker-heavy by
  notional, no visible SELL unwind, late favorite loading, and cheap-tail share
  accumulation.

When changing strategy behavior, check those reference datasets first. Do not
tune in a vacuum.

---

## 1. Strategy in one paragraph

We post resting limit **buy** orders on both outcome tokens in BTC 5m binary
markets. Early and mid-window, the base engine tries to accumulate pairable
inventory at a combined cost below $1 while earning maker rebates. If both legs
fill, we can merge the paired quantity back into collateral. If only one leg
fills, we either keep trying to pair it, hold it to resolution, or use late
window convex logic to reshape the payoff.

Late in the bar, the strategy may deliberately buy more of the favored side
and a smaller amount of the cheap opposite tail. The goal is not a strict
package-arb every tick. The goal is asymmetric payoff shaping: favorite
notional captures high-probability convergence; cheap-tail shares preserve
convex upside if the bar reverses. Existing stranded inventory changes the
sizing: losing-side stranded inventory should bias more favorite loading;
winning-side stranded inventory should reduce favorite loading and use tail
only where it improves convex payoff.

We do **not** use a SELL unwind in this strategy. If inventory is stranded, the
default tools are continued pairing, late convex reshaping, merge batching, and
resolution/redeem.

---

## 2. Active paths

### Path 1: paired bid ladder

Primary path. This is the maker/rebate workhorse.

- Tags: `mm-paired-bid:lN`.
- TIF: usually GTD, `post_only=true`.
- Intent kind: `Entry`.
- Emits: up to `entry_ladder_levels * 2` child intents.
- Expected behavior: quote both Up and Down around fair/book levels, with
  combined pair cost controlled by YAML risk and quote settings.
- Important invariant: if multiple ladder children collapse to the same
  `market + instrument + side + price`, aggregate them into one order and
  preserve metadata such as `quote_level_tag=l1+l2+l3`.

This path should operate early and mid-window unless hard risk/runtime gates
are active.

### Path 2: cheap-leg / convex accumulation

Small asymmetric side-bet path.

- Tags: typically convex/cheap-leg tags.
- TIF: GTD or maker-style resting order unless explicitly configured otherwise.
- Intent kind: `Entry`.
- Purpose: buy cheap opposite-side exposure where the payoff multiple is high
  and the loss budget is controlled.
- Sizing: should be driven by marginal payoff, remaining loss budget, current
  inventory, BTC momentum/regime, and order-book pressure, not hardcoded fixed
  clips where avoidable.

This is not a separate strategy. It is the convex arm of paired MM.

### Path 3: late convex favorite/tail package

Late-window asymmetric accumulation inspired by Bonereaper behavior.

- Tags: late convex / favorite / tail package tags.
- TIF: generally short-lived GTD; maker-first where possible.
- Intent kind: `Entry`.
- Fires late in the window when terminal timing, liquidity, BTC momentum, and
  book-pressure sanity checks pass.
- Favorite side: larger notional allocation when the model/regime supports the
  favorite and when losing-side stranded inventory needs offsetting.
- Cheap tail: smaller dollar notional, but meaningful share count where payoff
  multiple and max-loss constraints are acceptable.

The package does **not** require strict positive EV on every combined package.
That would suppress too much of the behavior we are trying to capture. It
should require:

- terminal timing sanity,
- liquidity sanity,
- price/payoff sanity,
- max-loss sanity,
- inventory-aware sizing,
- no SELL unwind.

### Path 4: merge batching / redeem

Exit/capital recycling path, not a trading alpha path.

- Merge paired inventory when capital pressure is high or pairable notional is
  above the configured batch threshold.
- Otherwise allow small pairs to batch, because constant tiny merges create
  noise and unnecessary operational churn.
- At resolution, winning inventory redeems to $1 and losing inventory expires
  to $0.

Policy:

```text
if free_cash low or gross inventory high:
  merge immediately
else if pairable_notional < merge_batch_threshold:
  wait / batch
else:
  merge
```

---

## 3. Per-tick decision model

```text
on_market_snapshot:
  refresh book, BTC, fair value, inventory, open-order state

  if runtime degraded or hard risk gate active:
    suppress all entry

  if paired-MM allowed:
    build paired bid ladder
    aggregate collapsed ladder levels

  if late-window convex overlay allowed:
    compute favorite/tail package
    size using inventory + momentum + book pressure + loss budget
    optionally suppress normal ladder when package fires

  if pairable inventory exists:
    apply merge batching policy

  never emit SELL unwind for paired_mm
```

Hard gates should be rare and obvious: runtime degraded, asymmetric fill
cooldown if explicitly enabled, insufficient cash, max gross inventory, or
venue/auth failure. Soft conditions should resize or switch paths rather than
silence the strategy.

---

## 4. Required invariants

### Strategy config

- Strategy behavior belongs in `polymarket-exec/config/strategies/*.yaml`.
- Rust defaults are safety fallbacks only. Live behavior should be explicit in
  the YAML profile.
- Do not bury strategy knobs in launch scripts.
- Tinylive env must not silently override YAML risk caps unless the override is
  deliberate and documented.

### Paired ladder

- Each ladder level must carry a stable `quote_level_tag`.
- Distinct levels at distinct prices remain distinct orders.
- Collapsed levels at the same price must aggregate into one order, not submit
  duplicates.
- Prices must be tick-aligned at the strategy or wire boundary.
- `post_only=true` must be preserved for maker-entry paths.

### Runtime state

- Live source of truth is runtime open-order state plus `OrderStore`.
- Replay-only stores must be named/scoped as replay-only.
- Reconciliation should be able to answer:
  - what we intended,
  - what was submitted,
  - what is working,
  - what filled,
  - what was cancelled/rejected,
  - what inventory exists,
  - what is pairable,
  - what has merged/redeemed.

### Risk

- Entry caps apply to new risk.
- Close/recycle actions must not be blocked by entry-only caps.
- Late convex sizing must respect:
  - max gross notional,
  - max leg cost,
  - max loss per package/window,
  - max order notional,
  - max position quantity,
  - open-order count caps.

If late convex needs more room than tinylive caps allow, change the YAML risk
profile explicitly rather than bypassing the risk engine.

### No SELL path

For active paired MM, SELL should not be emitted as an inventory-management
habit. We previously observed buy/sell loops that likely amplified losses.
Allowed exits are:

- merge paired inventory,
- redeem winning inventory,
- let losing inventory expire,
- optionally rescue/pair through buying the opposite leg if configured and EV
  justified.

---

## 5. Configuration sources of truth

| Concept | Source of truth |
|---|---|
| Paired ladder levels/spacings | Strategy YAML |
| Convex overlay enablement | Strategy YAML |
| Favorite/tail package sizing limits | Strategy YAML |
| Inventory caps | Strategy YAML |
| Merge batching threshold | Strategy YAML |
| Live venue/auth/env secrets | Host env/secrets |
| Runtime logging paths | Host env |
| Deployment unit name | `polymarket-exec@btc_5m_mm_tinylive.service` |

Current tracked tinylive template:

```text
polymarket-exec/ops/env/btc_5m_mm_tinylive.env.example
```

Deprecated naming such as `btc_5m_tinylive` or `hybrid-tinylive` should not be
used for the current paired-MM deployment unless intentionally running a
different profile.

---

## 6. Verification checklist before tinylive

Do not claim readiness until these pass or are explicitly waived.

1. Service target is correct.

```bash
systemctl --user status polymarket-exec@btc_5m_mm_tinylive
```

2. No accidental SELL path.

```bash
rg -n "TradeSide::Sell|Side::Sell|sell" polymarket-exec/src
```

Then inspect any matches and confirm they are not active paired-MM unwind
submissions.

3. YAML/env alignment.

```bash
rg -n "PM_BTC_5M|btc_5m_mm|btc_5m_paired_mm" polymarket-exec/src scripts polymarket-exec/ops/env polymarket-exec/config
```

Check launcher env names match `config/mod.rs`, and that tinylive does not
override YAML risk caps accidentally.

4. Ladder reaches venue.

```bash
journalctl --user -u polymarket-exec@btc_5m_mm_tinylive.service --since '15 minutes ago' --no-pager \
  | grep -oE 'mm-paired-bid:l[0-9]+' | sort | uniq -c
```

5. Collapsed ladder aggregation is visible.

Look for one submitted order where several intended levels collapsed to one
instrument/side/price and metadata preserves the combined level tags.

6. Maker ratio is sane.

Paired ladder fills should mostly be maker fills. Taker-heavy paired entries
mean `post_only` or quote placement is wrong.

7. Stranded inventory is visible and explainable.

The journal/dashboard should show:

- stranded side,
- stranded quantity,
- cost basis,
- current fair/book value,
- pairable quantity,
- mergeable notional,
- late convex adjustment reason.

8. Merge batching works.

Tiny pairable inventory should not necessarily merge immediately. Pairable
inventory should merge when batch threshold/cash pressure/gross inventory
policy says so.

9. Replay sanity.

Run at least one real Telonex-backed replay window before tinylive. Check:

- fills,
- stranded inventory,
- merge batching,
- late convex orders,
- settlement/redeem accounting,
- no SELL path.

---

## 7. Known bug patterns

### Bug 1: benign post-only rejection trips live kill switch

Symptom: one `invalid post-only order: order crosses book` failure causes live
execution budget exhaustion.

Fix invariant: benign post-only/crossing rejections must not count as fatal
live errors in either Ok-ack or Err paths.

### Bug 2: config drift between YAML, env, and launcher

Symptom: operator changes YAML but live behavior does not change, or env uses a
name that `config/mod.rs` never parses.

Fix invariant: strategy knobs live in YAML; launcher env only handles secrets,
paths, venue mode, and explicit operational overrides.

### Bug 3: ladder clamp or duplicate filter collapses orders

Symptom: strategy builds multiple ladder levels, but venue only sees one level
per instrument.

Fix invariant:

- quote engine passes pre-laddered intents through,
- duplicate filter matches by `market + instrument + side + level_tag`,
- collapsed same-price children are aggregated intentionally, not dropped.

### Bug 4: off-tick prices

Symptom: l2/l3+ orders silently reject or disappear.

Fix invariant: all submitted prices are one-cent tick aligned after any skew or
aggregation.

### Bug 5: stale open-order source of truth

Symptom: runtime thinks orders are working when venue has cancelled/expired
them, or submits duplicates because local state missed a fill/cancel.

Fix invariant: reconciliation should write and read the same `OrderStore`
state, and unexpected venue state should be logged as reconciliation drift.

### Bug 6: immediate tiny merge churn

Symptom: many tiny merge attempts for trivial pairable quantities.

Fix invariant: merge batching threshold applies unless free cash or gross
inventory pressure requires immediate recycling.

### Bug 7: buy/sell loop

Symptom: activity alternates BUY then SELL in the same market, often locking in
losses while still leaving inventory risk.

Fix invariant: active paired MM has no SELL unwind path. If SELL appears,
identify the exact code path before restart.

### Bug 8: late convex starved by risk caps

Symptom: late favorite/tail logic appears to fire in logs but submits nothing
because `max_order_notional`, `max_leg_cost`, or `max_position_quantity` are
too low.

Fix invariant: either YAML caps are intentionally tiny for tinylive, or the
profile is raised explicitly. Do not bypass risk in code.

---

## 8. Useful one-liners

```bash
# Live logs
journalctl --user -u polymarket-exec@btc_5m_mm_tinylive.service -f

# Ladder tags seen in logs
journalctl --user -u polymarket-exec@btc_5m_mm_tinylive.service --since '15 minutes ago' --no-pager \
  | grep -oE 'mm-paired-bid:l[0-9]+' | sort | uniq -c

# Late convex / package decisions
journalctl --user -u polymarket-exec@btc_5m_mm_tinylive.service --since '15 minutes ago' --no-pager \
  | grep -E 'convex|favorite|tail|package|stranded'

# Merge planning
journalctl --user -u polymarket-exec@btc_5m_mm_tinylive.service --since '30 minutes ago' --no-pager \
  | grep -E 'merge intent|pairable|merge batch|redeem'

# Post-only / live budget failures
journalctl --user -u polymarket-exec@btc_5m_mm_tinylive.service --since '30 minutes ago' --no-pager \
  | grep -E 'post-only|crosses book|error budget exhausted'

# Rebate balance
curl -s "https://data-api.polymarket.com/rebates/current?user=$WALLET" | jq
```

---

## 9. What winning should look like

Normal window:

- early/mid: both sides quoted passively,
- fills arrive mostly as maker,
- pairable inventory accumulates without excessive duplicate orders,
- stranded inventory is visible and bounded,
- late window: favorite/tail convex overlay may reshape payoff,
- paired quantities merge when batch/cash policy says so,
- no SELL unwind,
- resolution accounting correctly redeems winner and expires loser.

If the bot has no fills for active windows, start with quote suppression and
venue submission. If it has many fills but loses money, start with stranded
inventory, late convex sizing, merge batching, and settlement accounting.
