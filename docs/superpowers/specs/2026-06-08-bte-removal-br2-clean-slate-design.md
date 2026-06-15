# Design: Nuke BTE, clean slate to br2 (Phase 1)

Date: 2026-06-08
Branch: `br2-clean-slate`
Status: awaiting review

## Context & motivation

The live BTC-5m sleeve was running the **BTE** strategy and is structurally
loss-making. Live diagnosis against the profitable target wallet
`0xb55fa1296e6ec55d0ce53d93b9237389f11764d4` (today: target +$3,790 / +2.5% vs
our `0x00190179...` −$214 / −8.3%, same choppy regime) found BTE is −EV at the
mechanism level:

- **Sub-price win rate.** At our highest-volume entry (~0.69) our chosen side
  wins 64% vs the market-implied 69%. The target wins 74% at the same price.
- **Execution leak.** We lift the offer (+13.5c above the prevailing CLOB price,
  70% taker). The target trades at mid (49% taker, −0.5c).
- **Incoherent books.** Flip-flop churn (alternating Up/Down flat clips, ~10
  price levels/market) builds unbalanced directional positions with no thesis.

BTE was built in May to chase a fresh edge when br2's validated Feb–Apr edge
*seemed* to decay in choppy markets. The chase produced loss-making, sloppy
code (~15 commits this week of BTE + a bespoke router + guard layers).

**br2 (BonereaperV2)** is the chosen baseline: validated strong Feb–Apr,
calibrated (Black–Scholes binary-digital model → probability; edge gate that
only fires when `p_model − price > min_edge`), consumes Binance signed-flow /
adverse-vol features (validated predictive, AUC 0.60–0.64 on the 30d drawdown
window; BTE is price-only and blind to them), and is **equivalence-tested
byte-identical** to backtest champion `062901`
(`configs/bonereaper_v2_favourite_062901.command.txt`).

Live trading has been **killed** (sleeve `polymarket-exec@bte_tinylive.service`
stopped + masked; no resting orders; no fills since 12:28 UTC 2026-06-08).

## Goals

1. Remove BTE and the BTE-era router/guard scaffolding entirely.
2. Make br2 the **sole** live strategy, reproducing its validated, champion-
   equivalent behavior with no behavioral overlay.
3. Re-validate br2 on recent (May–June) data before any redeploy.
4. Leave the architecture parameterizable by (token, duration) so Phase 2
   expansion is tractable — without building expansion now. Multi-market is
   **core to the end-state**, not optional: the target wallet's edge today was
   concentrated in ETH and the 4h markets, and BTC-5m is the most contested
   book. Phase 1 therefore must NOT entrench BTC-5m hardcoding further; where
   the BTE/router removal touches BTC-5m-bound code, leave a clean seam.

## Non-goals (explicitly out of scope for Phase 1)

- Market expansion to ETH/SOL/XRP or 15m/1h/4h durations (Phase 2).
- Any new strategy logic, signal, or "chop module".
- Re-tuning br2's champion parameters.

## Design

### Strategy: br2 as the sole live path

br2's decisions flow straight to the existing execution path
(`execute_execution_adapter` → paper-fill sim or live CLOB submit), gated only
by what existed during br2's validation:

- br2's own champion gates: model gate (min-confidence 0.68 / max-risk 0.72),
  edge gate (`p_model − price > min_edge`), lane structure, and participation
  caps (max-pair-cost 0.99, max-orders-per-leg, max-inventory-delta-shares).
- The pre-existing execution-runtime risk caps in `core/risk.rs` (position /
  notional limits), which predate BTE.

No router. No session-stress dampening, whipsaw-entry-delay, route-confirm, or
damped-add/no-add. Those were added this week for BTE and would make br2 run in
an environment it was never validated in, breaking the equivalence guarantee.

### Removed

- **Files deleted:** `runtime/bte_live.rs`, `runtime/bte_shadow.rs`.
- **BTE excised from shared files:** `strategy_profile.rs` (BackToExploreSection
  + config method + import), `runtime/mod.rs` (module decls, accounting-lane and
  tag arms), `runtime/order_store.rs` (BackToExplore accounting lane, the
  `bte_orders`/`bte_submit_intents` RouterDecisionRecord fields + sqlite schema
  columns + binds, BTE tests), `runtime/runner.rs` (BTE init, the BTE shadow
  decision loop, BTE intent submission, BTE logging).
- **Router stripped from `runner.rs`:** `MarketRoute` enum,
  `static_cluster_router_route`, `live_router_route`, `canonical_router_route`,
  `shadow_vote_route`, `smoothed_market_route_signal`,
  `update_latched_router_route`, `router_session_stressed`,
  `update_router_session_regime`, `classify_router_execution_permission`,
  `RouterIntentBudget`, `router_session_damped_max_gross_usd`,
  `router_market_gross_exposure_usd`, `router_allows_strategy_intent`,
  `retain_router_allowed_intents_with_budget`, `load/persist_router_session_regime`,
  `MarketRegimeCluster` classification, router-decision sqlite persistence,
  and the associated `PM_BTC_5M_ROUTER_*` env vars.
- **Env vars removed:** all `PM_BTC_5M_BTE_*` and `PM_BTC_5M_ROUTER_*`.
- **Tests removed:** BTE + router tests in `tests/unit/runtime_runner.rs`,
  `tests/unit/runtime_core.rs`, `tests/ops_env_parity.rs`, and the in-file tests
  in the deleted modules.

### Kept (must survive untouched)

- **Binance spot feed** `wire/spot_ws.rs`: `SpotTradeEvent.is_buyer_maker:
  Option<bool>` and the aggTrade `m`-flag parse — br2's signed-flow features
  REQUIRE this. br2 ingests it via `on_spot_trade`.
- `runtime/btc_signals.rs` (`BtcSignalStore`), `signals/*`, `core/risk.rs`,
  `runtime/paper_fill.rs`, `wire/execution_adapter.rs`,
  `runtime/market_universe.rs`, `market_context.rs`.
- br2 adapter: `runtime/br2_live.rs`, `runtime/br2_shadow.rs`, and the
  `br2_equivalence` / `br2_shadow_smoke` bins.

### br2 live re-enablement

Today br2 is shadow-only because `canonical_router_route` maps `"br2" => Bte`
and the router-enforce step then filters every br2 intent (selected route is
never `Br2`). With the router removed, br2's `submit_intents` reach
`execute_execution_adapter` directly when the live arm is armed
(`PM_BTC_5M_BR2_LIVE_TRADE=true`, `!paper_mode`, kill-switch path configured,
notional caps > 0 — per `Br2LiveShadow::from_env`).

Open item: confirm the root cause of the currently-`failed`
`polymarket-exec@br2_live.service` from its journal (suppression is understood;
the systemd failure state is not yet confirmed) before relying on that unit.

### Re-validation before redeploy (gate to live)

No capital is redeployed until:

1. `cargo build` + `cargo test` green; `br2_equivalence` still passes
   byte-identical (proves removal didn't perturb br2's decisions).
2. br2 walk-forward backtest on **recent May–June** markets (via
   polymarket-backtest `pm-app walk-forward --strategies bonereaper_v2` with the
   062901 champion command) to measure whether the Feb–Apr edge actually
   decayed or held. Decision rule: redeploy only if recent-window EV is
   non-negative after fees.
3. A live **shadow / paper** window confirming br2 decisions + paper P&L look
   sane end-to-end on the cleaned runtime.

### Phase-2 seams (documented, NOT built)

Locations to generalize later for (token, duration): `market_universe.rs`
asset/slug-prefix + window binding; `PM_BTC_5M_*` env naming → `PM_${ASSET}_${DUR}_*`;
Binance `BTCUSDT` symbol; the `062901` champion → per-asset champion registry;
`btc_signals.rs` hardcoded 5m/15m/45m windows; binary YES/NO assumptions.

## Risks

- **runner.rs entanglement:** BTE/router code is interleaved with the br2 and
  core-strategy paths; removal must be incremental and compile-checked per step.
- **order_store schema change:** dropping `bte_*` columns needs a schema-version
  bump / migration so existing sqlite stores load.
- **br2 "edge decay" may be real:** if the recent-data backtest shows br2 is now
  −EV, Phase 1 still delivers a clean, trustworthy codebase, but redeploy waits
  on Phase 2 (expansion) or a chop fix rather than going live on a decayed edge.

## Verification plan

- Per-step `cargo build` (workspace) after each removal chunk (≤5 files/step).
- `cargo test` for the crate; specifically `br2_equivalence` byte-identity.
- `git diff` review confirming no surviving `bte`/`BackToExplore`/`MarketRoute`
  references (grep gate).
- Backtest + shadow/paper as above before any live redeploy.
