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

## Strategy intent: paired MM + convex asymmetric payoff

The `btc_5m_mm` strategy runs TWO complementary entry paths, not one.
Future agents have repeatedly over-suppressed one to "fix" the other.
Don't do that.

**Path 1 — Paired bidding (the rebate workhorse).**
Quote both legs (Up + Down) at fair − edge, capture maker rebates on
fills, merge paired inventory back to $1 collateral. Works in flat /
mid-priced markets where both legs are near 50/50. This is where most
of our day-to-day revenue comes from.

**Path 2 — Convex accumulation (the asymmetric payoff side bet).**
When paired entry is suppressed because one leg is at premium prices
(e.g., Up=$0.95, Down=$0.05), the strategy buys the cheap leg at
≤ $0.45 in small size, betting on rare reversal. Pays off ~5-15% of
the time but pays 5-20× when it does. This is NOT a separate strategy —
it's the second arm of the same one. The asymmetric payoff is a core
design intent.

**What this strategy is NOT.**
- NOT directional momentum chasing. We do not buy the winning side
  at $0.95 expecting $1 payout. That's a different strategy
  (latency-arb-against-spot) we don't currently run.
- NOT pure paired-only. Suppressing convex_accum because "one side is
  too expensive" kills the asymmetric payoff. Cooling state should
  pause paired bidding while still attempting convex_accum where the
  reason permits (premium fair cap, market mid moved, btc trending).

**Hard suppression triggers** (skip ALL entry paths, including convex):
asymmetric entry-fill cooldown, post-fill cooldown, btc regime
inactive, runtime degraded.

**Soft suppression triggers** (skip paired, allow convex):
premium fair cap, market mid moved, btc regime trending.

When introducing a new gate, ASK yourself: does this gate hurt paired
behavior, convex behavior, or both? Encode the answer in the gate's
return type (e.g., `GateOutcome::{Allow, SuppressPaired, SuppressAll}`)
rather than a string-prefix classifier downstream.

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

- `WHALE_PAIR_JOURNAL_PATH` (launcher) vs `WHALE_PAIR_EXEC_JOURNAL_PATH`
  (parser) → tinylive ran with no decision log on disk.
- `WHALE_PAIR_LIVE_AUTO_REDEEM` parsed by binary but never set by any
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

<!-- gitnexus:start -->
# GitNexus — Code Intelligence

This project is indexed by GitNexus as **polymarket-agent** (3668 symbols, 7760 relationships, 300 execution flows). Use the GitNexus MCP tools to understand code, assess impact, and navigate safely.

> If any GitNexus tool warns the index is stale, run `npx gitnexus analyze` in terminal first.

## Always Do

- **MUST run impact analysis before editing any symbol.** Before modifying a function, class, or method, run `gitnexus_impact({target: "symbolName", direction: "upstream"})` and report the blast radius (direct callers, affected processes, risk level) to the user.
- **MUST run `gitnexus_detect_changes()` before committing** to verify your changes only affect expected symbols and execution flows.
- **MUST warn the user** if impact analysis returns HIGH or CRITICAL risk before proceeding with edits.
- When exploring unfamiliar code, use `gitnexus_query({query: "concept"})` to find execution flows instead of grepping. It returns process-grouped results ranked by relevance.
- When you need full context on a specific symbol — callers, callees, which execution flows it participates in — use `gitnexus_context({name: "symbolName"})`.

## Never Do

- NEVER edit a function, class, or method without first running `gitnexus_impact` on it.
- NEVER ignore HIGH or CRITICAL risk warnings from impact analysis.
- NEVER rename symbols with find-and-replace — use `gitnexus_rename` which understands the call graph.
- NEVER commit changes without running `gitnexus_detect_changes()` to check affected scope.

## Resources

| Resource | Use for |
|----------|---------|
| `gitnexus://repo/polymarket-agent/context` | Codebase overview, check index freshness |
| `gitnexus://repo/polymarket-agent/clusters` | All functional areas |
| `gitnexus://repo/polymarket-agent/processes` | All execution flows |
| `gitnexus://repo/polymarket-agent/process/{name}` | Step-by-step execution trace |

## CLI

| Task | Read this skill file |
|------|---------------------|
| Understand architecture / "How does X work?" | `.claude/skills/gitnexus/gitnexus-exploring/SKILL.md` |
| Blast radius / "What breaks if I change X?" | `.claude/skills/gitnexus/gitnexus-impact-analysis/SKILL.md` |
| Trace bugs / "Why is X failing?" | `.claude/skills/gitnexus/gitnexus-debugging/SKILL.md` |
| Rename / extract / split / refactor | `.claude/skills/gitnexus/gitnexus-refactoring/SKILL.md` |
| Tools, resources, schema reference | `.claude/skills/gitnexus/gitnexus-guide/SKILL.md` |
| Index, status, clean, wiki CLI commands | `.claude/skills/gitnexus/gitnexus-cli/SKILL.md` |

<!-- gitnexus:end -->