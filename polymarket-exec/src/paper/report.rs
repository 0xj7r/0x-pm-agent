//! Paper-mode session report card. Accumulates per-fill records and
//! per-reject counts during a paper run, writes a JSON summary on flush.
//! Phase 3 of the paper env design.

use std::collections::BTreeMap;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::Serialize;

use crate::book::BookState;
use crate::event_log::EventRecord;
use crate::types::{
    ClientOrderId, FillLiquidity, FillReport, InstrumentId, MarketId, RuntimeCommand, TradeSide,
};

/// Per-fill record captured by `PaperReportWriter`.
#[derive(Clone, Debug, Serialize)]
pub struct PaperFillRecord {
    pub observed_at_ms: u64,
    pub client_order_id: Option<ClientOrderId>,
    pub market_id: MarketId,
    pub instrument_id: InstrumentId,
    pub side: TradeSide,
    pub limit_price: f64,
    pub fill_price: f64,
    pub quantity: f64,
    pub fee_usd: f64,
    pub liquidity: FillLiquidity,
    /// Mid price observed at the moment the originating intent was sent
    /// to the paper execution adapter. Used to compute expected edge.
    pub mid_at_submit: Option<f64>,
    /// |fill_price - limit_price| / limit_price * 10_000.
    pub slippage_bps: f64,
    /// (mid_at_submit - fill_price) * quantity for buys; sign-flipped for sells.
    pub realized_edge_usd: Option<f64>,
}

/// Per-rejection record (e.g. paper post-only cross rejection).
#[derive(Clone, Debug, Serialize)]
pub struct PaperRejectRecord {
    pub observed_at_ms: u64,
    pub client_order_id: ClientOrderId,
    pub market_id: MarketId,
    pub instrument_id: InstrumentId,
    pub limit_price: f64,
    pub reason: String,
}

/// Aggregated session-level summary written on flush.
#[derive(Clone, Debug, Serialize)]
pub struct PaperReportSummary {
    pub session: SessionInfo,
    pub fills: FillStats,
    pub edge: EdgeStats,
    pub queue: QueueStats,
    pub vs_whale: VsWhaleStats,
    pub strategy: StrategySessionStats,
    pub markets: Vec<MarketSessionStats>,
    pub suppression_samples: Vec<SuppressionSample>,
    pub acceptance: AcceptanceGateStats,
}

#[derive(Clone, Debug, Serialize)]
pub struct SessionInfo {
    pub run_id: String,
    pub mode: String,
    pub started_at_ms: u64,
    pub ended_at_ms: u64,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct FillStats {
    pub total_count: usize,
    pub maker_count: usize,
    pub taker_count: usize,
    pub maker_fraction: f64,
    pub total_notional_usd: f64,
    pub total_fees_usd: f64,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct EdgeStats {
    /// Sum of (mid_at_submit - limit_price) * quantity for buys (sign-flipped
    /// for sells), counted per-submission. Zero when mid_at_submit is unknown.
    pub expected_edge_usd: f64,
    /// Sum of realized_edge_usd across fills with known mid_at_submit.
    pub realized_edge_usd: f64,
    /// realized / expected when expected != 0.
    pub edge_capture_ratio: Option<f64>,
    pub avg_slippage_bps: f64,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct QueueStats {
    pub post_only_reject_count: usize,
    pub late_fill_after_cancel_count: usize,
}

/// Comparison against an external whale-fill stream (e.g. unlawful-shear's
/// on-chain fills) over the same time window as this paper session.
/// Populated by `record_whale_fill_observed` calls; emitted in the
/// summary so operators can spot under/over-fill vs. the whale.
#[derive(Clone, Debug, Default, Serialize)]
pub struct VsWhaleStats {
    pub whale_fill_count: usize,
    pub whale_buy_count: usize,
    pub whale_sell_count: usize,
    pub whale_total_notional_usd: f64,
    pub our_total_notional_usd: f64,
    /// our_total_notional_usd / whale_total_notional_usd. None when the
    /// whale stream is empty (no comparison possible).
    pub notional_capture_ratio: Option<f64>,
    /// Our fills minus whale fills in the same direction (rough proxy for
    /// "did we do roughly what the whale did"). Larger absolute value =
    /// more divergence.
    pub directional_imbalance_count: i64,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct StrategySessionStats {
    pub event_total: usize,
    pub intent_total: usize,
    pub events_by_type: BTreeMap<String, usize>,
    pub intents_by_type: BTreeMap<String, usize>,
}

#[derive(Clone, Debug, Serialize)]
pub struct MarketSessionStats {
    pub market_id: MarketId,
    pub submit_count: usize,
    pub cancel_count: usize,
    pub reject_count: usize,
    pub fill_count: usize,
    pub maker_fill_count: usize,
    pub taker_fill_count: usize,
    pub total_notional_usd: f64,
    pub buy_qty: f64,
    pub sell_qty: f64,
    pub buy_notional_usd: f64,
    pub sell_notional_usd: f64,
    pub net_qty: f64,
    pub fill_symmetry: Option<f64>,
    pub max_stranded_qty_estimate: f64,
    pub strategy_events_by_type: BTreeMap<String, usize>,
    pub intents_by_type: BTreeMap<String, usize>,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct AcceptanceGateStats {
    pub pass: bool,
    pub no_fills_observed: bool,
    pub excessive_cancel_churn: bool,
    pub poor_fill_symmetry: bool,
    pub stranded_exposure: bool,
    pub notes: Vec<String>,
}

#[derive(Clone, Debug, Serialize)]
pub struct SuppressionSample {
    pub observed_at_ms: u64,
    pub market_id: MarketId,
    pub reason_type: String,
    pub reason: String,
    pub books_at_suppress: Vec<BookMark>,
    pub books_after_60s: Vec<BookMark>,
    pub books_after_5m: Vec<BookMark>,
    pub avg_mid_move_60s: Option<f64>,
    pub avg_mid_move_5m: Option<f64>,
}

#[derive(Clone, Debug, Serialize)]
pub struct BookMark {
    pub instrument_id: InstrumentId,
    pub observed_at_ms: u64,
    pub best_bid: f64,
    pub best_ask: f64,
    pub mid: Option<f64>,
}

#[derive(Clone, Debug, Default)]
struct MarketAccumulator {
    submit_count: usize,
    cancel_count: usize,
    reject_count: usize,
    fill_count: usize,
    maker_fill_count: usize,
    taker_fill_count: usize,
    total_notional_usd: f64,
    buy_qty: f64,
    sell_qty: f64,
    buy_notional_usd: f64,
    sell_notional_usd: f64,
    instrument_qty: BTreeMap<InstrumentId, f64>,
    strategy_events_by_type: BTreeMap<String, usize>,
    intents_by_type: BTreeMap<String, usize>,
}

#[derive(Clone, Debug, Default)]
struct SuppressionAccumulator {
    observed_at_ms: u64,
    market_id: MarketId,
    reason_type: String,
    reason: String,
    books_at_suppress: Vec<BookMark>,
    books_after_60s: Option<Vec<BookMark>>,
    books_after_5m: Option<Vec<BookMark>>,
}

/// Accumulator + writer. Build it at run start; record events as they
/// happen; call `flush()` on shutdown to persist a JSON summary.
pub struct PaperReportWriter {
    run_id: String,
    mode: String,
    output_path: PathBuf,
    started_at_ms: u64,
    last_observed_at_ms: u64,
    fills: Vec<PaperFillRecord>,
    rejects: Vec<PaperRejectRecord>,
    expected_edge_usd: f64,
    late_fill_after_cancel_count: usize,
    strategy_events_by_type: BTreeMap<String, usize>,
    strategy_intents_by_type: BTreeMap<String, usize>,
    markets: BTreeMap<MarketId, MarketAccumulator>,
    latest_books_by_market: BTreeMap<MarketId, BTreeMap<InstrumentId, BookMark>>,
    suppression_samples: Vec<SuppressionAccumulator>,
    whale_fill_count: usize,
    whale_buy_count: usize,
    whale_sell_count: usize,
    whale_total_notional_usd: f64,
}

impl PaperReportWriter {
    pub fn new(
        run_id: impl Into<String>,
        mode: impl Into<String>,
        output_path: PathBuf,
        started_at_ms: u64,
    ) -> Self {
        Self {
            run_id: run_id.into(),
            mode: mode.into(),
            output_path,
            started_at_ms,
            last_observed_at_ms: started_at_ms,
            fills: Vec::new(),
            rejects: Vec::new(),
            expected_edge_usd: 0.0,
            late_fill_after_cancel_count: 0,
            strategy_events_by_type: BTreeMap::new(),
            strategy_intents_by_type: BTreeMap::new(),
            markets: BTreeMap::new(),
            latest_books_by_market: BTreeMap::new(),
            suppression_samples: Vec::new(),
            whale_fill_count: 0,
            whale_buy_count: 0,
            whale_sell_count: 0,
            whale_total_notional_usd: 0.0,
        }
    }

    pub fn record_book_observation(
        &mut self,
        market_id: &MarketId,
        instrument_id: &InstrumentId,
        book: &BookState,
    ) {
        let observed_at_ms = book.last_update_unix_ms;
        if observed_at_ms == 0 {
            return;
        }
        self.last_observed_at_ms = self.last_observed_at_ms.max(observed_at_ms);
        let mark = BookMark {
            instrument_id: instrument_id.clone(),
            observed_at_ms,
            best_bid: book.best_bid,
            best_ask: book.best_ask,
            mid: midpoint(book.best_bid, book.best_ask),
        };
        self.latest_books_by_market
            .entry(market_id.clone())
            .or_default()
            .insert(instrument_id.clone(), mark);
        self.capture_future_books(market_id, observed_at_ms);
    }

    /// Record runtime events and commands so the paper report can explain
    /// zero-fill sessions, not just summarize sessions that already filled.
    pub fn record_runtime_outcome(
        &mut self,
        records: &[EventRecord],
        commands: &[RuntimeCommand],
        observed_at_ms: u64,
    ) {
        self.last_observed_at_ms = self.last_observed_at_ms.max(observed_at_ms);
        for record in records {
            if let Some(event) = classify_runtime_event(record.message.as_str()) {
                increment(&mut self.strategy_events_by_type, event);
                if let Some(market_id) = &record.market_id {
                    increment(
                        &mut self
                            .markets
                            .entry(market_id.clone())
                            .or_default()
                            .strategy_events_by_type,
                        event,
                    );
                    self.record_suppression_sample(market_id, record, event);
                }
            }
            if record.message.contains("requested order cancellation") {
                if let Some(market_id) = &record.market_id {
                    self.markets
                        .entry(market_id.clone())
                        .or_default()
                        .cancel_count += 1;
                }
            }
        }
        for command in commands {
            match command {
                RuntimeCommand::Submit(intent) => {
                    let intent_type = classify_submit_intent(
                        intent.quote_level_tag.as_deref().unwrap_or_default(),
                    );
                    increment(&mut self.strategy_intents_by_type, intent_type);
                    let market = self.markets.entry(intent.market_id.clone()).or_default();
                    market.submit_count += 1;
                    increment(&mut market.intents_by_type, intent_type);
                }
                RuntimeCommand::Cancel {
                    client_order_id: _,
                    reason: _,
                } => {
                    increment(&mut self.strategy_intents_by_type, "cancel");
                    // Cancel commands do not currently carry market_id; keep
                    // the session-level count exact and per-market count from
                    // request-cancel EventRecords when available.
                }
                _ => {}
            }
        }
    }

    fn record_suppression_sample(
        &mut self,
        market_id: &MarketId,
        record: &EventRecord,
        event: &str,
    ) {
        if !matches!(
            event,
            "market_mid_trend_gate" | "premium_fair_gate" | "btc_flat_gate" | "btc_trend_gate"
        ) {
            return;
        }
        const MAX_SUPPRESSION_SAMPLES: usize = 1_000;
        if self.suppression_samples.len() >= MAX_SUPPRESSION_SAMPLES {
            return;
        }
        let books_at_suppress = self.latest_market_books(market_id);
        self.suppression_samples.push(SuppressionAccumulator {
            observed_at_ms: record.observed_at_ms,
            market_id: market_id.clone(),
            reason_type: event.to_string(),
            reason: record.message.clone(),
            books_at_suppress,
            books_after_60s: None,
            books_after_5m: None,
        });
    }

    fn capture_future_books(&mut self, market_id: &MarketId, now_ms: u64) {
        let latest_books = self.latest_market_books(market_id);
        if latest_books.is_empty() {
            return;
        }
        for sample in &mut self.suppression_samples {
            if &sample.market_id != market_id {
                continue;
            }
            let elapsed_ms = now_ms.saturating_sub(sample.observed_at_ms);
            let expected_book_count = sample.books_at_suppress.len().max(1);
            if sample.books_after_60s.is_none() && elapsed_ms >= 60_000 {
                let target_ms = sample.observed_at_ms.saturating_add(60_000);
                if market_books_are_fresh(&latest_books, target_ms, expected_book_count) {
                    sample.books_after_60s = Some(latest_books.clone());
                }
            }
            if sample.books_after_5m.is_none() && elapsed_ms >= 5 * 60_000 {
                let target_ms = sample.observed_at_ms.saturating_add(5 * 60_000);
                if market_books_are_fresh(&latest_books, target_ms, expected_book_count) {
                    sample.books_after_5m = Some(latest_books.clone());
                }
            }
        }
    }

    fn latest_market_books(&self, market_id: &MarketId) -> Vec<BookMark> {
        self.latest_books_by_market
            .get(market_id)
            .map(|books| books.values().cloned().collect())
            .unwrap_or_default()
    }

    /// Record a whale fill observed in the same session window. Used by
    /// the runtime to feed the `vs_whale` section so the report shows
    /// "we'd have filled X notional; whale filled Y" side-by-side. Side
    /// is matched on a best-effort basis from the whale event payload
    /// ("buy"/"sell" case-insensitive).
    pub fn record_whale_fill_observed(
        &mut self,
        observed_at_ms: u64,
        side: Option<&str>,
        notional_usd: f64,
    ) {
        self.last_observed_at_ms = self.last_observed_at_ms.max(observed_at_ms);
        self.whale_fill_count += 1;
        match side.map(|s| s.to_ascii_lowercase()) {
            Some(s) if s == "buy" => self.whale_buy_count += 1,
            Some(s) if s == "sell" => self.whale_sell_count += 1,
            _ => {}
        }
        if notional_usd > 0.0 {
            self.whale_total_notional_usd += notional_usd;
        }
    }

    /// Record the expected edge of a submission relative to a known mid.
    /// Called from the submit path when the order is first sent to the
    /// paper adapter. `mid_at_submit` should be the book mid observed at
    /// that moment; `quantity` is the requested order quantity.
    pub fn record_submit_edge(
        &mut self,
        side: TradeSide,
        limit_price: f64,
        quantity: f64,
        mid_at_submit: f64,
        observed_at_ms: u64,
    ) {
        if mid_at_submit <= 0.0 || limit_price <= 0.0 || quantity <= 0.0 {
            return;
        }
        let edge_per_unit = match side {
            TradeSide::Buy => mid_at_submit - limit_price,
            TradeSide::Sell => limit_price - mid_at_submit,
        };
        self.expected_edge_usd += edge_per_unit * quantity;
        self.last_observed_at_ms = self.last_observed_at_ms.max(observed_at_ms);
    }

    /// Record a fill observed in the paper environment.
    pub fn record_fill(&mut self, fill: &FillReport, mid_at_submit: Option<f64>) {
        let limit_price = fill.price.max(0.0);
        let slippage_bps = if limit_price > 0.0 {
            (fill.price - limit_price).abs() / limit_price * 10_000.0
        } else {
            0.0
        };
        let realized_edge_usd = mid_at_submit.map(|mid| match fill.side {
            TradeSide::Buy => (mid - fill.price) * fill.quantity,
            TradeSide::Sell => (fill.price - mid) * fill.quantity,
        });
        self.last_observed_at_ms = self.last_observed_at_ms.max(fill.observed_at_ms);
        self.fills.push(PaperFillRecord {
            observed_at_ms: fill.observed_at_ms,
            client_order_id: fill.client_order_id.clone(),
            market_id: fill.market_id.clone(),
            instrument_id: fill.instrument_id.clone(),
            side: fill.side,
            limit_price,
            fill_price: fill.price,
            quantity: fill.quantity,
            fee_usd: fill.fee_usd,
            liquidity: fill.liquidity,
            mid_at_submit,
            slippage_bps,
            realized_edge_usd,
        });
        let market = self.markets.entry(fill.market_id.clone()).or_default();
        market.fill_count += 1;
        match fill.liquidity {
            FillLiquidity::Maker => market.maker_fill_count += 1,
            FillLiquidity::Taker => market.taker_fill_count += 1,
            FillLiquidity::Unknown => {}
        }
        let notional = fill.price * fill.quantity;
        market.total_notional_usd += notional;
        match fill.side {
            TradeSide::Buy => {
                market.buy_qty += fill.quantity;
                market.buy_notional_usd += notional;
                *market
                    .instrument_qty
                    .entry(fill.instrument_id.clone())
                    .or_default() += fill.quantity;
            }
            TradeSide::Sell => {
                market.sell_qty += fill.quantity;
                market.sell_notional_usd += notional;
                *market
                    .instrument_qty
                    .entry(fill.instrument_id.clone())
                    .or_default() -= fill.quantity;
            }
        }
    }

    /// Record a venue-rejection event (post-only cross, etc.).
    pub fn record_reject(
        &mut self,
        client_order_id: &ClientOrderId,
        market_id: &MarketId,
        instrument_id: &InstrumentId,
        limit_price: f64,
        reason: impl Into<String>,
        observed_at_ms: u64,
    ) {
        self.last_observed_at_ms = self.last_observed_at_ms.max(observed_at_ms);
        self.rejects.push(PaperRejectRecord {
            observed_at_ms,
            client_order_id: client_order_id.clone(),
            market_id: market_id.clone(),
            instrument_id: instrument_id.clone(),
            limit_price,
            reason: reason.into(),
        });
        self.markets
            .entry(market_id.clone())
            .or_default()
            .reject_count += 1;
    }

    /// Increment the late-fill-after-cancel counter (called when a fill is
    /// applied during the paper cancel race window).
    pub fn record_late_fill_after_cancel(&mut self) {
        self.late_fill_after_cancel_count += 1;
    }

    /// Build the aggregated summary without flushing to disk. Useful for
    /// unit tests and for pre-flush dashboard snapshots.
    pub fn summary(&self) -> PaperReportSummary {
        let mut maker = 0;
        let mut taker = 0;
        let mut total_notional = 0.0;
        let mut total_fees = 0.0;
        let mut realized_edge = 0.0;
        let mut realized_edge_seen = false;
        let mut slippage_sum = 0.0;
        for fill in &self.fills {
            match fill.liquidity {
                FillLiquidity::Maker => maker += 1,
                FillLiquidity::Taker => taker += 1,
                FillLiquidity::Unknown => {}
            }
            total_notional += fill.fill_price * fill.quantity;
            total_fees += fill.fee_usd;
            if let Some(re) = fill.realized_edge_usd {
                realized_edge += re;
                realized_edge_seen = true;
            }
            slippage_sum += fill.slippage_bps;
        }
        let total_count = self.fills.len();
        let maker_fraction = if total_count > 0 {
            maker as f64 / total_count as f64
        } else {
            0.0
        };
        let avg_slippage_bps = if total_count > 0 {
            slippage_sum / total_count as f64
        } else {
            0.0
        };
        let edge_capture_ratio = if realized_edge_seen && self.expected_edge_usd != 0.0 {
            Some(realized_edge / self.expected_edge_usd)
        } else {
            None
        };
        let markets = self.market_summaries();
        let suppression_samples = self.suppression_summaries();
        let acceptance = self.acceptance_summary(total_count, &markets);
        PaperReportSummary {
            session: SessionInfo {
                run_id: self.run_id.clone(),
                mode: self.mode.clone(),
                started_at_ms: self.started_at_ms,
                ended_at_ms: self.last_observed_at_ms,
            },
            fills: FillStats {
                total_count,
                maker_count: maker,
                taker_count: taker,
                maker_fraction,
                total_notional_usd: total_notional,
                total_fees_usd: total_fees,
            },
            edge: EdgeStats {
                expected_edge_usd: self.expected_edge_usd,
                realized_edge_usd: realized_edge,
                edge_capture_ratio,
                avg_slippage_bps,
            },
            queue: QueueStats {
                post_only_reject_count: self.rejects.len(),
                late_fill_after_cancel_count: self.late_fill_after_cancel_count,
            },
            vs_whale: {
                let our_buys = self
                    .fills
                    .iter()
                    .filter(|f| matches!(f.side, TradeSide::Buy))
                    .count() as i64;
                let our_sells = self
                    .fills
                    .iter()
                    .filter(|f| matches!(f.side, TradeSide::Sell))
                    .count() as i64;
                let whale_buys = self.whale_buy_count as i64;
                let whale_sells = self.whale_sell_count as i64;
                let directional_imbalance_count =
                    (our_buys - whale_buys).abs() + (our_sells - whale_sells).abs();
                let notional_capture_ratio = if self.whale_total_notional_usd > 0.0 {
                    Some(total_notional / self.whale_total_notional_usd)
                } else {
                    None
                };
                VsWhaleStats {
                    whale_fill_count: self.whale_fill_count,
                    whale_buy_count: self.whale_buy_count,
                    whale_sell_count: self.whale_sell_count,
                    whale_total_notional_usd: self.whale_total_notional_usd,
                    our_total_notional_usd: total_notional,
                    notional_capture_ratio,
                    directional_imbalance_count,
                }
            },
            strategy: StrategySessionStats {
                event_total: self.strategy_events_by_type.values().sum(),
                intent_total: self.strategy_intents_by_type.values().sum(),
                events_by_type: self.strategy_events_by_type.clone(),
                intents_by_type: self.strategy_intents_by_type.clone(),
            },
            markets,
            suppression_samples,
            acceptance,
        }
    }

    fn suppression_summaries(&self) -> Vec<SuppressionSample> {
        self.suppression_samples
            .iter()
            .map(|sample| {
                let books_after_60s = sample.books_after_60s.clone().unwrap_or_default();
                let books_after_5m = sample.books_after_5m.clone().unwrap_or_default();
                SuppressionSample {
                    observed_at_ms: sample.observed_at_ms,
                    market_id: sample.market_id.clone(),
                    reason_type: sample.reason_type.clone(),
                    reason: sample.reason.clone(),
                    books_at_suppress: sample.books_at_suppress.clone(),
                    avg_mid_move_60s: avg_mid_move(&sample.books_at_suppress, &books_after_60s),
                    avg_mid_move_5m: avg_mid_move(&sample.books_at_suppress, &books_after_5m),
                    books_after_60s,
                    books_after_5m,
                }
            })
            .collect()
    }

    fn market_summaries(&self) -> Vec<MarketSessionStats> {
        self.markets
            .iter()
            .map(|(market_id, acc)| {
                let mut positive_legs: Vec<f64> = acc
                    .instrument_qty
                    .values()
                    .copied()
                    .filter(|qty| *qty > 0.0)
                    .collect();
                positive_legs.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
                let max_qty = positive_legs.last().copied().unwrap_or(0.0);
                let min_qty = positive_legs.first().copied().unwrap_or(0.0);
                let fill_symmetry = if max_qty > 0.0 && positive_legs.len() >= 2 {
                    Some(min_qty / max_qty)
                } else {
                    None
                };
                let max_stranded_qty_estimate = if positive_legs.len() >= 2 {
                    max_qty - min_qty
                } else {
                    max_qty
                };
                MarketSessionStats {
                    market_id: market_id.clone(),
                    submit_count: acc.submit_count,
                    cancel_count: acc.cancel_count,
                    reject_count: acc.reject_count,
                    fill_count: acc.fill_count,
                    maker_fill_count: acc.maker_fill_count,
                    taker_fill_count: acc.taker_fill_count,
                    total_notional_usd: acc.total_notional_usd,
                    buy_qty: acc.buy_qty,
                    sell_qty: acc.sell_qty,
                    buy_notional_usd: acc.buy_notional_usd,
                    sell_notional_usd: acc.sell_notional_usd,
                    net_qty: acc.buy_qty - acc.sell_qty,
                    fill_symmetry,
                    max_stranded_qty_estimate,
                    strategy_events_by_type: acc.strategy_events_by_type.clone(),
                    intents_by_type: acc.intents_by_type.clone(),
                }
            })
            .collect()
    }

    fn acceptance_summary(
        &self,
        total_fill_count: usize,
        markets: &[MarketSessionStats],
    ) -> AcceptanceGateStats {
        let duration_ms = self.last_observed_at_ms.saturating_sub(self.started_at_ms);
        let submit_count: usize = markets.iter().map(|market| market.submit_count).sum();
        let cancel_count: usize = markets.iter().map(|market| market.cancel_count).sum();
        let no_fills_observed = duration_ms >= 60_000 && total_fill_count == 0;
        let excessive_cancel_churn =
            submit_count >= 5 && cancel_count as f64 / submit_count as f64 > 5.0;
        let poor_fill_symmetry = markets.iter().any(|market| {
            market
                .fill_symmetry
                .is_some_and(|sym| market.buy_qty + market.sell_qty >= 10.0 && sym < 0.35)
        });
        let stranded_exposure = markets
            .iter()
            .any(|market| market.max_stranded_qty_estimate >= 5.0);
        let mut notes = Vec::new();
        if no_fills_observed {
            notes.push("no fills observed after at least 60s".to_string());
        }
        if excessive_cancel_churn {
            notes.push("cancel/submit ratio exceeded 5.0 with at least 5 submits".to_string());
        }
        if poor_fill_symmetry {
            notes.push(
                "market fill symmetry below 0.35 after at least 10 filled shares".to_string(),
            );
        }
        if stranded_exposure {
            notes.push("estimated stranded exposure reached at least 5 shares".to_string());
        }
        AcceptanceGateStats {
            pass: notes.is_empty(),
            no_fills_observed,
            excessive_cancel_churn,
            poor_fill_symmetry,
            stranded_exposure,
            notes,
        }
    }

    /// Persist the summary as JSON to `output_path`. Creates the parent
    /// directory if missing. Idempotent.
    pub fn flush(&self) -> Result<()> {
        let summary = self.summary();
        if let Some(parent) = self.output_path.parent() {
            fs::create_dir_all(parent).with_context(|| {
                format!(
                    "failed to create paper report directory {}",
                    parent.display()
                )
            })?;
        }
        let mut file = fs::File::create(&self.output_path).with_context(|| {
            format!(
                "failed to create paper report file {}",
                self.output_path.display()
            )
        })?;
        let body = serde_json::to_string_pretty(&summary)
            .context("failed to serialize PaperReportSummary")?;
        file.write_all(body.as_bytes()).with_context(|| {
            format!(
                "failed to write paper report to {}",
                self.output_path.display()
            )
        })?;
        Ok(())
    }

    pub fn output_path(&self) -> &Path {
        &self.output_path
    }

    pub fn started_at_ms(&self) -> u64 {
        self.started_at_ms
    }
}

fn increment(map: &mut BTreeMap<String, usize>, key: &str) {
    *map.entry(key.to_string()).or_default() += 1;
}

fn classify_runtime_event(message: &str) -> Option<&'static str> {
    if message.contains("entry-fill asymmetry cooldown")
        || message.contains("asymmetric entry-fill cooldown")
    {
        return Some("asym_fill_cooldown");
    }
    if message.contains("market mid moved") {
        return Some("market_mid_trend_gate");
    }
    if message.contains("btc regime flat") {
        return Some("btc_flat_gate");
    }
    if message.contains("btc regime trending") {
        return Some("btc_trend_gate");
    }
    if message.contains("premium fair cap") {
        return Some("premium_fair_gate");
    }
    if message.contains("hold stranded positive-asymmetry") {
        return Some("hold_positive_asym");
    }
    if message.contains("rescue stranded leg") {
        return Some("rescue_ev_selected");
    }
    if message.contains("on-fill IOC rescue emitted") {
        return Some("on_fill_rescue");
    }
    if message.contains("on-fill rescue throttled") {
        return Some("rescue_throttled");
    }
    if message.contains("paired entry ladder rejected") {
        return Some("entry_ladder_rejected");
    }
    if message.contains("inventory management rejected") {
        return Some("inventory_management_rejected");
    }
    if message.contains("market context replaced") {
        return Some("market_rollover");
    }
    if message.contains("runtime degraded") {
        return Some("runtime_degraded");
    }
    if message.contains("runtime risk-off") {
        return Some("runtime_riskoff");
    }
    None
}

fn classify_submit_intent(tag: &str) -> &'static str {
    if tag.starts_with("mm-paired-bid") {
        "paired_ladder"
    } else if tag.starts_with("mm-convex-accum") {
        "convex_accum"
    } else if tag.starts_with("mm-hedge-rescue") {
        "hedge_rescue"
    } else if tag.starts_with("mm-reduce") {
        "reduce_cleanup"
    } else if tag.is_empty() {
        "untagged_submit"
    } else {
        "other_submit"
    }
}

fn midpoint(best_bid: f64, best_ask: f64) -> Option<f64> {
    (best_bid > 0.0 && best_ask > 0.0 && best_ask >= best_bid)
        .then_some((best_bid + best_ask) * 0.5)
}

fn market_books_are_fresh(books: &[BookMark], target_ms: u64, expected_book_count: usize) -> bool {
    books.len() >= expected_book_count
        && books
            .iter()
            .filter(|mark| mark.observed_at_ms >= target_ms)
            .count()
            >= expected_book_count
}

fn avg_mid_move(start: &[BookMark], end: &[BookMark]) -> Option<f64> {
    let mut total = 0.0;
    let mut count = 0usize;
    for start_mark in start {
        let Some(start_mid) = start_mark.mid else {
            continue;
        };
        let Some(end_mark) = end
            .iter()
            .find(|mark| mark.instrument_id == start_mark.instrument_id)
        else {
            continue;
        };
        let Some(end_mid) = end_mark.mid else {
            continue;
        };
        total += (end_mid - start_mid).abs();
        count += 1;
    }
    (count > 0).then_some(total / count as f64)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event_log::{EventCategory, EventRecord};
    use crate::types::{CloseMethod, IntentKind, OrderIntent};

    fn fill(side: TradeSide, price: f64, qty: f64, liq: FillLiquidity, ts: u64) -> FillReport {
        FillReport {
            order_id: None,
            client_order_id: Some(ClientOrderId::from(format!("c-{ts}-{price}-{qty}"))),
            market_id: MarketId::from("m-1"),
            instrument_id: InstrumentId::from("i-1"),
            side,
            price,
            quantity: qty,
            fee_usd: 0.01,
            liquidity: liq,
            close_method: Some(CloseMethod::Unknown),
            observed_at_ms: ts,
        }
    }

    fn submit_command(market_id: &str, tag: &str) -> RuntimeCommand {
        RuntimeCommand::Submit(OrderIntent {
            client_order_id: ClientOrderId::from(format!("coid-{market_id}-{tag}")),
            market_id: MarketId::from(market_id),
            instrument_id: InstrumentId::from(format!("asset-{market_id}")),
            side: TradeSide::Buy,
            limit_price: 0.49,
            quantity: 5.0,
            reduce_only: false,
            reason: "test".to_string(),
            quote_level_tag: Some(tag.to_string()),
            created_at_ms: 1,
            pair_id: None,
            kind: IntentKind::Entry,
        })
    }

    fn book(asset_id: &str, bid: f64, ask: f64, ts: u64) -> BookState {
        BookState::from_top_of_book(asset_id, bid, 10.0, ask, 10.0, (bid + ask) * 0.5, ts)
    }

    #[test]
    fn paper_report_summary_aggregates_maker_taker_and_edge() {
        let mut writer =
            PaperReportWriter::new("run-test", "paper", PathBuf::from("/tmp/_unused"), 1_000);
        // Maker buy at 0.50, mid was 0.52 → expected edge 0.02 * 10 = +0.20
        writer.record_submit_edge(TradeSide::Buy, 0.50, 10.0, 0.52, 1_000);
        writer.record_fill(
            &fill(TradeSide::Buy, 0.50, 10.0, FillLiquidity::Maker, 1_100),
            Some(0.52),
        );
        // Taker buy at 0.55 (limit=0.55, mid was 0.53) → expected edge -0.02 * 5 = -0.10
        writer.record_submit_edge(TradeSide::Buy, 0.55, 5.0, 0.53, 1_200);
        writer.record_fill(
            &fill(TradeSide::Buy, 0.55, 5.0, FillLiquidity::Taker, 1_250),
            Some(0.53),
        );

        let summary = writer.summary();
        assert_eq!(summary.fills.total_count, 2);
        assert_eq!(summary.fills.maker_count, 1);
        assert_eq!(summary.fills.taker_count, 1);
        assert!((summary.fills.maker_fraction - 0.5).abs() < 1e-9);
        // Expected edge: 0.02*10 + (-0.02)*5 = 0.10
        assert!((summary.edge.expected_edge_usd - 0.10).abs() < 1e-9);
        // Realized edge: (0.52-0.50)*10 + (0.53-0.55)*5 = 0.20 - 0.10 = 0.10
        assert!((summary.edge.realized_edge_usd - 0.10).abs() < 1e-9);
        let cap = summary.edge.edge_capture_ratio.expect("ratio set");
        assert!((cap - 1.0).abs() < 1e-9);
    }

    #[test]
    fn paper_report_records_strategy_attribution_and_acceptance_gates() {
        let mut writer =
            PaperReportWriter::new("run-test", "paper", PathBuf::from("/tmp/_unused"), 0);
        let records = vec![
            EventRecord::new(
                EventCategory::Strategy,
                1_000,
                "fresh paired entry suppressed by market state: market mid moved 0.100",
            )
            .with_market("m-1"),
            EventRecord::new(
                EventCategory::Runtime,
                2_000,
                "requested order cancellation from Working",
            )
            .with_market("m-1"),
        ];
        let commands = vec![
            submit_command("m-1", "mm-paired-bid:l1"),
            RuntimeCommand::Cancel {
                client_order_id: ClientOrderId::from("coid-m-1"),
                reason: "no longer desired".to_string(),
            },
        ];

        writer.record_runtime_outcome(&records, &commands, 61_000);

        let summary = writer.summary();
        assert_eq!(
            summary.strategy.events_by_type.get("market_mid_trend_gate"),
            Some(&1)
        );
        assert_eq!(
            summary.strategy.intents_by_type.get("paired_ladder"),
            Some(&1)
        );
        assert_eq!(summary.strategy.intents_by_type.get("cancel"), Some(&1));
        assert_eq!(summary.markets.len(), 1);
        assert_eq!(summary.markets[0].submit_count, 1);
        assert_eq!(summary.markets[0].cancel_count, 1);
        assert_eq!(
            summary.markets[0]
                .strategy_events_by_type
                .get("market_mid_trend_gate"),
            Some(&1)
        );
        assert!(summary.acceptance.no_fills_observed);
        assert!(!summary.acceptance.pass);
    }

    #[test]
    fn paper_report_records_suppression_future_book_marks() {
        let mut writer =
            PaperReportWriter::new("run-test", "paper", PathBuf::from("/tmp/_unused"), 0);
        let market_id = MarketId::from("m-1");
        let up = InstrumentId::from("up");
        let down = InstrumentId::from("down");
        writer.record_book_observation(&market_id, &up, &book("up", 0.49, 0.51, 1_000));
        writer.record_book_observation(&market_id, &down, &book("down", 0.49, 0.51, 1_000));
        writer.record_runtime_outcome(
            &[EventRecord::new(
                EventCategory::Strategy,
                1_000,
                "btc-5m-mm no quote: market mid moved 0.100",
            )
            .with_market("m-1")],
            &[],
            1_000,
        );
        writer.record_book_observation(&market_id, &up, &book("up", 0.59, 0.61, 61_000));
        writer.record_book_observation(&market_id, &down, &book("down", 0.39, 0.41, 61_000));

        let summary = writer.summary();
        assert_eq!(summary.suppression_samples.len(), 1);
        let sample = &summary.suppression_samples[0];
        assert_eq!(sample.reason_type, "market_mid_trend_gate");
        assert_eq!(sample.books_at_suppress.len(), 2);
        assert_eq!(sample.books_after_60s.len(), 2);
        assert!(sample.avg_mid_move_60s.is_some_and(|move_| move_ > 0.09));
    }

    #[test]
    fn paper_report_summary_handles_empty_session() {
        let writer = PaperReportWriter::new("empty", "paper", PathBuf::from("/tmp/_unused"), 0);
        let summary = writer.summary();
        assert_eq!(summary.fills.total_count, 0);
        assert_eq!(summary.fills.maker_fraction, 0.0);
        assert_eq!(summary.edge.expected_edge_usd, 0.0);
        assert!(summary.edge.edge_capture_ratio.is_none());
    }

    #[test]
    fn vs_whale_section_aggregates_whale_fills_and_capture_ratio() {
        let mut writer = PaperReportWriter::new(
            "vs-whale-test",
            "shadow_live",
            PathBuf::from("/tmp/_unused"),
            1_000,
        );
        // Our fills: 2 buys, total notional 0.5*10 + 0.55*5 = 7.75
        writer.record_fill(
            &fill(TradeSide::Buy, 0.50, 10.0, FillLiquidity::Maker, 1_100),
            None,
        );
        writer.record_fill(
            &fill(TradeSide::Buy, 0.55, 5.0, FillLiquidity::Taker, 1_200),
            None,
        );
        // Whale fills: 3 buys + 1 sell, total notional 100.0
        writer.record_whale_fill_observed(1_050, Some("buy"), 30.0);
        writer.record_whale_fill_observed(1_150, Some("Buy"), 40.0);
        writer.record_whale_fill_observed(1_180, Some("buy"), 20.0);
        writer.record_whale_fill_observed(1_220, Some("sell"), 10.0);

        let summary = writer.summary();
        assert_eq!(summary.vs_whale.whale_fill_count, 4);
        assert_eq!(summary.vs_whale.whale_buy_count, 3);
        assert_eq!(summary.vs_whale.whale_sell_count, 1);
        assert!((summary.vs_whale.whale_total_notional_usd - 100.0).abs() < 1e-9);
        assert!((summary.vs_whale.our_total_notional_usd - 7.75).abs() < 1e-9);
        let cap = summary.vs_whale.notional_capture_ratio.expect("set");
        assert!((cap - 0.0775).abs() < 1e-9);
        // Our sells = 0 vs whale sells = 1; our buys = 2 vs whale buys = 3
        // imbalance = |2-3| + |0-1| = 2
        assert_eq!(summary.vs_whale.directional_imbalance_count, 2);
    }

    #[test]
    fn vs_whale_section_handles_no_whale_data() {
        let mut writer =
            PaperReportWriter::new("no-whale", "paper", PathBuf::from("/tmp/_unused"), 0);
        writer.record_fill(
            &fill(TradeSide::Buy, 0.5, 1.0, FillLiquidity::Maker, 1),
            None,
        );
        let summary = writer.summary();
        assert_eq!(summary.vs_whale.whale_fill_count, 0);
        assert!(summary.vs_whale.notional_capture_ratio.is_none());
    }

    #[test]
    fn paper_report_records_rejects_and_late_fills() {
        let mut writer = PaperReportWriter::new("rj", "paper", PathBuf::from("/tmp/_unused"), 0);
        writer.record_reject(
            &ClientOrderId::from("c-1"),
            &MarketId::from("m"),
            &InstrumentId::from("i"),
            0.55,
            "post-only-cross-paper",
            1_000,
        );
        writer.record_late_fill_after_cancel();
        writer.record_late_fill_after_cancel();
        let summary = writer.summary();
        assert_eq!(summary.queue.post_only_reject_count, 1);
        assert_eq!(summary.queue.late_fill_after_cancel_count, 2);
    }

    #[test]
    fn paper_report_flush_writes_valid_json() {
        let tmp =
            std::env::temp_dir().join(format!("paper-report-flush-{}.json", std::process::id()));
        let mut writer = PaperReportWriter::new("flush", "paper", tmp.clone(), 0);
        writer.record_fill(
            &fill(TradeSide::Buy, 0.5, 1.0, FillLiquidity::Maker, 1),
            None,
        );
        writer.flush().expect("flush ok");
        let body = std::fs::read_to_string(&tmp).expect("read back");
        let parsed: serde_json::Value = serde_json::from_str(&body).expect("valid json");
        assert_eq!(parsed["fills"]["total_count"].as_u64(), Some(1));
        let _ = std::fs::remove_file(&tmp);
    }
}
