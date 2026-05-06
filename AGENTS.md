# Engineering principles

## Calibration sources — always reference whale data first

Before tuning ANY strategy knob, check what we know about the operators
who already make this work. Don't tune in a vacuum.

| Source | Use for |
|---|---|
| [whale-research-index.md](docs/research/whale-research-index.md) | Index of all wallet threads |
| [unlawful-shear-reconstruction-thread.md](docs/research/unlawful-shear-reconstruction-thread.md) | unlawful's canonical strategy reconstruction |
| [unlawful-shear-signal-pack-spec.md](docs/research/unlawful-shear-signal-pack-spec.md) | Signal-level spec |
| [unlawful-shear-microstructure-spec.md](docs/research/unlawful-shear-microstructure-spec.md) | Microstructure observations |
| [unlawful-whale-vs-us-calibration.md](docs/research/unlawful-whale-vs-us-calibration.md) | Direct comparison vs our bot |
| [bonereaper-research-thread.md](docs/research/bonereaper-research-thread.md) | bonereaper as second benchmark |
| `data/research/wallet_research/unlawful-shear/wallet_research.db` | Raw SQLite for deeper queries |
| `data/research/wallet_research/unlawful-shear/unlawful_signal_pack.json` | Machine-readable signal pack |
| [BANDAIDS.md](BANDAIDS.md) | Catalog of known anti-patterns to avoid |

**Empirically-validated facts** (don't re-derive these wrong):
- unlawful's strategy is **paired MM + convex/cheap-leg accumulation**, NOT
  directional latency arb. Most fills look one-sided because aggressive
  bilateral laddering produces single-side fills when book moves.
- unlawful **narrowed from multi-asset/multi-timeframe to ONLY 5min BTC**.
  Concentration > diversification at production scale.
- unlawful is **bootstrapped from $2K** of external capital — the rest
  ($228K balance) came from trading + rebates compounded.

When proposing any strategy change, the first question to answer is
"what does the whale data say?" If the answer is "we haven't measured,"
go measure first.

## Gate calibration: prefer signals over hardcoded constants

When adding a threshold (timing, magnitude, count), the V1 implementation
can use a constant for safety, but the V2 should be SIGNAL-DERIVED:
- Trend persistence threshold → scaled by `btc_regime.realized_vol_5m_bps`
- Bar-relative timing → fraction of `bar_window_ms` (works for 5m, 15m,
  any future timeframe)
- Bid count caps → `max_leg_cost / typical_clip` (capital-aware)
- Hit-rate-driven self-feedback (rolling P&L tracking) where possible

Constants get the bot bounded; signals get it optimal. Plan the V2 in
the same PR's commit message even if you ship V1.

## Env var alignment

Launcher (`scripts/*.sh`) env exports MUST match what `config/mod.rs`
parses, exactly. We've shipped 2 silent-failure bugs from this:

- `PM_BTC_5M_JOURNAL_PATH` (launcher) vs `PM_BTC_5M_EXEC_JOURNAL_PATH`
  (parser) → tinylive ran with no decision log on disk.
- `PM_BTC_5M_LIVE_AUTO_REDEEM` parsed by binary but never set by any
  launcher → auto-redeem silently disabled in production.

**Before shipping any new env knob:** grep both directions
(`grep -rn "MY_NEW_VAR" polymarket-exec/src/ scripts/`) and verify
the names match. If you remove a knob from the binary, also remove
from env files / launcher. If you remove from launcher, remove from
binary.

A coverage table belongs in BANDAIDS.md so we don't drift.

## No bandaids — fix the core engine

When a behavior is missing or broken, the fix goes IN THE RUST ENGINE
(`polymarket-exec/src/`), NOT in a bash supervisor, env wrapper, polling
script, cron, or any other shell-level workaround.

If the answer to "why doesn't X work" is "the engine doesn't have a code
path for X yet" — then add the code path to the engine. Do not paper
over it with shell.

**Examples of what NOT to do:**
- "Engine reads markets from env vars at startup only" → DO NOT wrap in a
  bash loop that restarts the bot when markets change. ADD periodic
  context refresh inside `runtime/runner.rs` that subscribes WS to new
  markets dynamically.
- "Engine doesn't know when to stand down in flat tape" → DO NOT add an
  env override or wrapper-side gate. ADD a regime gate in `strategy.rs`.
- "Engine doesn't track in-flight rescues so we over-rescue" → DO NOT
  add a sleep or per-tick throttle in shell. ADD per-market rescue
  tracking in `market_states`.

**Deploy scripts (`scripts/*.sh`) should ONLY:**
- Load env vars / secrets
- Launch the binary

They should NOT contain business logic, retry loops, polling, market
context refresh, or anything that needs to know about strategy/runtime
state. Process supervision via systemd is fine; bash supervision around
the bot's domain logic is not.

**Why this matters:** ad-hoc shell wrappers ship faster but they kill
in-flight engine state every restart, can't share state with the
engine's data structures, hide bugs from the type checker / tests, and
require operators to understand TWO systems instead of one. The Rust
engine is the source of truth.

## Engineering workflow

We are building an execution and strategy engine, not a pile of wrappers.
Fix defects in the engine with robust typed code paths, focused tests, and
Rust best practices. Keep modules small enough to reason about; when a file is
too large, prefer narrow extractions that preserve behavior before adding more
logic.

Use the local Matthew Pocock engineering skills where they fit:
- `diagnose` for bug validation and root-cause work.
- `tdd` for red/green implementation of behavior changes.
- `improve-codebase-architecture` for cleanup that reduces complexity without
  changing behavior.
- `zoom-out` when the local fix needs broader execution-engine context.
