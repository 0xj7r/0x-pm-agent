# Replay Event Synthesizer

The v=1 canonical event stream produced by the live collector contains
`market_meta`, `btc_tick`, `book_*`, `trade`, and `user_*` records but no
explicit "window-open" or "window-close" markers. The replay synthesizer
(`replay::synthesizer::EventSynthesizer`) fills that gap by deriving two
new event types and injecting them into the in-replay stream:

- `event_type=price_to_beat` at window-open, carrying the BTC oracle
  price observed closest to `window_start_ts_ns`.
- `event_type=resolution` at window-close, carrying the winning outcome
  derived from the most recent BTC tick relative to the strike.

## Why we synthesize

Strategies that need to gate on "do I have a recent BTC oracle reading
for this exact window?" or "did this window resolve UP or DOWN?" today
infer those moments from raw stream-watching, which is brittle. The
synthesizer provides a typed signal so future strategy code can branch
on it directly. In the live collector path the same two events should
eventually be emitted by the discovery / oracle layers; for replay we
derive them deterministically from the stream we already have.

## Synthesis trigger rules

- **`price_to_beat`**: emitted exactly once per `(market_slug, window)`
  pair. Triggered on the first `market_meta` for that window when at
  least one BTC tick has been observed; otherwise queued and emitted on
  the next BTC tick. Subsequent `market_meta` events for the same
  window (live runtime "context refresh") are idempotent.
- **`resolution`**: emitted exactly once per market when virtual time
  crosses `window_end_ts_ns`. The triggering event can be any later
  event (BTC tick, trade, book update, etc.). The `resolution` event's
  `received_ns` is pinned to `window_end_ts_ns` so it sorts immediately
  before any post-window real event.

## Resolution outcome fallback chain

The outcome label on `resolution` follows this fallback chain:

1. **Live emit** (`resolution_source = "polymarket_market_ws"`): a
   future v=1 stream may carry an authoritative resolution event from
   the venue. Not implemented today.
2. **`market_meta` resolved field**
   (`resolution_source = "synthesizer_market_meta"`): if a later
   `market_meta` for the same window carries a `resolved` /
   `winning_outcome` field, the synthesizer adopts it. Today the
   discovery payload does not include this field, so the path is
   reserved.
3. **Synthesizer onchain fills**
   (`resolution_source = "synthesizer_onchain_fills"`): default. The
   synthesizer compares the last observed BTC tick against the strike
   and labels the winning outcome accordingly.

When `strike` is missing from `market_meta`, no `resolution` is
emitted (the synthesizer cannot derive a winner) and the gap is
silent. The same applies when no BTC tick has been observed by
`window_end_ts_ns`.

## Determinism

The synthesizer is a pure function of (a) the prior event stream and
(b) the synthesizer's own `BTreeMap` state. No clock reads, no RNG, no
hash iteration. Two replays of the same input stream produce
byte-identical synthetic events.

## Wiring

`replay::runner::run_window` instantiates one `EventSynthesizer` per
window and dispatches synthetic events through `dispatch_event` BEFORE
the real event that triggered them. Synthetic events flow through the
strategy adapter exactly like real ones.

## Known gap (Phase 3c finding, deferred)

The current `paired_mm` strategy does NOT gate on `price_to_beat` or on
"BTC feed staleness". Phase 3c surfaced this as a live-failure mode: the
strategy continues quoting paired entries when `price_to_beat` is
missing or when the most recent BTC tick is older than 90s. The
synthesizer makes those signals available, but the strategy must be
extended to honor them in a follow-up. Until then the synthetic events
have no impact on strategy decisions, and the golden PnL from Phase 3c
remains stable.
