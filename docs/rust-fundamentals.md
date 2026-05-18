# Rust Fundamentals — `polymarket-exec` in Practice

A one-pager mapping the language features the engine relies on to the concrete code that uses them. Follow the file:line citations to read the real example.

## 1. Ownership, borrowing, and "no aliasing of mutable state"

Every value has exactly one owner. Functions either *take ownership* (consume the value), *borrow immutably* (`&T`, many readers), or *borrow mutably* (`&mut T`, exactly one writer). The compiler refuses code that violates this.

- `polymarket-exec/src/runtime/mod.rs:52` — `Runtime` owns the `InventoryState`, `MergeExecutor`, `OrderStore`, etc. Nothing else can mutate them concurrently because `Runtime` itself is the unique owner.
- `core/inventory.rs` — `&mut self` on the apply-fill / reserve-cash methods enforces that no other reference can read inconsistent inventory mid-update. This is the Rust answer to "stranded inventory because two writers raced": a second writer is a compile error, not a runtime bug.
- `paired_mm/engine.rs:183` `decide<M>(&self, input: &PairedMmInput<M>)` — the engine borrows the input immutably so the same snapshot can be passed to many decision branches without cloning.

**Practical takeaway:** when you find yourself wanting to `.clone()` a big struct just to satisfy the borrow checker, the right move is usually to borrow `&` from a single owner, or to split the struct so the parts you mutate are smaller.

## 2. `Result` / `Option`: errors and absence are values, not exceptions

Rust has no exceptions. Functions that can fail return `Result<T, E>`, functions that can be empty return `Option<T>`. The `?` operator propagates either. This forces every fallible call to be acknowledged at the call site.

- `core/inventory.rs` — `apply_settlement` returns `Result<InventoryAdjustment, InventoryError>`; the caller cannot accidentally ignore an oversell. This is what "your accounting can't silently drift" looks like as a type.
- `market_making/pairing/rescue_engine.rs:61` `choose_rescue` returns a `RescueDecision` with `RescueAction::{Hold, BuyOppositeForMerge, SellStrandedLeg}`. The "we did nothing" outcome is a *named variant*, not a sentinel `null`.
- `runtime/mod.rs:1180` `inventory_merge_intent(...) -> Option<MergeIntent>` — single-leg / both-zero markets return `None`, and the runtime's match on the result decides whether to push a `RuntimeCommand::Merge`.

**Practical takeaway:** if you see `.unwrap()` or `.expect(...)` outside tests, that is a fail-fast assertion — the author claims this branch is unreachable. Treat that claim like a comment that needs to be re-checked when invariants change.

## 3. Enums replace stringly-typed flags

Rust enums carry data per variant and the compiler forces you to handle every case in `match`. This is what kills the `if tag.starts_with("...")` family of bugs.

- `core/types.rs:208-230` `IntentKind { Entry, Close }` — replaces five separate `quote_level_tag.starts_with("mm-hedge-rescue")` checks. Adding a new gate cannot accidentally trap a `Close` intent, because `match kind { Entry => …, Close => … }` is exhaustive.
- `core/types.rs:388` `MmQuoteKind { PairedEntry, CapitalRecycle, ConvexAccumulation, HedgeRescue, ReduceOnlyExit, LateBarCore }` — promoted from substring sniffing for typed reporting.
- `core/types.rs:371-380` `RuntimeCommand { Submit, Cancel, Merge, Redeem, Noop }` — every command the runtime can emit is a variant; downstream consumers must handle each.

**Lingering smell:** `runtime/mod.rs:3501,3540`, `runtime/attribution.rs:126`, and `paper/report.rs:858` still do `tag.starts_with("mm-hedge-rescue")`. These should branch on `IntentKind::Close` instead — that's the third PR candidate from this session's audit.

## 4. Traits + generics: behavior without inheritance

A trait is a behavior contract. Generics let a function work for any type that satisfies the contract, with zero runtime overhead (monomorphization).

- `markets::MarketDescriptor` (used at `paired_mm/engine.rs:183`) — any market type that exposes `tick_size()`, `time_remaining_ms(now)`, `market_id()`, etc. plugs into the engine. Tests pass a fake; production passes the real `polymarket-exec` market.
- `market_making/pairing/rescue_engine.rs:57` `RescueIntentBuilder` — the EV brain produces a pure `RescueDecision`. A separate object that knows venue rules (tick alignment, IOC padding, reduce-only flag) implements `build_rescue_intents`. **Pure decision logic stays untestable-by-venue.**
- `runtime/order_store.rs` `OrderStore` trait + `Box<dyn OrderStore>` on `Runtime` — durable order persistence is swappable (rusqlite in prod, in-memory in tests) without changing the runtime.

## 5. Async / Tokio: cooperative concurrency

`async fn` returns a `Future`. Futures don't run until polled. The Tokio runtime polls many futures concurrently on a thread pool. `.await` is a yield point.

- `wire/market_ws.rs`, `wire/user_ws.rs`, `wire/spot_ws.rs` — each WebSocket connection is an async task. They communicate with the synchronous strategy core via channels, *not* shared state.
- `main.rs` and operational binaries use `#[tokio::main]` to spin up the runtime.

**Footgun called out by clippy:** holding a synchronous `MutexGuard` across an `.await` deadlocks if any other task on the same thread also tries to lock. There's one such warning in tests (`tests/unit/runtime_runner.rs:747`); production code is clean. Use `tokio::sync::Mutex` or scope the guard before the await.

## 6. `Send` / `Sync`: thread-safety as types

`Send`: safe to move across threads. `Sync`: safe to share `&T` across threads. Most types are auto-derived; `Rc<T>` and raw pointers aren't. Tokio's spawn requires `Send`.

The runtime stays single-threaded by design (the strategy core is `!Sync` in spirit — only the runtime owns it). Async I/O fans out via channels rather than shared `Arc<Mutex<…>>` over hot state. This is a deliberate choice: it eliminates an entire class of partial-fill / stranded-inventory bugs that only manifest under interleaving.

## 7. Lifetimes: "this borrow doesn't outlive its source"

Annotations like `&'a T` only matter when a function returns a reference derived from one of its inputs. Most of the engine ducks lifetimes by *owning* the data it returns (intents, decisions, snapshots are all owned). When you do see explicit lifetimes here, it is usually around iterator chains in `core/inventory.rs` (`positions()` borrows the map).

**Rule of thumb:** prefer returning owned data from public APIs; reserve lifetime annotations for tight, internal helpers.

## 8. Pattern matching over data, not strings

Rust's `match` is the canonical control-flow primitive. Combined with enums, it forces exhaustiveness.

- `runtime/mod.rs:2447` matches on `RuntimeCommand` to dispatch Submit/Cancel/Merge/Redeem/Noop.
- `paired_mm/engine.rs:213` matches on `HardPolicyAction { Allow, SuppressPaired, ForceFlatten }` to decide whether to clear the ladder.
- `pairing/merge_policy.rs::MergePolicyDecision { MergeNow { reason }, Wait { reason } }` — both arms carry a `reason` string, so logging is uniform whatever the outcome.

If a new policy state appears, the compiler tells you every site that needs to be updated. That is the primary reason this codebase is safe to keep refactoring.

## 9. Newtypes: meaning encoded in the type system

Wrapping a `String` or `u64` in a struct makes the type system check what equality and assignment mean. `ClientOrderId`, `MarketId`, `InstrumentId`, `OrderId`, `EpochMillis` are all newtypes. You cannot pass a `MarketId` where an `InstrumentId` is expected, even though both are strings underneath. This catches "wrong-id" bugs at compile time — exactly the family that produces "merge tried with the wrong leg" incidents.

## 10. The big-picture patterns this codebase uses

| Pattern | Where | What it buys |
|---|---|---|
| Pure decision functions returning typed decisions | `pairing/rescue_engine.rs`, `pairing/merge_policy.rs`, `paired_mm/risk_boundary.rs` | Trivially unit-testable, no I/O, no globals |
| Decision → Intent adapter trait | `RescueIntentBuilder`, `Strategy` | Venue rules don't pollute the EV math |
| Single-owner runtime, channels for I/O | `runtime/mod.rs`, `wire/*` | No shared mutable state, no `Arc<Mutex>` over hot paths |
| Enum-driven branching (no string sniffing) | `IntentKind`, `RescueAction`, `HardPolicyAction`, `RuntimeCommand` | Compiler enforces exhaustiveness when you add a new state |
| Newtypes for identifiers | `ClientOrderId`, `MarketId`, `InstrumentId` | Wrong-id bugs become compile errors |

When you're tempted to add a `bool` flag, a stringly-typed tag, or a fallback that "just returns 0.0 if the data isn't there," the Rust answer is almost always: promote it to an enum, return `Option<T>`, or split the struct so the bad state is unrepresentable.
