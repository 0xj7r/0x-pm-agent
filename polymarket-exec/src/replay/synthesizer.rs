//! Replay-side event synthesizer.
//!
//! The live collector path emits `market_meta` events from the Polymarket
//! data API (slug, asset_ids, strike, end_time_ms) and `btc_tick` events
//! from the spot oracle. It does NOT emit explicit "window-open" or
//! "window-close" markers. Strategies that need to gate on
//! `price_to_beat-at-window-open` or react to `resolution` therefore
//! depend on inferring those moments from the raw stream, which is
//! brittle and out-of-scope for the live trader.
//!
//! In replay we have the entire stream up-front, so we synthesize the
//! two markers directly:
//!
//! 1. **`price_to_beat`**: emitted once per market when the first
//!    `market_meta` is observed, using the BTC tick whose `received_ns`
//!    is closest to (and not after) `window_start_ns`. If no BTC tick
//!    has been observed yet, the synthetic event is queued and emitted
//!    on the first BTC tick that lands at or after window start.
//! 2. **`resolution`**: emitted once per market when virtual time
//!    crosses `window_end_ns`. Winning outcome derives from the most
//!    recent BTC tick relative to `strike`. The fallback chain
//!    (`live emit -> market_meta resolved field -> synthesizer onchain`)
//!    is documented in `SYNTHESIZER.md`; today we only implement the
//!    last hop because the prior two do not exist in the v=1 stream.
//!
//! Determinism: synthetic events flow through the runner's event loop
//! exactly like real ones. Their `received_ns` is keyed off the upstream
//! `market_meta` / window-end timestamp, so two replays produce
//! byte-identical synthetic events.

use std::collections::BTreeMap;

use serde_json::json;

use crate::collector::schema::{Event, EventType, Source};

/// Per-market synthesizer state. We use `BTreeMap` so iteration is
/// deterministic in tests that walk multiple markets.
#[derive(Debug, Clone)]
struct MarketState {
    market_type: String,
    market_slug: String,
    yes_asset_id: Option<String>,
    no_asset_id: Option<String>,
    strike: Option<f64>,
    window_start_ns: i64,
    window_end_ns: i64,
    /// Whether we have already emitted `price_to_beat` for this window.
    emitted_price_to_beat: bool,
    /// Whether we have already emitted `resolution` for this window.
    emitted_resolution: bool,
    /// Pending `price_to_beat` waiting on a BTC tick (no oracle sample
    /// observed at `market_meta` time).
    price_to_beat_pending: bool,
}

/// Synthesizes `price_to_beat` and `resolution` events from a v=1 event
/// stream. Wire into `replay::runner` so synthetic events are dispatched
/// to the strategy adapter alongside real ones.
#[derive(Debug, Default)]
pub struct EventSynthesizer {
    /// Tracked markets, keyed by `market_slug` (the canonical key for a
    /// single binary-outcome market on Polymarket).
    markets: BTreeMap<String, MarketState>,
    /// Most recently observed BTC tick. `(received_ns, price_usd)`.
    last_btc_tick: Option<(i64, f64)>,
}

impl EventSynthesizer {
    pub fn new() -> Self {
        Self::default()
    }

    /// Process `event` and return any synthetic events that should be
    /// injected into the stream BEFORE `event` is dispatched. Synthetic
    /// events are sorted by `received_ns` so the runner sees a strictly
    /// monotonic stream.
    pub fn on_event(&mut self, event: &Event) -> Vec<Event> {
        let mut out: Vec<Event> = Vec::new();

        match event.event_type {
            EventType::MarketMeta => {
                self.handle_market_meta(event, &mut out);
            }
            EventType::BtcTick => {
                self.handle_btc_tick(event, &mut out);
            }
            _ => {}
        }

        // After updating state, walk all tracked markets and emit any
        // resolutions whose window has closed by `event.received_ns`.
        // Done here (not just on btc_tick) so a long quiescent stretch
        // still resolves once any later event arrives.
        self.emit_due_resolutions(event.received_ns, &mut out);

        out.sort_by(|a, b| {
            a.received_ns
                .cmp(&b.received_ns)
                .then_with(|| event_type_order(a.event_type).cmp(&event_type_order(b.event_type)))
        });
        out
    }

    /// Emit any synthetic events due by the supplied replay timestamp.
    /// The runner calls this at the end of a window so settlement does not
    /// depend on an unrelated later market/reference event arriving after
    /// the market close.
    pub fn flush_due(&mut self, now_ns: i64) -> Vec<Event> {
        let mut out = Vec::new();
        self.emit_due_resolutions(now_ns, &mut out);
        out.sort_by(|a, b| {
            a.received_ns
                .cmp(&b.received_ns)
                .then_with(|| event_type_order(a.event_type).cmp(&event_type_order(b.event_type)))
        });
        out
    }

    fn handle_market_meta(&mut self, event: &Event, out: &mut Vec<Event>) {
        let Some(slug) = event.market_slug.as_deref() else {
            return;
        };
        let raw = &event.raw;
        let asset_ids = raw
            .get("asset_ids")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();
        let yes_id = asset_ids
            .first()
            .and_then(|v| v.as_str())
            .map(str::to_string);
        let no_id = asset_ids
            .get(1)
            .and_then(|v| v.as_str())
            .map(str::to_string);
        let strike = raw.get("strike").and_then(|v| v.as_f64());
        let end_time_ms = raw.get("end_time_ms").and_then(|v| v.as_i64());
        let Some(end_time_ms) = end_time_ms else {
            return;
        };
        let window_ms = window_ms_for_market_type(&event.market_type);
        let window_end_ns = end_time_ms.saturating_mul(1_000_000);
        let window_start_ns = window_end_ns - (window_ms as i64) * 1_000_000;

        // Same-window idempotency: if the market is already tracked for
        // this exact window, do nothing. A second market_meta for the
        // same window is the live runtime's "context refresh" emission;
        // it must not trigger a duplicate price_to_beat.
        if let Some(existing) = self.markets.get(slug) {
            if existing.window_end_ns == window_end_ns
                && existing.window_start_ns == window_start_ns
            {
                return;
            }
        }

        let mut state = MarketState {
            market_type: event.market_type.clone(),
            market_slug: slug.to_string(),
            yes_asset_id: yes_id,
            no_asset_id: no_id,
            strike,
            window_start_ns,
            window_end_ns,
            emitted_price_to_beat: false,
            emitted_resolution: false,
            price_to_beat_pending: true,
        };

        // Try to emit price_to_beat right now if we already have a BTC
        // tick. Otherwise defer until the next btc_tick lands.
        if let Some(price_event) = self.try_emit_price_to_beat(&mut state, event.received_ns) {
            out.push(price_event);
        }

        self.markets.insert(slug.to_string(), state);
    }

    fn handle_btc_tick(&mut self, event: &Event, out: &mut Vec<Event>) {
        let Some(price) = event
            .price
            .as_deref()
            .and_then(|s| s.parse::<f64>().ok())
            .filter(|p| p.is_finite() && *p > 0.0)
        else {
            return;
        };
        self.last_btc_tick = Some((event.received_ns, price));

        // Drain any pending price_to_beat for markets that were waiting
        // on the first BTC sample.
        let slugs: Vec<String> = self.markets.keys().cloned().collect();
        for slug in slugs {
            let mut state = self
                .markets
                .remove(&slug)
                .expect("slug came from keys iteration");
            if let Some(price_event) = self.try_emit_price_to_beat(&mut state, event.received_ns) {
                out.push(price_event);
            }
            self.markets.insert(slug, state);
        }
    }

    fn try_emit_price_to_beat(&self, state: &mut MarketState, now_ns: i64) -> Option<Event> {
        if !state.price_to_beat_pending || state.emitted_price_to_beat {
            return None;
        }
        let (tick_ns, tick_price) = self.last_btc_tick?;
        // Use the tick if (a) we are at or past window_start_ns and have
        // any tick at all, or (b) the tick predates window_start by some
        // amount (we accept it as the best available oracle sample).
        let emit_at_ns = state.window_start_ns.max(now_ns).max(tick_ns);
        // Persist the BTC tick price as the de facto strike when the
        // market metadata did not carry one. btc-updown-5m markets do
        // not declare a fixed strike; the strike IS the BTC price at
        // window open, and emit_due_resolutions later needs it to pick
        // a winner. Without this state.strike stays None and every
        // window resolves with no winner_asset_id, so stranded
        // inventory cannot be marked to its realised outcome.
        if state.strike.is_none() && tick_price.is_finite() && tick_price > 0.0 {
            state.strike = Some(tick_price);
        }
        let event = build_price_to_beat_event(state, tick_ns, tick_price, emit_at_ns);
        state.emitted_price_to_beat = true;
        state.price_to_beat_pending = false;
        Some(event)
    }

    fn emit_due_resolutions(&mut self, now_ns: i64, out: &mut Vec<Event>) {
        for state in self.markets.values_mut() {
            if state.emitted_resolution {
                continue;
            }
            if now_ns < state.window_end_ns {
                continue;
            }
            let Some(strike) = state.strike else {
                // No strike means we cannot derive a winner. The
                // documented fallback chain is exhausted; suppress the
                // synthetic resolution rather than emit a malformed one.
                state.emitted_resolution = true;
                continue;
            };
            let Some((_, last_price)) = self.last_btc_tick else {
                state.emitted_resolution = true;
                continue;
            };
            let event = build_resolution_event(state, strike, last_price);
            state.emitted_resolution = true;
            out.push(event);
        }
    }
}

fn build_price_to_beat_event(
    state: &MarketState,
    tick_ns: i64,
    tick_price: f64,
    received_ns: i64,
) -> Event {
    let (outcome, asset_id) = match state.strike {
        Some(strike) if tick_price >= strike => ("Up", state.yes_asset_id.clone()),
        Some(_) => ("Down", state.no_asset_id.clone()),
        None => ("Up", state.yes_asset_id.clone()),
    };
    let raw = json!({
        "market_id": state.market_slug,
        "asset_id": asset_id,
        "outcome": outcome,
        "oracle_source": "binance_btcusdt",
        "btc_price_at_window_open_usd": format!("{:.2}", tick_price),
        "window_start_ts_ns": state.window_start_ns,
        "window_end_ts_ns": state.window_end_ns,
        "btc_tick_observed_ns": tick_ns,
        "strike": state.strike,
    });
    Event {
        v: 1,
        ts_ns: received_ns,
        received_ns,
        event_type: EventType::PriceToBeat,
        market_type: state.market_type.clone(),
        market_slug: Some(state.market_slug.clone()),
        asset_id,
        side: None,
        price: Some(format!("{:.2}", tick_price)),
        size: None,
        sequence: None,
        source: Source::Synthesizer,
        raw,
    }
}

fn build_resolution_event(state: &MarketState, strike: f64, last_price: f64) -> Event {
    let (winning_outcome, winning_asset_id) = if last_price >= strike {
        ("Up", state.yes_asset_id.clone())
    } else {
        ("Down", state.no_asset_id.clone())
    };
    let raw = json!({
        "market_id": state.market_slug,
        "winning_outcome": winning_outcome,
        "winning_asset_id": winning_asset_id,
        "oracle_btc_price_at_close_usd": format!("{:.2}", last_price),
        "window_end_ts_ns": state.window_end_ns,
        "resolution_source": "derived_from_market_strike_and_btc_close",
    });
    Event {
        v: 1,
        ts_ns: state.window_end_ns,
        received_ns: state.window_end_ns,
        event_type: EventType::Resolution,
        market_type: state.market_type.clone(),
        market_slug: Some(state.market_slug.clone()),
        asset_id: winning_asset_id,
        side: None,
        price: Some(format!("{:.2}", last_price)),
        size: None,
        sequence: None,
        source: Source::Synthesizer,
        raw,
    }
}

fn window_ms_for_market_type(market_type: &str) -> u64 {
    match market_type {
        "btc_5m" | "eth_5m" => 5 * 60 * 1_000,
        "btc_15m" | "eth_15m" => 15 * 60 * 1_000,
        // Default: 5 minutes. Matches the live trader's default tenor.
        _ => 5 * 60 * 1_000,
    }
}

/// Stable ordering used to break ties when two synthetic events share
/// `received_ns` (e.g. price_to_beat at window-open + a market_meta).
/// Keep MarketMeta first, then PriceToBeat, then Resolution.
fn event_type_order(et: EventType) -> u8 {
    match et {
        EventType::MarketMeta => 0,
        EventType::PriceToBeat => 1,
        EventType::Resolution => 2,
        _ => 3,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const SLUG: &str = "btc-5m-test";
    const YES: &str = "0xyes";
    const NO: &str = "0xno";
    const STRIKE: f64 = 60_000.0;
    const WINDOW_END_MS: i64 = 1_714_579_500_000;
    const WINDOW_END_NS: i64 = WINDOW_END_MS * 1_000_000;
    const WINDOW_START_NS: i64 = WINDOW_END_NS - 300 * 1_000_000_000;

    fn market_meta_event(received_ns: i64, strike: Option<f64>) -> Event {
        let mut raw = json!({
            "slug": SLUG,
            "market_type": "btc_5m",
            "asset_ids": [YES, NO],
            "end_time_ms": WINDOW_END_MS,
        });
        if let Some(s) = strike {
            raw["strike"] = json!(s);
        }
        Event {
            v: 1,
            ts_ns: received_ns,
            received_ns,
            event_type: EventType::MarketMeta,
            market_type: "btc_5m".into(),
            market_slug: Some(SLUG.into()),
            asset_id: Some(YES.into()),
            side: None,
            price: None,
            size: None,
            sequence: None,
            source: Source::PolymarketDataApi,
            raw,
        }
    }

    fn btc_tick_event(received_ns: i64, price: f64) -> Event {
        Event {
            v: 1,
            ts_ns: received_ns,
            received_ns,
            event_type: EventType::BtcTick,
            market_type: "_global".into(),
            market_slug: None,
            asset_id: None,
            side: None,
            price: Some(format!("{:.2}", price)),
            size: Some("1".into()),
            sequence: None,
            source: Source::BinanceAggtrade,
            raw: json!({}),
        }
    }

    fn trade_event(received_ns: i64) -> Event {
        Event {
            v: 1,
            ts_ns: received_ns,
            received_ns,
            event_type: EventType::Trade,
            market_type: "btc_5m".into(),
            market_slug: Some(SLUG.into()),
            asset_id: Some(YES.into()),
            side: Some("buy".into()),
            price: Some("0.55".into()),
            size: Some("1".into()),
            sequence: None,
            source: Source::PolymarketMarketWs,
            raw: json!({}),
        }
    }

    #[test]
    fn emits_price_to_beat_on_first_market_meta() {
        let mut s = EventSynthesizer::new();
        // BTC tick arrives BEFORE market_meta so the synthesizer can emit
        // immediately when the meta lands.
        let synth = s.on_event(&btc_tick_event(WINDOW_START_NS - 1, 60_010.0));
        assert!(synth.is_empty());
        let synth = s.on_event(&market_meta_event(WINDOW_START_NS, Some(STRIKE)));
        assert_eq!(synth.len(), 1);
        let p = &synth[0];
        assert_eq!(p.event_type, EventType::PriceToBeat);
        assert_eq!(p.source, Source::Synthesizer);
        assert_eq!(p.market_slug.as_deref(), Some(SLUG));
        let btc_str = p
            .raw
            .get("btc_price_at_window_open_usd")
            .and_then(|v| v.as_str())
            .unwrap();
        assert_eq!(btc_str, "60010.00");
        assert_eq!(p.raw.get("outcome").and_then(|v| v.as_str()), Some("Up"));
    }

    #[test]
    fn defers_price_to_beat_until_first_btc_tick() {
        let mut s = EventSynthesizer::new();
        let synth = s.on_event(&market_meta_event(WINDOW_START_NS, Some(STRIKE)));
        // No BTC tick yet → no price_to_beat.
        assert!(synth.is_empty());
        let synth = s.on_event(&btc_tick_event(WINDOW_START_NS + 1_000_000, 59_990.0));
        assert_eq!(synth.len(), 1);
        assert_eq!(synth[0].event_type, EventType::PriceToBeat);
        assert_eq!(
            synth[0].raw.get("outcome").and_then(|v| v.as_str()),
            Some("Down")
        );
    }

    #[test]
    fn emits_resolution_after_window_end() {
        let mut s = EventSynthesizer::new();
        // Establish state: market + early BTC tick yields price_to_beat.
        let _ = s.on_event(&btc_tick_event(WINDOW_START_NS - 1, 60_010.0));
        let _ = s.on_event(&market_meta_event(WINDOW_START_NS, Some(STRIKE)));
        // BTC closes BELOW strike late in the window.
        let _ = s.on_event(&btc_tick_event(WINDOW_END_NS - 1, 59_500.0));
        // Any event at or after window_end_ns triggers resolution.
        let synth = s.on_event(&trade_event(WINDOW_END_NS + 1_000_000));
        let res: Vec<_> = synth
            .iter()
            .filter(|e| e.event_type == EventType::Resolution)
            .collect();
        assert_eq!(res.len(), 1);
        let r = res[0];
        assert_eq!(r.source, Source::Synthesizer);
        assert_eq!(
            r.raw.get("winning_outcome").and_then(|v| v.as_str()),
            Some("Down")
        );
        assert_eq!(
            r.raw.get("resolution_source").and_then(|v| v.as_str()),
            Some("derived_from_market_strike_and_btc_close")
        );
        assert_eq!(r.received_ns, WINDOW_END_NS);
    }

    #[test]
    fn does_not_double_emit() {
        let mut s = EventSynthesizer::new();
        let _ = s.on_event(&btc_tick_event(WINDOW_START_NS - 1, 60_010.0));
        let first = s.on_event(&market_meta_event(WINDOW_START_NS, Some(STRIKE)));
        assert_eq!(first.len(), 1);
        // Second market_meta for the same window: must not re-emit
        // price_to_beat.
        let second = s.on_event(&market_meta_event(WINDOW_START_NS + 1, Some(STRIKE)));
        assert!(
            second.is_empty(),
            "expected no synthetic events on duplicate market_meta, got {second:?}"
        );

        // Crossing window_end emits resolution exactly once.
        let _ = s.on_event(&btc_tick_event(WINDOW_END_NS - 1, 60_100.0));
        let r1 = s.on_event(&trade_event(WINDOW_END_NS + 1));
        let r2 = s.on_event(&trade_event(WINDOW_END_NS + 2));
        assert_eq!(
            r1.iter()
                .filter(|e| e.event_type == EventType::Resolution)
                .count(),
            1
        );
        assert!(r2.is_empty());
    }

    #[test]
    fn falls_back_to_synthesizer_market_meta_when_resolution_missing() {
        // The v=1 stream has no "live resolution" emit; the synthesizer
        // falls back to deriving the winner from the most recent BTC
        // tick + the strike from market_meta. That fallback path is the
        // default tested elsewhere; this test verifies the
        // `resolution_source` tag matches the expected fallback label.
        let mut s = EventSynthesizer::new();
        let _ = s.on_event(&btc_tick_event(WINDOW_START_NS - 1, 60_010.0));
        let _ = s.on_event(&market_meta_event(WINDOW_START_NS, Some(STRIKE)));
        let _ = s.on_event(&btc_tick_event(WINDOW_END_NS - 1, 60_500.0));
        let synth = s.on_event(&trade_event(WINDOW_END_NS + 1));
        let r = synth
            .iter()
            .find(|e| e.event_type == EventType::Resolution)
            .expect("resolution event present");
        assert_eq!(
            r.raw.get("resolution_source").and_then(|v| v.as_str()),
            Some("derived_from_market_strike_and_btc_close")
        );
        assert_eq!(
            r.raw.get("winning_outcome").and_then(|v| v.as_str()),
            Some("Up")
        );
    }

    #[test]
    fn no_strike_skips_resolution_silently() {
        let mut s = EventSynthesizer::new();
        let _ = s.on_event(&btc_tick_event(WINDOW_START_NS - 1, 60_010.0));
        let _ = s.on_event(&market_meta_event(WINDOW_START_NS, None));
        let _ = s.on_event(&btc_tick_event(WINDOW_END_NS - 1, 59_500.0));
        let synth = s.on_event(&trade_event(WINDOW_END_NS + 1));
        let res: Vec<_> = synth
            .iter()
            .filter(|e| e.event_type == EventType::Resolution)
            .collect();
        assert!(res.is_empty(), "no strike → no synthetic resolution");
    }
}
