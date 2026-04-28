# Shadowlive Fidelity + Bonereaper Validation — Design

Date: 2026-04-28
Branch: `shadowlive-fidelity-bonereaper`
Status: Design proposal, awaiting approval

## Goal

Make our shadow-live environment a faithful proxy for live Polymarket trading, so any strategy or execution change validated in shadow can be deployed live with directional confidence. The bonereaper strategy ships as the calibration probe: because we have a real on-chain wallet running an identical strategy (`0xeebde7a0e019a63e6b476eb425505b7b3e6eba30`), we have a continuous ground-truth feed against which to measure shadow fidelity. Bonereaper is also a real shipping strategy in its own right, deployed at micro-capital on the existing AWS Dublin bot.

## Non-goals

- Multi-strategy revalidation. We do not in this spec re-run `btc_5m_mm` or other in-house strategies through the new simulator. They will work by construction; full revalidation is a follow-on.
- WS connection multiplexing. Running live and shadow in one process sharing one WS pool is a v2 efficiency improvement, not v1.
- Geoblock / VPN infrastructure. The AWS Dublin bot already has VPN connectivity; this spec assumes that substrate.
- Shadow-live for strategies other than bonereaper. The native bonereaper market universe (`eth-updown-5m`, `eth-updown-15m`, `btc-updown-5m`, `btc-updown-15m`, `btc-updown-4h`) is the v1 scope.
- Performance benchmarking. Bonereaper at ~100 markets × ~1 quote/sec fits well within current Tokio capacity.

## Background

The `paper_fill_from_book_snapshot` function in `polymarket-exec/src/runtime/runner.rs` is the documented bottleneck. Memory records that even with `post_only_reject_probability=0` and `safety_ticks=1`, paper runs produce zero fills, despite the strategy emitting bids that bonereaper's wallet would have been filled on. The function is heuristic and conservative. It ships zero fills because it cannot tell whether a real trade would have hit our standing order; it only sees the static book at the snapshot moment.

The fix is to drive shadow fills from the actual Polymarket trade tape: when a real trade prints at our price, our standing shadow order at that price gets filled, with an FIFO queue-priority discount based on how much depth was ahead of us when we posted.

Bonereaper is the calibration target because its strategy (`mid - 1 tick` paired BUYs across many markets, hold to resolution) is simple, breadth-first, and produces enough fills (~990 over the comparator's recent window) to fit a one-parameter queue model with reasonable confidence. Because we will also ship our own bonereaper strategy live on the AWS box, the divergence between our shadow's predicted fills and our live wallet's actual fills becomes a continuous fidelity probe.

## Architecture overview

Five new in-engine components, two existing components extended, one new systemd unit on AWS, one Python diagnostic visualizer extended.

```
                        Polymarket WS
                          │   │
                     depth│   │trade tape
                          ▼   ▼
                      ┌──────────┐
                      │ market_  │ ◄── existing
                      │   ws.rs  │
                      └──┬────┬──┘     ┌────────────────┐
                         │    │        │ trade_ws.rs    │ ◄── NEW
              book       │    │ trades │  emits         │
              snapshots  │    │        │  TradeEvent    │
                         ▼    ▼        └───────┬────────┘
                    ┌──────────────────────────┼─────────────┐
                    │  strategy_bonereaper     │             │
                    │   on_market_snapshot ──► OrderIntent   │
                    │                          │             │
                    │       ┌──────────────────┘             │
                    │       ▼                                │
                    │  PaperExecutionAdapter ──► trade_tape::ShadowBook
                    │                            ▲           │
                    │                            │           │
                    │  TradeEvent  ──────────────┴► queue_model
                    │                                        │
                    │                            PaperFill ──┤
                    └────────────────────────────────────────┤
                                                             ▼
                                    ┌─────────────────────────────────────┐
                                    │  journal.jsonl                      │
                                    │  (existing schema +                 │
                                    │     new event types §3.2)           │
                                    └────────────────┬────────────────────┘
                                                     ▼
                                          ┌──────────────────┐
              bonereaper /activity API ──►│ runtime/         │
                                          │   fidelity.rs    │
                                          │  emits           │
                                          │  FidelityEvent   │
                                          └─────────┬────────┘
                                                    │
                                                    └──► journal.jsonl
```

Key invariants:
1. Every `OrderIntent` from the strategy passes through `PaperExecutionAdapter` and registers in `ShadowBook`. There is no path that bypasses the shadow book.
2. Every `TradeEvent` is processed against `ShadowBook` exactly once (dedup by `trade_id`).
3. `PaperFill`s emitted from the queue model flow through the *same* journal/inventory pipeline as live fills do today. Strategy and inventory code cannot tell shadow from live.
4. `FidelityEvent`s emit on a fixed clock (every 60s), independent of trade-event rate, so empty-market periods still produce data points.
5. Live bonereaper deployment is gated on shadow `verdict != FAIL` over a rolling 24h. Shadow is the deploy gate.

## Components

### Trade-tape WS subscription — `polymarket-exec/src/wire/trade_ws.rs` (new)

Subscribes to Polymarket's public trade-event WS topic and emits each executed print as a typed `TradeEvent` into the runtime channel.

```rust
pub struct TradeTapeClient { /* ws conn, subscribed assets */ }

pub struct TradeEvent {
    pub asset_id: InstrumentId,
    pub side: TradeSide,           // taker side
    pub price: f64,
    pub size: f64,
    pub event_at_ms: EpochMillis,
    pub trade_id: String,          // for dedup
}

impl TradeTapeClient {
    pub async fn connect(url: &str) -> Result<Self>;
    pub async fn subscribe(&mut self, asset_ids: &[InstrumentId]) -> Result<()>;
    pub fn events(&mut self) -> impl Stream<Item = TradeEvent>;
}
```

Reuses patterns from existing `wire/market_ws.rs`: auth, reconnect, subscribe-on-discover. Discovery integrates with the existing `MARKET_DISCOVERY_INTERVAL_MS` loop so newly-found markets get trade-tape subscriptions automatically.

**Verification before coding:** Polymarket's WS contract may not expose a per-trade topic distinct from the depth/last-trade-price stream we already consume. If trade events must be inferred from `last_trade_price` deltas, this component synthesises `TradeEvent`s from depth deltas instead. Inferred trade synthesis is acceptable; it becomes a v0 of `trade_ws.rs` with a `synthesised: true` flag on the event for journal traceability.

### Shadow standing-order book + queue model — `polymarket-exec/src/paper/trade_tape.rs` (new)

In-memory book of every standing shadow order. On each `TradeEvent`, decides which shadow orders fill.

```rust
pub struct ShadowBook {
    orders: HashMap<ClientOrderId, ShadowOrder>,
    by_market_price: BTreeMap<(InstrumentId, OrderedFloat<f64>), Vec<ClientOrderId>>,
}

struct ShadowOrder {
    intent: OrderIntent,
    posted_at_ms: EpochMillis,
    depth_ahead_at_post: f64,   // total depth at our price level when we placed
    remaining_qty: f64,
    cumulative_volume_at_or_better: f64,
}

impl ShadowBook {
    pub fn on_submit(&mut self, intent: OrderIntent, book: &BookSnapshot, now_ms: EpochMillis);
    pub fn on_cancel(&mut self, client_order_id: &ClientOrderId);
    pub fn on_trade_event(&mut self, trade: &TradeEvent, decay: f64) -> Vec<PaperFill>;
}
```

Behavior on `on_trade_event`: for each shadow order at-or-better than `trade.price` on the same asset and resting side:
1. Update `cumulative_volume_at_or_better += trade.size`.
2. Compute `depth_ahead_remaining` per the queue model formula in §3.
3. Allocate fill: `fill_qty = max(0, min(remaining_qty, trade.size - depth_ahead_remaining))`.
4. If `fill_qty > 0`, emit `PaperFill`, decrement `remaining_qty`.

FIFO assumption is explicit. When a fill outcome strongly disagrees with FIFO (e.g., we predict zero but the same-window /activity shows bonereaper got hit), the fidelity scorer records a `fifo_violation` counter for later review.

Order expiry: the shadow book honors each `OrderIntent`'s `time_in_force` and `expires_at_ms` exactly as the live venue would. Expired orders are removed on the next `on_trade_event` or on a 1-second sweep clock, whichever fires first. This bounds memory growth during long trade-tape outages: if WS is down for 5 minutes, expired orders age out cleanly rather than accumulating.

### Self-calibrating queue model — `polymarket-exec/src/paper/queue_model.rs` (new)

The single tunable parameter `queue_decay_rate_per_sec` is *internal state*, not an environment variable. The model maintains a rolling estimate updated from observed fill outcomes.

Decay formula:
```
depth_ahead_remaining = max(
    0,
    depth_ahead_at_post
        - queue_decay_rate_per_sec * elapsed_seconds
        - cumulative_volume_at_or_better_since_post
)
```

Update logic: on each observed fill (live wallet fill in production, joined `/activity` fill in shadow), the model adjusts `queue_decay_rate_per_sec` toward the maximum-likelihood estimate over the recent observation window. Implementation: an EMA over per-fill ML estimates, with an `n_observations` floor before the parameter is considered valid.

State persistence: model state checkpoints to disk every minute alongside existing strategy state. On warm restart, the model resumes its calibrated state. On parse failure, cold-starts and warns loudly.

### In-engine fidelity scorer — `polymarket-exec/src/runtime/fidelity.rs` (new)

Periodically polls bonereaper's `/activity` endpoint (every 60s, public REST), joins against the in-process intent journal within ±5s, and emits `FidelityEvent` to the journal per market family per minute.

```rust
pub struct FidelityScorer { /* http client, last_seen_ms, market_family_index */ }

pub struct FidelityEvent {
    pub market_family: String,
    pub window_start_ms: EpochMillis,
    pub window_end_ms: EpochMillis,
    pub shadow_fill_count: u32,
    pub shadow_fill_notional: f64,
    pub shadow_rebate_usd: f64,
    pub bonereaper_fill_count: u32,
    pub bonereaper_fill_notional: f64,
    pub bonereaper_rebate_usd: f64,
    pub mape_fill_count: f64,
    pub verdict: FidelityVerdict,
}

pub enum FidelityVerdict { Ok, Warn, Fail }
```

Verdict bands are constants in source: `Ok` if MAPE ≤ 0.30, `Warn` if 0.30 < MAPE ≤ 0.75, `Fail` otherwise. Tuning happens by editing source with a commit message explaining why; never via env.

### Bonereaper strategy hardening — `polymarket-exec/src/strategy_bonereaper.rs` (edit)

Two additions:

1. **Position cap** routed through existing `core/risk.rs` (no new env knob). If a per-instrument position cap path does not yet exist there, add it generally so all strategies benefit. Discovery task (5min, before implementing): grep `core/risk.rs` for `max_position_qty_per_market`-style logic; if absent, the design adds it.

2. **Resolution-redeem path verification.** Bonereaper holds to resolution and never merges. The CLAUDE.md note says runtime handles redeem on resolution via the existing path; verify with a fixture-driven integration test rather than trusting prose. Add `paper_market_close_with_redeem_settles_stranded_inventory`-style scenario for bonereaper.

The core `mid - 1 tick` math, breadth-first quoting, and no-cooldown design are intentional and unchanged.

### Shadow-live runner switch — `polymarket-exec/src/runtime/runner.rs` (edit)

In `run_shadow_live`, route paper fills through `trade_tape.rs` instead of `paper_fill_from_book_snapshot`. The switch is internal Rust dispatch keyed on the existing `WHALE_PAIR_EXEC_MODE=shadow_live` runtime mode, not a new env knob. `paper_fill_from_book_snapshot` stays unchanged for offline replay (`run_replay_cli`), since offline replay has no live trade tape to consume.

### Live-deploy gate (cross-process)

The bonereaper-live process on AWS reads the most recent `fidelity_event` rows from the shadow process's journal file (path resolved through the existing `WHALE_PAIR_EXEC_JOURNAL_PATH` mechanism — both processes are on the same AWS box and the live process resolves shadow's journal path through a sibling-process-discovery convention added to `runtime/fidelity.rs`).

Gate evaluation, on startup and every 60s thereafter:

- Read all `fidelity_event` rows from shadow's journal whose `window_end_ms` falls within the last 24 hours of wall-clock time.
- If zero rows are present (shadow not running, journal not produced, or shadow in cold-warmup), gate returns `RuntimeStatus::RiskOff` with reason `shadow_unobserved`. This treats absence-of-evidence as evidence-of-absence on purpose: live cannot proceed without active fidelity validation.
- If any `verdict == Fail` row is present in the window, gate returns `RuntimeStatus::RiskOff` with reason `shadow_fail`.
- Otherwise gate returns `RuntimeStatus::Running`.

Implemented as a fidelity-aware extension of the existing `RuntimeStatus` plumbing in `core/types.rs`. No new env coupling; the live process discovers the shadow journal path through the same `WHALE_PAIR_EXEC_JOURNAL_PATH` env that already exists, but resolved against shadow's well-known systemd unit-name convention rather than its own.

### Diagnostic Python visualizer — `polymarket-exec/scripts/bonereaper_exec_compare.py` (extend)

Reads the journaled `fidelity_event` and `queue_model_estimate` rows from shadow's journal. Adds:
- `--fidelity-trend` mode: produce a time-series plot of `verdict` and `mape_fill_count` per market family over a date range.
- `--queue-decay-trend` mode: same for `queue_decay_rate_per_sec`.

Existing fill-classification logic stays intact. The script is for human-readable post-hoc analysis and is **not** on the calibration critical path; the engine self-calibrates with or without this script.

## Data flow + journal schema

### Journal additions (no schema break)

Three new categories on the existing `JournalEvent { seq, observed_at_ms, category, message, market_id?, instrument_id?, client_order_id?, metrics }` envelope:

`category: "trade_tape_event"` — one row per inbound `TradeEvent`:
```jsonc
{
  "category": "trade_tape_event",
  "instrument_id": "<asset>",
  "metrics": {
    "trade_id": "<string>",
    "price": 0.49,
    "size": 12.0,
    "taker_side": "buy",
    "event_at_ms": 1745000000000,
    "synthesised": false
  }
}
```

`category: "queue_model_estimate"` — one row per minute per market family:
```jsonc
{
  "category": "queue_model_estimate",
  "metrics": {
    "market_family": "btc-updown-5m",
    "queue_decay_rate_per_sec": 1.45,
    "n_observations": 87,
    "log_likelihood": -34.2,
    "ema_age_minutes": 60
  }
}
```

`category: "fidelity_event"` — one row per minute per market family:
```jsonc
{
  "category": "fidelity_event",
  "metrics": {
    "market_family": "btc-updown-5m",
    "window_start_ms": 1745000000000,
    "window_end_ms": 1745000060000,
    "shadow_fill_count": 14,
    "shadow_fill_notional": 280.0,
    "shadow_rebate_usd": 0.42,
    "bonereaper_fill_count": 18,
    "bonereaper_fill_notional": 360.0,
    "bonereaper_rebate_usd": 0.54,
    "mape_fill_count": 0.22,
    "verdict": "OK"
  }
}
```

### Persistence and replay

- All three new event types append to the existing `WHALE_PAIR_EXEC_JOURNAL_PATH`. No new files.
- Existing `WHALE_PAIR_BOOK_SNAPSHOT_LOG_PATH` continues to capture book state for offline analysis.
- Crash + resume: queue model rolling-estimator state checkpoints alongside existing strategy state. On cold start: `queue_decay_rate_per_sec = NaN` (uncalibrated), zero fills emitted, warmup-progress log every 5 minutes.

## Error handling

| Failure | Detection | Response |
|---|---|---|
| Trade-tape WS disconnects | WS keepalive miss | Pause new shadow fills, journal `category: "trade_tape_disconnected"` + warn. Do not silently fall back. Resume on reconnect, journal `trade_tape_resumed`. Strategy keeps emitting intents; they accumulate in `ShadowBook`. |
| `/activity` API call fails | HTTP error / parse failure | Skip that minute's `fidelity_event`, journal `category: "fidelity_scorer_error"` with status code. Next minute's poll uses the same `last_seen_ms`; truth data is delayed, not lost. |
| Queue model warmup | `queue_decay_rate_per_sec.is_nan()` | Emit zero shadow fills, journal `queue_model_estimate` with `n_observations`, log warmup progress every 5 minutes. Honest "we don't know yet" rather than guessing. |
| Stale book depth (sequence-number gap) | Sequence-number gap on book WS | Reject the affected order's shadow registration, journal `category: "shadow_book_stale_reject"`. Strategy re-emits on next snapshot tick. |
| Persistence corruption on restart | Queue-model state file fails parse | Cold-start the rolling estimator, journal `queue_model_warm_restart_failed`. Costs warmup time, does not crash. |

Default principle: fail loud, never silently fall back to a known-pessimistic model. CLAUDE.md "no bandaids" + "All testing optimizes for LIVE validation".

## Testing strategy

### Unit tests (Rust, in-module)

- `queue_model.rs`: table-driven tests over `(depth_ahead_at_post, elapsed_seconds, prior_volume, decay_rate)` → `depth_ahead_remaining`. Cover warmup, mid-life, depleted, FIFO violation log path.
- `trade_tape.rs`: `on_submit` / `on_cancel` / `on_trade_event` round-trips. One test per failure mode in §4. Verify `PaperFill` events populate journal with correct fee/rebate accounting.
- `fidelity.rs`: synthetic intent journal + synthetic `/activity` response → assert correct `fidelity_event` rollup (per-family MAPE, verdict bands).

### Integration tests (Rust, `polymarket-exec/tests/`)

- `shadowlive_trade_tape_scenarios.rs` (new) — fixture-driven: feed canned WS depth + trade events, run bonereaper strategy, assert journal contains expected `PaperFill` count and prices. Mirrors `paper_market_close_with_redeem_settles_stranded_inventory` pattern in `btc_5m_mm_scenarios.rs`.
- Regression fixture for queue calibration: record one shadow-live session of bonereaper data (~hour), commit a slimmed snippet under `tests/fixtures/`, assert calibrator converges to a parameter in `[0.5, 5.0]` /sec range.

### Cross-cutting

- TDD-before-merge: every commit on this branch has a failing test before its fix. Per memory `feedback_tdd_before_deploy`.
- Property test for the queue model: shadow fill count over a synthetic window monotonically increases as `queue_decay_rate_per_sec` increases, all else equal. Catches sign-flip bugs.

### Live validation

The fidelity scorer IS the live regression test. Once shadow runs on AWS, the `fidelity_event` stream is the gate.

PRs that touch `paper/`, `strategy_bonereaper.rs`, or `runtime/` are validated as follows in v1:

1. The author deploys the PR's branch to AWS shadow as a transient unit (existing systemd-override pattern).
2. Shadow runs for ≥ 24 wall-clock hours.
3. The diagnostic visualizer (`bonereaper_exec_compare.py --fidelity-trend`) is run against the shadow journal, producing a per-family verdict roll-up.
4. The author posts the verdict roll-up in the PR description; reviewers gate merge on `OK` across all families for the full 24h window.

This is operational, not coded — no CI integration in v1. Codifying the gate as automated CI is a follow-on.

## Operational deployment

A second systemd unit on the existing AWS Dublin bot, pointing at the engine binary with `WHALE_PAIR_EXEC_MODE=shadow_live` and the existing `polymarket-exec/env/bonereaper_shadowlive.env`. No new shell script required; the existing systemd substrate from `dublin_tinylive.sh` is the precedent.

The live bonereaper process is a third systemd unit on the same box, micro-capital ($5-10 clip), `WHALE_PAIR_EXEC_MODE=live`, gated on shadow's fidelity verdict per §2.7.

## Open risks

1. **Polymarket trade WS topic existence.** Must verify whether per-trade events are exposed distinctly from depth WS. If not, `trade_ws.rs` falls back to synthesising trade events from depth deltas. Acceptable v0; tag `synthesised: true` for journal traceability.
2. **FIFO assumption in queue model.** Polymarket may not match orders strictly FIFO. If single global decay rate underfits, v2 adds per-market-family decay rates as internal state (still no env knob).
3. **Calibration data sufficiency.** ~990 fills from bonereaper over the comparator window may not be enough for per-family decay rates. Confirmed adequate for one global parameter.
4. **Shadow-live process resource share with live tinylive.** Two extra processes on the same AWS box (shadow + live bonereaper) plus existing tinylive. No measurement yet; CPU and memory headroom check before deploying both.
5. **`/activity` polling rate.** 60s cadence × per-process is low load. If cadence needs to drop to 10s for tighter calibration, requires a rate-limit verification with Polymarket; not anticipated.

## Decisions log

| # | Decision | Rationale |
|---|---|---|
| D1 | Trade-tape replay over heuristic fill-rate tuning | "No bandaids — fix the core engine" |
| D2 | Bonereaper as calibration target | Real on-chain wallet, simple strategy, ~990 fills available |
| D3 | Bonereaper ships live at micro-capital | C in scope, AWS+VPN already running |
| D4 | Shadow runs on AWS, not local Mac | Same network conditions as live; calibration parameters transfer |
| D5 | No new env knobs for queue model | "Constants bound the bot; signals optimize it" |
| D6 | Live deploy gated on shadow `verdict != Fail` over rolling 24h | Whole point of shadow is to be a deploy gate |
| D7 | WS multiplexing deferred to v2 | Don't bite off refactor while validating the simulator |
| D8 | Multi-strategy revalidation deferred | Bonereaper-only as v1 calibration target |
| D9 | Python diagnostic visualizer kept | Off-critical-path, useful for human review, doesn't violate "no bandaids" |
| D10 | Fail-loud on every error path | "All testing optimizes for LIVE validation" |
