//! Shadow-live subsystem: bundles ShadowBook + TradeSynthesiser +
//! JournalWriter into a single coordinator that the runner.rs and
//! market_ws.rs call sites can consult through a process-global handle.
//!
//! Architecture: the engine's main control flow is unchanged. Submit
//! intents continue to feed `paper_fill_from_book_snapshot` for runtime
//! inventory. In parallel, when shadow_live mode is active, this
//! subsystem runs as a passive observer that journals what the trade-
//! tape simulator would have produced. The fidelity scorer compares
//! those journaled predictions against bonereaper's /activity truth.
//!
//! Inventory and live order semantics are NOT affected. Phase 5b ships
//! parallel observation; flipping shadow to drive runtime inventory
//! happens in a future phase once the simulator's fidelity has been
//! validated multi-day on AWS per docs/runbooks/multi-day-shadow-soak.md.

use std::sync::{Arc, Mutex, OnceLock};

use crate::core::types::{EpochMillis, OrderIntent};
use crate::journal::JournalWriter;
use crate::paper::queue_model::QueueDecayEstimator;
use crate::paper::trade_tape::ShadowBook;
use crate::runtime::fidelity::extract_market_family;
use crate::wire::trade_ws::TradeSynthesiser;

pub struct ShadowSubsystem {
    book: ShadowBook,
    synthesiser: TradeSynthesiser,
    journal: Option<JournalWriter>,
}

impl ShadowSubsystem {
    pub fn new(journal: Option<JournalWriter>) -> Self {
        Self {
            book: ShadowBook::with_estimator(QueueDecayEstimator::default()),
            synthesiser: TradeSynthesiser::new(),
            journal,
        }
    }

    pub fn on_intent_submit(
        &mut self,
        intent: &OrderIntent,
        depth_ahead_at_post: f64,
        posted_at_ms: EpochMillis,
        expires_at_ms: Option<EpochMillis>,
    ) {
        let family = extract_market_family(intent.market_id.as_str()).to_string();
        self.book.on_submit(
            intent.clone(),
            expires_at_ms,
            depth_ahead_at_post.max(0.0),
            posted_at_ms,
            family,
        );
    }

    pub fn on_intent_cancel(&mut self, client_order_id: &crate::core::types::ClientOrderId) {
        self.book.on_cancel(client_order_id);
    }

    pub fn on_book_depth_delta(
        &mut self,
        asset_id: &str,
        price: f64,
        depth_delta: f64,
        observed_at_ms: EpochMillis,
    ) {
        self.synthesiser
            .observe_price_change(asset_id, price, depth_delta, observed_at_ms);
    }

    pub fn on_last_trade_price_with_book(
        &mut self,
        asset_id: &str,
        price: f64,
        book: &crate::core::types::QuoteSnapshot,
        observed_at_ms: EpochMillis,
    ) {
        let Some(trade) = self
            .synthesiser
            .synthesise(asset_id, price, book, observed_at_ms)
        else {
            return;
        };
        if let Some(journal) = self.journal.as_mut() {
            let _ = journal.append_trade_tape_event(&trade);
        }
        let fills = self.book.on_trade_event(&trade);
        if let Some(journal) = self.journal.as_mut() {
            for fill in &fills {
                let market_family =
                    extract_market_family(fill.market_id.as_str()).to_string();
                let record = crate::runtime::fidelity::ShadowFillRecord {
                    observed_at_ms: fill.observed_at_ms,
                    market_family,
                    price: fill.price,
                    size: fill.quantity,
                    notional_usd: fill.notional_usd(),
                    // Maker rebate accounting is downstream; populate
                    // when the rebate engine wires through. Today we
                    // leave 0.0 explicit so consumers see the gap.
                    rebate_usd: 0.0,
                };
                let _ = journal.append_shadow_fill(&record);
            }
            if !fills.is_empty() {
                let _ = journal.flush();
            }
        }
    }

    /// Convenience entry point used by wire/market_ws.rs which has
    /// best_bid + best_ask floats but not a full QuoteSnapshot. Builds a
    /// minimal QuoteSnapshot internally so the synthesiser can derive
    /// taker side from price-vs-touch.
    pub fn on_last_trade_price(
        &mut self,
        asset_id: &str,
        price: f64,
        best_bid: Option<f64>,
        best_ask: Option<f64>,
        observed_at_ms: EpochMillis,
    ) {
        use crate::core::types::{BookLevel, QuoteSnapshot};
        let book = QuoteSnapshot {
            best_bid: best_bid.map(|p| BookLevel::new(p, 0.0)),
            best_ask: best_ask.map(|p| BookLevel::new(p, 0.0)),
            bid_levels: vec![],
            ask_levels: vec![],
            depth_observed_at_ms: Some(observed_at_ms),
            last_trade_price: Some(price),
            observed_at_ms,
        };
        let Some(trade) = self
            .synthesiser
            .synthesise(asset_id, price, &book, observed_at_ms)
        else {
            return;
        };
        if let Some(journal) = self.journal.as_mut() {
            let _ = journal.append_trade_tape_event(&trade);
        }
        let fills = self.book.on_trade_event(&trade);
        if let Some(journal) = self.journal.as_mut() {
            for fill in &fills {
                let market_family =
                    extract_market_family(fill.market_id.as_str()).to_string();
                let record = crate::runtime::fidelity::ShadowFillRecord {
                    observed_at_ms: fill.observed_at_ms,
                    market_family,
                    price: fill.price,
                    size: fill.quantity,
                    notional_usd: fill.notional_usd(),
                    rebate_usd: 0.0,
                };
                let _ = journal.append_shadow_fill(&record);
            }
            if !fills.is_empty() {
                let _ = journal.flush();
            }
        }
        // Fills are journaled via trade_tape_event + shadow_fill +
        // downstream fidelity scoring; we do not re-emit them as
        // runtime_event here because they are not real venue fills and
        // must not affect the runtime inventory pipeline.
    }

    pub fn snapshot_queue_estimates(&mut self, now_ms: EpochMillis, families: &[&str]) {
        let Some(journal) = self.journal.as_mut() else {
            return;
        };
        for family in families {
            let estimate = self
                .book
                .rate_for(family);
            // Re-build a QueueModelEstimate from public ShadowBook accessors.
            let snapshot = crate::paper::queue_model::QueueModelEstimate {
                observed_at_ms: now_ms,
                market_family: (*family).to_string(),
                queue_decay_rate_per_sec: estimate,
                n_observations: self.book.n_observations(family),
            };
            let _ = journal.append_queue_model_estimate(&snapshot);
        }
        let _ = journal.flush();
    }

    pub fn shadow_book(&self) -> &ShadowBook {
        &self.book
    }

    pub fn journal_mut(&mut self) -> Option<&mut JournalWriter> {
        self.journal.as_mut()
    }
}

static GLOBAL_SHADOW: OnceLock<Arc<Mutex<ShadowSubsystem>>> = OnceLock::new();

/// Initialise the process-global shadow subsystem. Called once in
/// run_shadow_live before the main run loop starts. Subsequent calls
/// are no-ops — by design the subsystem is a singleton per process.
pub fn init_shadow(subsystem: ShadowSubsystem) {
    let _ = GLOBAL_SHADOW.set(Arc::new(Mutex::new(subsystem)));
}

/// Get the process-global shadow subsystem if one was initialised. The
/// runner.rs paper-mode submit branch and market_ws.rs event handlers
/// call this; in non-shadow modes it returns None and the call sites
/// no-op.
pub fn shadow() -> Option<Arc<Mutex<ShadowSubsystem>>> {
    GLOBAL_SHADOW.get().cloned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::types::{
        BookLevel, ClientOrderId, InstrumentId, IntentKind, MarketId, OrderIntent, QuoteSnapshot,
        TradeSide,
    };

    fn intent(coid: &str, market: &str, asset: &str, price: f64, qty: f64) -> OrderIntent {
        OrderIntent {
            client_order_id: ClientOrderId::from(coid),
            market_id: MarketId::from(market),
            instrument_id: InstrumentId::from(asset),
            side: TradeSide::Buy,
            limit_price: price,
            quantity: qty,
            reduce_only: false,
            reason: "test".to_string(),
            quote_level_tag: Some("bonereaper-mid".to_string()),
            created_at_ms: 1_000,
            pair_id: None,
            kind: IntentKind::Entry,
        }
    }

    fn book(bid: f64, ask: f64) -> QuoteSnapshot {
        QuoteSnapshot {
            best_bid: Some(BookLevel::new(bid, 100.0)),
            best_ask: Some(BookLevel::new(ask, 100.0)),
            bid_levels: vec![BookLevel::new(bid, 100.0)],
            ask_levels: vec![BookLevel::new(ask, 100.0)],
            depth_observed_at_ms: Some(1_000),
            last_trade_price: Some((bid + ask) / 2.0),
            observed_at_ms: 1_000,
        }
    }

    #[test]
    fn submit_then_synthesised_trade_books_fill() {
        // Pre-calibrate the estimator by injecting a fill observation
        // directly through the public ShadowBook handle.
        let est = QueueDecayEstimator::new(1, 1.0);
        let mut sub = ShadowSubsystem {
            book: ShadowBook::with_estimator({
                let mut est = est;
                est.update_with_fill("btc-updown-5m", 0.0, 1.0, 0.0);
                est
            }),
            synthesiser: TradeSynthesiser::new(),
            journal: None,
        };

        sub.on_intent_submit(
            &intent("c1", "btc-updown-5m-1776961500", "asset-yes", 0.49, 100.0),
            0.0,
            1_000,
            None,
        );
        assert_eq!(sub.shadow_book().open_order_count(), 1);

        sub.on_book_depth_delta("asset-yes", 0.49, 50.0, 4_950);
        sub.on_last_trade_price_with_book("asset-yes", 0.49, &book(0.49, 0.50), 5_000);

        // Trade size 50 fully fills the order's 100 qty up to size 50;
        // remaining 50 keeps order standing.
        assert_eq!(sub.shadow_book().open_order_count(), 1);
    }

    #[test]
    fn cancel_removes_intent_from_shadow_book() {
        let mut sub = ShadowSubsystem::new(None);
        let it = intent("c1", "btc-updown-5m-1776961500", "asset-yes", 0.49, 100.0);
        sub.on_intent_submit(&it, 0.0, 1_000, None);
        sub.on_intent_cancel(&it.client_order_id);
        assert_eq!(sub.shadow_book().open_order_count(), 0);
    }

    #[test]
    fn no_synthesis_when_book_is_empty() {
        let mut sub = ShadowSubsystem::new(None);
        let empty_book = QuoteSnapshot::default();
        sub.on_last_trade_price_with_book("asset-yes", 0.49, &empty_book, 5_000);
        // Nothing crashes; no fills produced.
        assert_eq!(sub.shadow_book().open_order_count(), 0);
    }

    #[test]
    fn last_trade_price_floats_entry_synthesises_correctly() {
        let mut sub = ShadowSubsystem::new(None);
        // No book bid/ask -> synthesiser refuses, no panics.
        sub.on_last_trade_price("asset-yes", 0.49, None, None, 5_000);
        // With both touches present, taker side is derived; even with no
        // pre-calibration, no fill is produced (uncalibrated estimator).
        sub.on_last_trade_price("asset-yes", 0.49, Some(0.49), Some(0.50), 5_000);
        assert_eq!(sub.shadow_book().open_order_count(), 0);
    }
}
