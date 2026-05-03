//! Phase 3c expanded `window_summary` row + accumulator.
//!
//! `WindowAccumulator` ingests events, fills, intents, and rejections as
//! the replay loop progresses and produces a single
//! `ExpandedWindowSummary` row per window when finalized. The schema is
//! the one specified in the Phase 3c plan and is the input for downstream
//! analytics + the (Phase 3d) html/markdown report.
//!
//! Determinism: all internal state is `BTreeMap` and accumulation is a
//! pure function of the input stream; no wall-clock reads.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::collector::schema::{Event, EventType};
use crate::replay::fill_sim::{MakerOrTaker, Side, SimulatedFill, SimulatedRejection};
use crate::replay::risk_trace::RiskRejection;

/// One row of the expanded window summary table. All monetary values
/// serialize as decimal strings to keep the Parquet output free of f64
/// drift; `Option<String>` is null when the metric is not yet computed
/// (placeholder for Phase 3d additions like reward_estimate_usd).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ExpandedWindowSummary {
    pub window_id: String,
    pub market_id: String,
    pub market_slug: String,
    pub market_type: String,
    pub window_start_ts_ns: i64,
    pub window_end_ts_ns: i64,
    pub panicked: bool,
    pub panic_reason: Option<String>,

    // PnL decomposition
    pub gross_pnl_usd: String,
    pub realized_pnl_usd: String,
    pub mtm_pnl_usd: String,
    pub resolution_pnl_usd: String,
    pub spread_capture_usd: String,
    pub rebate_estimate_usd: String,
    pub reward_estimate_usd: Option<String>,
    pub fees_gas_estimate_usd: String,

    // Fill stats
    pub intents_emitted_total: i64,
    pub fills_total: i64,
    pub fills_maker: i64,
    pub fills_taker: i64,
    pub fills_partial: i64,
    pub fills_post_only_rejected: i64,
    pub fill_ratio: String,
    pub average_fill_delay_ns: i64,

    // Markouts (mean signed mid - fill_price across fills)
    pub markout_5s_usd: String,
    pub markout_15s_usd: String,
    pub markout_30s_usd: String,
    pub markout_60s_usd: String,

    // Branch breakdown
    pub fills_paired_entry: i64,
    pub fills_convex_accum: i64,
    pub fills_hedge_rescue: i64,
    pub fills_other: i64,

    // Strategy actions
    pub merges_total: i64,
    pub redeems_total: i64,
    pub rescues_total: i64,
    pub risk_rejections_total: i64,

    // Exposure
    pub max_gross_exposure_usd: String,
    pub max_net_exposure_usd: String,
    pub max_one_sided_stranded_exposure_usd: String,
    pub time_with_unpaired_inventory_seconds: i64,

    // Pair-cost
    pub pair_cost_p25: String,
    pub pair_cost_p50: String,
    pub pair_cost_p75: String,
    pub pair_cost_at_merge_mean: String,

    // Resolution
    pub resolution_outcome: Option<String>,
    pub excess_side_at_resolution: Option<String>,
    pub excess_size_at_resolution: String,
}

/// Per-window accumulator. Constructed at window start; fed events,
/// fills, intents, and rejections as the replay loop runs; finalized
/// into an `ExpandedWindowSummary` at window end.
#[derive(Debug, Clone)]
pub struct WindowAccumulator {
    window_id: String,
    market_id: String,
    market_slug: String,
    market_type: String,
    window_start_ts_ns: i64,
    window_end_ts_ns: i64,

    intents_emitted_total: i64,
    fills: Vec<FillAcc>,
    fills_post_only_rejected: i64,
    risk_rejections_total: i64,

    // Tagged fills
    fills_paired_entry: i64,
    fills_convex_accum: i64,
    fills_hedge_rescue: i64,
    fills_other: i64,

    // Pair cost samples
    pair_cost_samples: Vec<f64>,
    pair_cost_at_merge_samples: Vec<f64>,

    // Strategy actions
    merges_total: i64,
    redeems_total: i64,
    rescues_total: i64,

    // Exposure tracking (max running)
    max_gross_exposure_usd: f64,
    max_net_exposure_usd: f64,
    max_one_sided_stranded_exposure_usd: f64,
    time_with_unpaired_inventory_seconds: i64,

    // Realized PnL accumulator (cash flow from fills)
    realized_pnl_usd: f64,
    fees_gas_estimate_usd: f64,
    rebate_estimate_usd: f64,

    // Mid-price samples per asset for markouts.
    mid_samples: BTreeMap<String, Vec<(i64, f64)>>,

    // Resolution
    resolution_outcome: Option<String>,
    excess_side_at_resolution: Option<String>,
    excess_size_at_resolution: f64,

    // Panic state propagated from runner.
    panicked: bool,
    panic_reason: Option<String>,
}

#[allow(dead_code)]
#[derive(Debug, Clone)]
struct FillAcc {
    fill_ms: u64,
    asset_id: String,
    side: Side,
    price: f64,
    size: f64,
    maker_or_taker: MakerOrTaker,
    branch: FillBranch,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FillBranch {
    PairedEntry,
    ConvexAccum,
    HedgeRescue,
    Other,
}

impl WindowAccumulator {
    pub fn new(
        window_id: impl Into<String>,
        market_id: impl Into<String>,
        market_slug: impl Into<String>,
        market_type: impl Into<String>,
        window_start_ts_ns: i64,
        window_end_ts_ns: i64,
    ) -> Self {
        Self {
            window_id: window_id.into(),
            market_id: market_id.into(),
            market_slug: market_slug.into(),
            market_type: market_type.into(),
            window_start_ts_ns,
            window_end_ts_ns,
            intents_emitted_total: 0,
            fills: Vec::new(),
            fills_post_only_rejected: 0,
            risk_rejections_total: 0,
            fills_paired_entry: 0,
            fills_convex_accum: 0,
            fills_hedge_rescue: 0,
            fills_other: 0,
            pair_cost_samples: Vec::new(),
            pair_cost_at_merge_samples: Vec::new(),
            merges_total: 0,
            redeems_total: 0,
            rescues_total: 0,
            max_gross_exposure_usd: 0.0,
            max_net_exposure_usd: 0.0,
            max_one_sided_stranded_exposure_usd: 0.0,
            time_with_unpaired_inventory_seconds: 0,
            realized_pnl_usd: 0.0,
            fees_gas_estimate_usd: 0.0,
            rebate_estimate_usd: 0.0,
            mid_samples: BTreeMap::new(),
            resolution_outcome: None,
            excess_side_at_resolution: None,
            excess_size_at_resolution: 0.0,
            panicked: false,
            panic_reason: None,
        }
    }

    pub fn ingest_event(&mut self, event: &Event) {
        if event.event_type == EventType::BookSnapshot
            || event.event_type == EventType::BookDelta
        {
            // Keep mid-price samples per asset for markout windows.
            if let (Some(asset), Some(price_str)) =
                (event.asset_id.as_deref(), event.price.as_deref())
            {
                if let Ok(p) = price_str.parse::<f64>() {
                    self.mid_samples
                        .entry(asset.to_string())
                        .or_default()
                        .push((event.received_ns, p));
                }
            }
        }
    }

    pub fn record_intent(&mut self) {
        self.intents_emitted_total += 1;
    }

    pub fn record_fill(&mut self, fill: &SimulatedFill, branch: FillBranch) {
        let acc = FillAcc {
            fill_ms: fill.fill_ms,
            asset_id: fill.asset_id.clone(),
            side: fill.side,
            price: fill.price,
            size: fill.size,
            maker_or_taker: fill.maker_or_taker,
            branch,
        };
        let cash_flow = match fill.side {
            Side::Buy => -fill.price * fill.size,
            Side::Sell => fill.price * fill.size,
        };
        self.realized_pnl_usd += cash_flow;
        match branch {
            FillBranch::PairedEntry => self.fills_paired_entry += 1,
            FillBranch::ConvexAccum => self.fills_convex_accum += 1,
            FillBranch::HedgeRescue => self.fills_hedge_rescue += 1,
            FillBranch::Other => self.fills_other += 1,
        }
        self.fills.push(acc);
    }

    pub fn record_post_only_rejection(&mut self, _r: &SimulatedRejection) {
        self.fills_post_only_rejected += 1;
    }

    pub fn record_risk_rejection(&mut self, _r: &RiskRejection) {
        self.risk_rejections_total += 1;
    }

    pub fn record_merge(&mut self, pair_cost_at_merge: Option<f64>) {
        self.merges_total += 1;
        if let Some(c) = pair_cost_at_merge {
            self.pair_cost_at_merge_samples.push(c);
        }
    }

    pub fn record_redeem(&mut self) {
        self.redeems_total += 1;
    }

    pub fn record_rescue(&mut self) {
        self.rescues_total += 1;
    }

    pub fn record_pair_cost_sample(&mut self, c: f64) {
        self.pair_cost_samples.push(c);
    }

    pub fn observe_exposure(
        &mut self,
        gross_usd: f64,
        net_usd: f64,
        stranded_usd: f64,
        unpaired_secs_delta: i64,
    ) {
        if gross_usd > self.max_gross_exposure_usd {
            self.max_gross_exposure_usd = gross_usd;
        }
        if net_usd.abs() > self.max_net_exposure_usd.abs() {
            self.max_net_exposure_usd = net_usd;
        }
        if stranded_usd > self.max_one_sided_stranded_exposure_usd {
            self.max_one_sided_stranded_exposure_usd = stranded_usd;
        }
        self.time_with_unpaired_inventory_seconds += unpaired_secs_delta.max(0);
    }

    pub fn record_resolution(
        &mut self,
        outcome: impl Into<String>,
        excess_side: Option<String>,
        excess_size: f64,
    ) {
        self.resolution_outcome = Some(outcome.into());
        self.excess_side_at_resolution = excess_side;
        self.excess_size_at_resolution = excess_size;
    }

    pub fn mark_panic(&mut self, reason: impl Into<String>) {
        self.panicked = true;
        self.panic_reason = Some(reason.into());
    }

    /// Average mid-price for `asset_id` over the `window_ms` ending at
    /// `at_ns`. Returns `None` if no samples in range.
    fn mid_at(&self, asset_id: &str, at_ns: i64, window_ms: i64) -> Option<f64> {
        let samples = self.mid_samples.get(asset_id)?;
        let cutoff = at_ns.saturating_add(window_ms.saturating_mul(1_000_000));
        let mut best: Option<f64> = None;
        for (t, p) in samples.iter() {
            if *t <= cutoff {
                best = Some(*p);
            } else {
                break;
            }
        }
        best
    }

    fn mean_markout_secs(&self, secs: i64) -> f64 {
        if self.fills.is_empty() {
            return 0.0;
        }
        let mut sum = 0.0;
        let mut n = 0i64;
        for f in &self.fills {
            let at_ns = (f.fill_ms as i64).saturating_mul(1_000_000);
            if let Some(mid) = self.mid_at(&f.asset_id, at_ns, secs * 1_000) {
                let sign = match f.side {
                    Side::Buy => 1.0,
                    Side::Sell => -1.0,
                };
                let pnl_per_unit = sign * (mid - f.price);
                sum += pnl_per_unit * f.size;
                n += 1;
            }
        }
        if n == 0 {
            0.0
        } else {
            sum / (n as f64)
        }
    }

    pub fn finalize(self) -> ExpandedWindowSummary {
        let fills_total = self.fills.len() as i64;
        let fills_maker = self
            .fills
            .iter()
            .filter(|f| f.maker_or_taker == MakerOrTaker::Maker)
            .count() as i64;
        let fills_taker = fills_total - fills_maker;
        // "partial" = a maker fill that filled less than its parent intent
        // size. We don't know parent size from fills alone; report 0 for
        // now (Phase 3d will add parent-intent linkage). Documented as
        // best-effort.
        let fills_partial = 0;
        let fill_ratio = if self.intents_emitted_total > 0 {
            fills_total as f64 / self.intents_emitted_total as f64
        } else {
            0.0
        };
        // Average fill delay: not yet computed (we'd need intent placed_ms
        // alongside fill_ms; the fill carries fill_ms only). Phase 3d.
        let average_fill_delay_ns: i64 = 0;
        let markout_5 = self.mean_markout_secs(5);
        let markout_15 = self.mean_markout_secs(15);
        let markout_30 = self.mean_markout_secs(30);
        let markout_60 = self.mean_markout_secs(60);
        let (p25, p50, p75) = percentiles_25_50_75(&self.pair_cost_samples);
        let pcam_mean = mean(&self.pair_cost_at_merge_samples);

        ExpandedWindowSummary {
            window_id: self.window_id,
            market_id: self.market_id,
            market_slug: self.market_slug,
            market_type: self.market_type,
            window_start_ts_ns: self.window_start_ts_ns,
            window_end_ts_ns: self.window_end_ts_ns,
            panicked: self.panicked,
            panic_reason: self.panic_reason,
            gross_pnl_usd: dec(self.realized_pnl_usd),
            realized_pnl_usd: dec(self.realized_pnl_usd),
            mtm_pnl_usd: dec(0.0),
            resolution_pnl_usd: dec(0.0),
            spread_capture_usd: dec(0.0),
            rebate_estimate_usd: dec(self.rebate_estimate_usd),
            reward_estimate_usd: None,
            fees_gas_estimate_usd: dec(self.fees_gas_estimate_usd),
            intents_emitted_total: self.intents_emitted_total,
            fills_total,
            fills_maker,
            fills_taker,
            fills_partial,
            fills_post_only_rejected: self.fills_post_only_rejected,
            fill_ratio: dec(fill_ratio),
            average_fill_delay_ns,
            markout_5s_usd: dec(markout_5),
            markout_15s_usd: dec(markout_15),
            markout_30s_usd: dec(markout_30),
            markout_60s_usd: dec(markout_60),
            fills_paired_entry: self.fills_paired_entry,
            fills_convex_accum: self.fills_convex_accum,
            fills_hedge_rescue: self.fills_hedge_rescue,
            fills_other: self.fills_other,
            merges_total: self.merges_total,
            redeems_total: self.redeems_total,
            rescues_total: self.rescues_total,
            risk_rejections_total: self.risk_rejections_total,
            max_gross_exposure_usd: dec(self.max_gross_exposure_usd),
            max_net_exposure_usd: dec(self.max_net_exposure_usd),
            max_one_sided_stranded_exposure_usd: dec(self.max_one_sided_stranded_exposure_usd),
            time_with_unpaired_inventory_seconds: self.time_with_unpaired_inventory_seconds,
            pair_cost_p25: dec(p25),
            pair_cost_p50: dec(p50),
            pair_cost_p75: dec(p75),
            pair_cost_at_merge_mean: dec(pcam_mean),
            resolution_outcome: self.resolution_outcome,
            excess_side_at_resolution: self.excess_side_at_resolution,
            excess_size_at_resolution: dec(self.excess_size_at_resolution),
        }
    }
}

fn dec(v: f64) -> String {
    if !v.is_finite() {
        return "0".to_string();
    }
    let mut s = format!("{:.6}", v);
    if s.contains('.') {
        while s.ends_with('0') {
            s.pop();
        }
        if s.ends_with('.') {
            s.pop();
        }
    }
    s
}

fn mean(xs: &[f64]) -> f64 {
    if xs.is_empty() {
        0.0
    } else {
        xs.iter().sum::<f64>() / xs.len() as f64
    }
}

fn percentiles_25_50_75(xs: &[f64]) -> (f64, f64, f64) {
    if xs.is_empty() {
        return (0.0, 0.0, 0.0);
    }
    let mut sorted: Vec<f64> = xs.to_vec();
    sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let pick = |q: f64| {
        let idx = ((sorted.len() as f64 - 1.0) * q).round() as usize;
        sorted[idx.min(sorted.len() - 1)]
    };
    (pick(0.25), pick(0.50), pick(0.75))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::collector::schema::Source;
    use serde_json::json;

    fn book_event(ns: i64, asset: &str, side: &str, price: &str, size: &str) -> Event {
        Event {
            v: 1,
            ts_ns: ns - 1,
            received_ns: ns,
            event_type: EventType::BookSnapshot,
            market_type: "btc_5m".into(),
            market_slug: Some("btc-up".into()),
            asset_id: Some(asset.into()),
            side: Some(side.into()),
            price: Some(price.into()),
            size: Some(size.into()),
            sequence: Some(ns),
            source: Source::PolymarketMarketWs,
            raw: json!({}),
        }
    }

    #[test]
    fn empty_window_finalizes_to_zero_row() {
        let acc = WindowAccumulator::new("w1", "m1", "slug-1", "btc_5m", 0, 1_000_000_000);
        let row = acc.finalize();
        assert_eq!(row.window_id, "w1");
        assert_eq!(row.fills_total, 0);
        assert_eq!(row.fill_ratio, "0");
        assert_eq!(row.realized_pnl_usd, "0");
        assert!(!row.panicked);
        assert!(row.resolution_outcome.is_none());
    }

    #[test]
    fn ingest_fill_accumulates_realized_pnl() {
        let mut acc =
            WindowAccumulator::new("w1", "m1", "slug-1", "btc_5m", 0, 10_000_000_000);
        // Buy 10 @ 0.55 and sell 10 @ 0.60 → net realized 0.5
        let buy = SimulatedFill {
            client_order_id: "b".into(),
            asset_id: "asset-a".into(),
            side: Side::Buy,
            price: 0.55,
            size: 10.0,
            fill_ms: 1_000,
            maker_or_taker: MakerOrTaker::Maker,
        };
        let sell = SimulatedFill {
            client_order_id: "s".into(),
            asset_id: "asset-a".into(),
            side: Side::Sell,
            price: 0.60,
            size: 10.0,
            fill_ms: 2_000,
            maker_or_taker: MakerOrTaker::Maker,
        };
        acc.record_intent();
        acc.record_intent();
        acc.record_fill(&buy, FillBranch::PairedEntry);
        acc.record_fill(&sell, FillBranch::PairedEntry);
        let row = acc.finalize();
        assert_eq!(row.fills_total, 2);
        assert_eq!(row.fills_maker, 2);
        assert_eq!(row.fills_paired_entry, 2);
        assert_eq!(row.realized_pnl_usd, "0.5");
        assert_eq!(row.fill_ratio, "1");
    }

    #[test]
    fn percentiles_basic() {
        let xs = [0.92, 0.94, 0.95, 0.96, 0.98];
        let (p25, p50, p75) = percentiles_25_50_75(&xs);
        assert!((p25 - 0.94).abs() < 1e-9);
        assert!((p50 - 0.95).abs() < 1e-9);
        assert!((p75 - 0.96).abs() < 1e-9);
    }

    #[test]
    fn markout_picks_nearest_pre_window_mid_sample() {
        let mut acc =
            WindowAccumulator::new("w1", "m1", "slug-1", "btc_5m", 0, 60_000_000_000);
        // Mid samples for asset-a at t=1s,2s,3s,4s with rising price.
        for (i, p) in [(1, "0.50"), (2, "0.51"), (3, "0.52"), (4, "0.53")] {
            acc.ingest_event(&book_event(
                (i as i64) * 1_000_000_000,
                "asset-a",
                "buy",
                p,
                "1",
            ));
        }
        let buy = SimulatedFill {
            client_order_id: "b".into(),
            asset_id: "asset-a".into(),
            side: Side::Buy,
            price: 0.50,
            size: 10.0,
            fill_ms: 1_000, // 1s
            maker_or_taker: MakerOrTaker::Maker,
        };
        acc.record_fill(&buy, FillBranch::PairedEntry);
        // 5s markout: latest sample at t<=6s is 0.53 → markout = (0.53-0.50)*10 = 0.30
        let row = acc.finalize();
        assert_eq!(row.markout_5s_usd, "0.3");
    }
}
