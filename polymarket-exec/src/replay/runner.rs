//! Deterministic replay loop.
//!
//! Drives a `ReplayStrategy` over a sorted, deduped stream of `Event`
//! records. Per-event lifecycle:
//!
//! 1. Advance the virtual clock to `event.received_ns`.
//! 2. Update orderbook state (delegated to the strategy via `on_event` —
//!    the live `core::book::BookStore` is accessible to the strategy
//!    adapter).
//! 3. Tick the strategy. Returned intents go through `FillSimulator`.
//! 4. Drain any fills and forward them to `on_fill`.
//!
//! Bug isolation: each window is wrapped in `panic::catch_unwind`. A
//! strategy panic in window N records a `Panicked` outcome and does not
//! poison any other window.
//!
//! Determinism: no `Instant::now`, no `SystemTime::now`, no
//! `chrono::Utc::now`. The clock is `received_ns` only. RNG, if any,
//! flows from `Seed64` derived per-window. `BTreeMap` everywhere so
//! iteration order is fixed.

use std::collections::BTreeMap;
use std::panic::{self, AssertUnwindSafe};

use anyhow::Result;
use rayon::prelude::*;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::collector::schema::{Event, EventType};
use crate::replay::fill_sim::{
    FillQuality, FillSimConfig, FillSimulator, MakerOrTaker, Side, SimulatedFill,
    SimulatedOrderSubmission, SimulatedRejection, StrategyOrderIntent,
};
use crate::replay::journal::JournalEvent;
use crate::replay::risk_trace::RiskRejection;
use crate::replay::synthesizer::EventSynthesizer;

/// What a window outputs. Aggregated into the run summary.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WindowSummary {
    pub window_id: String,
    pub events_replayed: u64,
    pub intents_submitted: u64,
    /// Stable per-window fingerprint of the replay input that generated this
    /// summary.
    #[serde(default)]
    pub input_hash: String,
    /// Compact simulator-derived queue/fill calibration summary. Full
    /// per-order traces are intentionally not embedded here because a single
    /// window can emit hundreds of thousands of quote replacements.
    #[serde(default)]
    pub queue_calibration: ReplayQueueCalibrationSummary,
    /// Bounded sample of simulator-normalized submissions for inspection.
    /// This keeps large replay reports compact while still answering "what
    /// did the strategy actually try to rest?" for smoke/debug windows.
    #[serde(default)]
    pub submitted_order_samples: Vec<SimulatedOrderSubmission>,
    pub fills: Vec<SimulatedFill>,
    /// Deterministic accounting from the replayed fill stream plus the
    /// replayed market marks. This is the canonical source for backtest
    /// PnL/equity fields; downstream reports should not infer PnL from
    /// `filled_qty * limit_price` alone.
    #[serde(default)]
    pub accounting: ReplayAccountingSummary,
    /// Post-only rejections produced by the fill simulator (intents that
    /// would have crossed the live book at submit time).
    #[serde(default)]
    pub post_only_rejections: Vec<SimulatedRejection>,
    /// Risk-engine rejections captured during replay. Emitted to
    /// `runs/run_id=<id>/trace/strand=risk_rejections/...parquet` by the
    /// downstream writer (Phase 3d wiring; the strand is captured here).
    #[serde(default)]
    pub risk_rejections: Vec<RiskRejection>,
    /// Append-only audit-trail journal events captured per window. Each
    /// row maps to one of the discriminated event_kinds documented in
    /// `replay::journal`. The runner does not write Parquet here; the
    /// CLI binary aggregates summaries across windows and writes a
    /// single `journal.parquet` per run.
    #[serde(default, skip_serializing)]
    pub journal_events: Vec<JournalEvent>,
    pub status: WindowStatus,
}

#[derive(Debug, Clone)]
struct WindowPlan {
    window_id: String,
    events: Vec<Event>,
    index: usize,
}

/// Deterministic per-window input fingerprint used for replay reproducibility
/// checks.
pub fn compute_window_input_hash(window_id: &str, events: &[Event]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(window_id.as_bytes());
    for event in events {
        let event_bytes = serde_json::to_vec(event).expect("event must serialize for hash");
        hasher.update(&event_bytes);
        hasher.update(b"\n");
    }
    hasher
        .finalize()
        .iter()
        .map(|byte| format!("{:02x}", byte))
        .collect()
}

/// Fold independent window summaries into sequential carry-forward cash/equity.
///
/// Input order is market-window order. This preserves total PnL while lifting
/// each summary into the carry-forward cash baseline.
pub fn fold_cash_carry(windows: &mut [WindowSummary], starting_cash_usd: f64) -> f64 {
    let mut carried_cash_usd = starting_cash_usd;
    for summary in windows.iter_mut() {
        let delta_cash = summary.accounting.ending_cash_usd - summary.accounting.starting_cash_usd;
        let cash_shift = carried_cash_usd - summary.accounting.starting_cash_usd;

        summary.accounting.starting_cash_usd = carried_cash_usd;
        summary.accounting.ending_cash_usd = carried_cash_usd + delta_cash;
        summary.accounting.ending_equity_usd += cash_shift;

        if summary.status == WindowStatus::Ok {
            carried_cash_usd = summary.accounting.ending_cash_usd;
        }
    }

    carried_cash_usd
}

#[cfg(test)]
mod replay_accounting_tests {
    use super::*;
    use serde_json::json;

    use crate::collector::schema::Source;
    use crate::replay::fill_sim::MakerOrTaker;

    fn mark_event(asset_id: &str, price: &str) -> Event {
        Event {
            v: 1,
            ts_ns: 1,
            received_ns: 1,
            event_type: EventType::BookSnapshot,
            market_type: "btc_5m".to_string(),
            market_slug: Some("btc-up-or-down".to_string()),
            asset_id: Some(asset_id.to_string()),
            side: Some("buy".to_string()),
            price: Some(price.to_string()),
            size: Some("100".to_string()),
            sequence: Some(1),
            source: Source::PolymarketMarketWs,
            raw: json!({}),
        }
    }

    fn resolution_event(winner_asset_id: &str) -> Event {
        Event {
            v: 1,
            ts_ns: 2,
            received_ns: 2,
            event_type: EventType::Resolution,
            market_type: "btc_5m".to_string(),
            market_slug: Some("btc-up-or-down".to_string()),
            asset_id: None,
            side: None,
            price: None,
            size: None,
            sequence: Some(2),
            source: Source::Synthesizer,
            raw: json!({ "winner_asset_id": winner_asset_id }),
        }
    }

    fn resolution_outcome_event(winning_outcome: &str, fee_usd: f64, gas_usd: f64) -> Event {
        Event {
            v: 1,
            ts_ns: 2,
            received_ns: 2,
            event_type: EventType::Resolution,
            market_type: "btc_5m".to_string(),
            market_slug: Some("btc-up-or-down".to_string()),
            asset_id: None,
            side: None,
            price: None,
            size: None,
            sequence: Some(2),
            source: Source::Synthesizer,
            raw: json!({
                "winning_outcome": winning_outcome,
                "redeem_fee_usd": fee_usd,
                "redeem_gas_usd": gas_usd
            }),
        }
    }

    fn buy_fill(asset_id: &str, price: f64, size: f64) -> SimulatedFill {
        SimulatedFill {
            client_order_id: format!("buy-{asset_id}"),
            asset_id: asset_id.to_string(),
            side: Side::Buy,
            price,
            size,
            fill_ms: 1,
            maker_or_taker: MakerOrTaker::Maker,
        }
    }

    fn tagged_buy_fill(
        client_order_id: &str,
        asset_id: &str,
        price: f64,
        size: f64,
    ) -> SimulatedFill {
        SimulatedFill {
            client_order_id: client_order_id.to_string(),
            asset_id: asset_id.to_string(),
            side: Side::Buy,
            price,
            size,
            fill_ms: 1,
            maker_or_taker: MakerOrTaker::Maker,
        }
    }

    fn intent_submit(intent_id: &str, asset_id: &str, reason_tag: &str) -> JournalEvent {
        JournalEvent::IntentSubmit {
            ts_ns: 1,
            market_slug: "btc-up-or-down".to_string(),
            asset_id: asset_id.to_string(),
            intent_id: intent_id.to_string(),
            side: "buy".to_string(),
            price: 0.5,
            size: 1.0,
            post_only: true,
            ladder_position: Some(1),
            reason_tag: reason_tag.to_string(),
        }
    }

    #[test]
    fn accounting_marks_open_inventory_to_last_replayed_market_price() {
        let accounting = compute_accounting(
            &[mark_event("UP", "0.45")],
            &[buy_fill("UP", 0.40, 10.0)],
            1_000.0,
            0.0,
        );

        assert_eq!(accounting.starting_cash_usd, 1_000.0);
        assert_eq!(accounting.ending_cash_usd, 996.0);
        assert_eq!(accounting.market_value_usd, 4.5);
        assert_eq!(accounting.ending_equity_usd, 1_000.5);
        assert_eq!(accounting.total_pnl_usd, 0.5);
        assert_eq!(accounting.unrealized_pnl_usd, 0.5);
        assert_eq!(accounting.unmarked_open_positions, 0);
        assert_eq!(accounting.mark_source, "last_replayed_market_price");
    }

    #[test]
    fn accounting_uses_resolution_winner_for_redeemable_value() {
        let events = vec![
            mark_event("UP", "0.01"),
            mark_event("DOWN", "0.99"),
            resolution_event("UP"),
        ];
        let fills = vec![buy_fill("UP", 0.40, 10.0), buy_fill("DOWN", 0.55, 10.0)];
        let accounting = compute_accounting(&events, &fills, 1_000.0, 0.0);

        assert_eq!(accounting.ending_cash_usd, 1_000.5);
        assert_eq!(accounting.redeemable_value_usd, 10.0);
        assert_eq!(accounting.market_value_usd, 0.0);
        assert_eq!(accounting.ending_equity_usd, 1_000.5);
        assert_eq!(accounting.total_pnl_usd, 0.5);
        assert_eq!(accounting.realized_pnl_usd, 0.5);
        assert_eq!(accounting.unrealized_pnl_usd, 0.0);
        assert_eq!(
            accounting.resolution_winner_asset_id,
            Some("UP".to_string())
        );
        assert_eq!(accounting.mark_source, "resolution");
        assert!(accounting.open_positions.is_empty());
        assert_eq!(accounting.settlement.redeemed_winning_qty, 10.0);
        assert_eq!(accounting.settlement.expired_losing_qty, 10.0);
        assert_eq!(accounting.settlement.stranded_qty_total, 0.0);
        assert_eq!(accounting.settlement.stranded_cost_usd, 0.0);
        assert!(accounting.settlement.stranded_inventory.is_empty());
        assert_eq!(accounting.settlement.status, "resolved_settled");
    }

    #[test]
    fn accounting_allows_negative_pnl_when_bought_leg_loses() {
        let events = vec![resolution_event("DOWN")];
        let fills = vec![buy_fill("UP", 0.90, 10.0)];
        let accounting = compute_accounting(&events, &fills, 1_000.0, 0.0);

        assert_eq!(accounting.ending_cash_usd, 991.0);
        assert_eq!(accounting.market_value_usd, 0.0);
        assert_eq!(accounting.ending_equity_usd, 991.0);
        assert_eq!(accounting.total_pnl_usd, -9.0);
        assert_eq!(accounting.realized_pnl_usd, -9.0);
        assert_eq!(accounting.unrealized_pnl_usd, 0.0);
        assert_eq!(accounting.settlement.expired_losing_qty, 10.0);
        assert_eq!(accounting.settlement.stranded_qty_total, 0.0);
        assert_eq!(accounting.settlement.stranded_cost_usd, 0.0);
    }

    #[test]
    fn accounting_maps_winning_outcome_to_yes_no_asset_ids_and_deducts_redeem_fees() {
        let events = vec![
            market_meta_event("UP_TOKEN", "DOWN_TOKEN"),
            resolution_outcome_event("Down", 0.10, 0.15),
        ];
        let fills = vec![
            buy_fill("UP_TOKEN", 0.40, 10.0),
            buy_fill("DOWN_TOKEN", 0.55, 10.0),
        ];
        let accounting = compute_accounting(&events, &fills, 1_000.0, 0.0);

        assert_eq!(
            accounting.resolution_winner_asset_id,
            Some("DOWN_TOKEN".to_string())
        );
        assert!((accounting.ending_cash_usd - 1_000.25).abs() < 1e-9);
        assert!((accounting.total_pnl_usd - 0.25).abs() < 1e-9);
        assert!((accounting.fees_paid_usd - 0.25).abs() < 1e-9);
        assert_eq!(accounting.settlement.redeemed_winning_qty, 10.0);
        assert_eq!(accounting.settlement.expired_losing_qty, 10.0);
        assert!((accounting.settlement.redeem_credit_usd - 10.0).abs() < 1e-9);
        assert!((accounting.settlement.redeem_fee_usd - 0.10).abs() < 1e-9);
        assert!((accounting.settlement.redeem_gas_usd - 0.15).abs() < 1e-9);
        assert_eq!(accounting.settlement.stranded_qty_total, 0.0);
        assert_eq!(accounting.settlement.stranded_cost_usd, 0.0);
        assert!(accounting.open_positions.is_empty());
    }

    #[test]
    fn accounting_reports_unmerged_pairable_inventory() {
        let events = vec![market_meta_event("UP", "DOWN")];
        let fills = vec![buy_fill("UP", 0.40, 10.0), buy_fill("DOWN", 0.55, 7.0)];
        let accounting = compute_accounting(&events, &fills, 1_000.0, 0.0);

        assert_eq!(accounting.settlement.pair_asset_ids, vec!["UP", "DOWN"]);
        assert_eq!(accounting.settlement.pairable_qty_before_resolution, 7.0);
        assert_eq!(accounting.settlement.merged_pair_qty, 0.0);
        assert_eq!(accounting.settlement.unmerged_pairable_qty, 7.0);
        assert_eq!(accounting.settlement.stranded_qty_total, 17.0);
        assert_eq!(
            accounting.settlement.status,
            "profitable_merge_opportunity_unexecuted"
        );
    }

    #[test]
    fn accounting_applies_successful_merge_events_once() {
        let events = vec![
            market_meta_event("UP", "DOWN"),
            merge_event("5", "confirmed"),
        ];
        let fills = vec![buy_fill("UP", 0.40, 10.0), buy_fill("DOWN", 0.55, 7.0)];
        let accounting = compute_accounting(&events, &fills, 1_000.0, 0.0);

        assert!((accounting.ending_cash_usd - 997.15).abs() < 1e-9);
        assert!((accounting.realized_pnl_usd - 0.25).abs() < 1e-9);
        assert_eq!(accounting.settlement.merge_attempted_count, 1);
        assert_eq!(accounting.settlement.merge_success_count, 1);
        assert_eq!(accounting.settlement.merged_pair_qty, 5.0);
        assert_eq!(accounting.settlement.unmerged_pairable_qty, 2.0);
    }

    #[test]
    fn accounting_does_not_credit_sell_fills_beyond_held_inventory() {
        let fills = vec![SimulatedFill {
            client_order_id: "phantom-sell".to_string(),
            asset_id: "UP".to_string(),
            side: Side::Sell,
            price: 0.70,
            size: 10.0,
            fill_ms: 1,
            maker_or_taker: MakerOrTaker::Maker,
        }];

        let accounting = compute_accounting(&[], &fills, 1_000.0, 0.0);

        assert_eq!(accounting.ending_cash_usd, 1_000.0);
        assert_eq!(accounting.ending_equity_usd, 1_000.0);
        assert_eq!(accounting.sell_notional_usd, 0.0);
        assert_eq!(accounting.invalid_fill_count, 1);
        assert!((accounting.invalid_fill_notional_usd - 7.0).abs() < 1e-9);
        assert_eq!(accounting.total_pnl_usd, 0.0);
    }

    #[test]
    fn accounting_allows_negative_pnl_when_bought_losing_leg_settles_to_zero() {
        let events = vec![
            market_meta_event("UP", "DOWN"),
            resolution_outcome_event("Down", 0.0, 0.0),
        ];
        let fills = vec![buy_fill("UP", 0.40, 10.0)];

        let accounting = compute_accounting(&events, &fills, 1_000.0, 0.0);

        assert_eq!(accounting.ending_cash_usd, 996.0);
        assert_eq!(accounting.ending_equity_usd, 996.0);
        assert_eq!(accounting.total_pnl_usd, -4.0);
        assert_eq!(accounting.realized_pnl_usd, -4.0);
        assert_eq!(accounting.settlement.expired_losing_qty, 10.0);
        assert_eq!(accounting.settlement.redeemed_winning_qty, 0.0);
    }

    #[test]
    fn attribution_breaks_terminal_pnl_into_strategy_paths() {
        let events = vec![
            market_meta_event("UP", "DOWN"),
            resolution_outcome_event("Up", 0.0, 0.0),
        ];
        let fills = vec![
            tagged_buy_fill("paired-up", "UP", 0.45, 10.0),
            tagged_buy_fill("late-down", "DOWN", 0.90, 5.0),
            tagged_buy_fill("tail-up", "UP", 0.02, 20.0),
        ];
        let journal_events = vec![
            JournalEvent::StrategyDecision {
                ts_ns: 1,
                market_slug: "btc-up-or-down".to_string(),
                asset_id: Some("UP".to_string()),
                decision_type: "paired_entry".to_string(),
                raw_inputs_hash: "h".to_string(),
                reason_tag: "mm-paired-bid:yes:l1:PairedEntry".to_string(),
            },
            intent_submit("paired-up", "UP", "mm-paired-bid:yes:l1:PairedEntry"),
            intent_submit("late-down", "DOWN", "mm-late-bar-core:l1"),
            intent_submit(
                "late-hyphen",
                "DOWN",
                "mm-convex-accum:late-favorite-only:no:ConvexAccumulation",
            ),
            intent_submit("tail-up", "UP", "mm-convex-accum:l1"),
        ];
        let accounting = compute_accounting(&events, &fills, 1_000.0, 0.0);
        let attribution = compute_pnl_attribution(
            &events,
            &fills,
            &journal_events,
            &accounting,
            FillQuality::Base,
        );

        assert_eq!(attribution.fill_quality, "base");
        assert_eq!(
            attribution.strategy_validation_status,
            "valid_strategy_replay"
        );
        assert_eq!(attribution.paired_mm.decisions_count, 1);
        assert_eq!(attribution.paired_mm.submitted_orders, 1);
        assert_eq!(attribution.late_favorite_loading.submitted_orders, 2);
        assert_eq!(attribution.cheap_tail_convexity.submitted_orders, 1);
        assert!((attribution.paired_mm.total_pnl_usd - 5.5).abs() < 1e-9);
        assert!((attribution.late_favorite_loading.total_pnl_usd + 4.5).abs() < 1e-9);
        assert!((attribution.cheap_tail_convexity.total_pnl_usd - 19.6).abs() < 1e-9);
        assert!((attribution.attributed_path_pnl_usd - accounting.total_pnl_usd).abs() < 1e-9);
        assert!(attribution.unattributed_pnl_usd.abs() < 1e-9);
        assert!(
            (attribution
                .stranded_inventory_losses
                .expired_losing_cost_usd
                - 4.5)
                .abs()
                < 1e-9
        );
    }

    #[test]
    fn attribution_reports_merge_recycling_and_explicit_costs() {
        let merge = Event {
            raw: json!({
                "type": "merge",
                "status": "confirmed",
                "size": "5",
                "fee_usd": 0.10,
                "gas_usd": 0.20
            }),
            ..merge_event("5", "confirmed")
        };
        let events = vec![market_meta_event("UP", "DOWN"), merge];
        let fills = vec![
            tagged_buy_fill("paired-up", "UP", 0.40, 5.0),
            tagged_buy_fill("paired-down", "DOWN", 0.55, 5.0),
        ];
        let journal_events = vec![
            intent_submit("paired-up", "UP", "mm-paired-bid:yes:l1:PairedEntry"),
            intent_submit("paired-down", "DOWN", "mm-paired-bid:no:l1:PairedEntry"),
        ];
        let accounting = compute_accounting(&events, &fills, 1_000.0, 0.0);
        let attribution = compute_pnl_attribution(
            &events,
            &fills,
            &journal_events,
            &accounting,
            FillQuality::Conservative,
        );

        assert_eq!(attribution.fill_quality, "conservative");
        assert_eq!(attribution.merge_redeem_recycling.merge_success_count, 1);
        assert_eq!(attribution.merge_redeem_recycling.merged_pair_qty, 5.0);
        assert!((attribution.merge_redeem_recycling.merge_realized_pnl_usd - 0.25).abs() < 1e-9);
        assert!((attribution.costs.fees_paid_usd - 0.30).abs() < 1e-9);
        assert!((attribution.costs.merge_fee_usd - 0.10).abs() < 1e-9);
        assert!((attribution.costs.merge_gas_usd - 0.20).abs() < 1e-9);
        assert!((attribution.paired_mm.total_pnl_usd - 0.25).abs() < 1e-9);
        assert!((attribution.attributed_path_pnl_usd - accounting.total_pnl_usd).abs() < 1e-9);
    }

    #[test]
    fn attribution_flags_no_intent_no_pair_smoke_as_invalid_strategy_validation() {
        let events = vec![mark_event("UP", "0.45")];
        let fills = Vec::new();
        let journal_events = Vec::new();
        let accounting = compute_accounting(&events, &fills, 1_000.0, 0.0);
        let attribution = compute_pnl_attribution(
            &events,
            &fills,
            &journal_events,
            &accounting,
            FillQuality::Base,
        );

        assert_eq!(
            attribution.strategy_validation_status,
            "invalid_non_strategy_validation"
        );
        assert!(attribution
            .invalid_reasons
            .contains(&"no_intents_submitted".to_string()));
        assert!(attribution
            .invalid_reasons
            .contains(&"no_strategy_decisions_journaled".to_string()));
        assert!(attribution
            .invalid_reasons
            .contains(&"no_binary_pair_detected".to_string()));
    }

    fn market_meta_event(up: &str, down: &str) -> Event {
        Event {
            v: 1,
            ts_ns: 0,
            received_ns: 0,
            event_type: EventType::MarketMeta,
            market_type: "btc_5m".to_string(),
            market_slug: Some("btc-up-or-down".to_string()),
            asset_id: None,
            side: None,
            price: None,
            size: None,
            sequence: Some(0),
            source: Source::PolymarketDataApi,
            raw: json!({ "asset_ids": [up, down] }),
        }
    }

    fn merge_event(size: &str, status: &str) -> Event {
        Event {
            v: 1,
            ts_ns: 3,
            received_ns: 3,
            event_type: EventType::UserOrder,
            market_type: "btc_5m".to_string(),
            market_slug: Some("btc-up-or-down".to_string()),
            asset_id: None,
            side: None,
            price: None,
            size: Some(size.to_string()),
            sequence: Some(3),
            source: Source::PolymarketUserWs,
            raw: json!({ "type": "merge", "status": status, "size": size }),
        }
    }

    #[test]
    fn maker_rebate_credits_cash_on_maker_fills_only() {
        // Two maker BUYs at $0.40 size 10 each = $4 each, $8 total notional
        // for buys. Rebate at 10 bps = $8 * 0.001 = $0.008. Cash should
        // reflect notional outflow ($8) minus rebate credit ($0.008).
        let events = vec![market_meta_event("UP", "DOWN")];
        let fills = vec![buy_fill("UP", 0.40, 10.0), buy_fill("DOWN", 0.40, 10.0)];

        let no_rebate = compute_accounting(&events, &fills, 1_000.0, 0.0);
        let with_rebate = compute_accounting(&events, &fills, 1_000.0, 10.0);

        assert!((no_rebate.maker_rebate_usd).abs() < 1e-9);
        let expected_rebate = (0.40 * 10.0 + 0.40 * 10.0) * 10.0 / 10_000.0;
        assert!(
            (with_rebate.maker_rebate_usd - expected_rebate).abs() < 1e-9,
            "expected {expected_rebate}, got {}",
            with_rebate.maker_rebate_usd
        );
        assert!(
            (with_rebate.ending_cash_usd - (no_rebate.ending_cash_usd + expected_rebate)).abs()
                < 1e-9
        );
        assert!(
            (with_rebate.realized_pnl_usd - (no_rebate.realized_pnl_usd + expected_rebate)).abs()
                < 1e-9
        );
    }

    #[test]
    fn classify_tag_recognizes_production_client_order_id_prefixes() {
        // Production strategy emits these exact prefixes; the classifier
        // must work on raw client_order_ids when journal mode is `none` so
        // attribution still buckets fills correctly without journal events.
        assert_eq!(
            classify_tag("paired-mm:btc-updown-5m-1777862100:yes:l1:1234"),
            AttributionPath::PairedMm,
        );
        assert_eq!(
            classify_tag("paired-mm-convex:btc-updown-5m-1777862100:yes:1234"),
            AttributionPath::LateFavorite,
        );
        assert_eq!(
            classify_tag("paired-mm-tail:btc-updown-5m-1777862100:no:1234"),
            AttributionPath::CheapTailConvexity,
        );
        assert_eq!(
            classify_tag("capital-recycle:btc-updown-5m-1777862100:1234"),
            AttributionPath::PairedMm,
        );
        assert_eq!(
            classify_tag("paired-mm-merge:btc-updown-5m-1777862100:1234"),
            AttributionPath::PairedMm,
        );
    }

    #[test]
    fn unrealized_lots_bucket_stranded_cost_per_path() {
        let events = vec![market_meta_event("UP", "DOWN")];
        let fills = vec![
            tagged_buy_fill("paired-mm:btc-updown-5m-1:yes:l1:1", "UP", 0.40, 10.0),
            tagged_buy_fill("paired-mm-convex:btc-updown-5m-1:yes:1", "UP", 0.55, 7.0),
        ];
        let accounting = compute_accounting(&events, &fills, 1_000.0, 0.0);
        let attribution =
            compute_pnl_attribution(&events, &fills, &[], &accounting, FillQuality::Base);

        assert!((attribution.paired_mm.stranded_cost_usd - 4.0).abs() < 1e-9);
        assert!((attribution.paired_mm.stranded_qty - 10.0).abs() < 1e-9);
        assert!(
            (attribution.late_favorite_loading.stranded_cost_usd - 3.85).abs() < 1e-9,
            "actual {}",
            attribution.late_favorite_loading.stranded_cost_usd
        );
        assert!((attribution.late_favorite_loading.stranded_qty - 7.0).abs() < 1e-9);
    }

    #[test]
    fn resolution_buckets_expired_losing_cost_per_path() {
        let events = vec![
            market_meta_event("UP", "DOWN"),
            resolution_outcome_event("Up", 0.0, 0.0),
        ];
        let fills = vec![
            tagged_buy_fill("paired-mm:btc-updown-5m-1:no:l1:1", "DOWN", 0.45, 10.0),
            tagged_buy_fill("paired-mm-convex:btc-updown-5m-1:no:1", "DOWN", 0.60, 5.0),
        ];
        let accounting = compute_accounting(&events, &fills, 1_000.0, 0.0);
        let attribution =
            compute_pnl_attribution(&events, &fills, &[], &accounting, FillQuality::Base);

        assert!((attribution.paired_mm.expired_losing_cost_usd - 4.5).abs() < 1e-9);
        assert!((attribution.paired_mm.expired_losing_qty - 10.0).abs() < 1e-9);
        assert!((attribution.late_favorite_loading.expired_losing_cost_usd - 3.0).abs() < 1e-9);
        assert!((attribution.late_favorite_loading.expired_losing_qty - 5.0).abs() < 1e-9);
        // Both losing buckets feed negative realized P&L.
        assert!((attribution.paired_mm.realized_pnl_usd + 4.5).abs() < 1e-9);
        assert!((attribution.late_favorite_loading.realized_pnl_usd + 3.0).abs() < 1e-9);
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ReplayAccountingSummary {
    pub starting_cash_usd: f64,
    pub ending_cash_usd: f64,
    pub market_value_usd: f64,
    pub ending_equity_usd: f64,
    pub total_pnl_usd: f64,
    pub realized_pnl_usd: f64,
    pub unrealized_pnl_usd: f64,
    pub fees_paid_usd: f64,
    /// Total maker rebate credited to cash this window. Zero unless
    /// RunnerConfig.maker_rebate_bps was set.
    #[serde(default)]
    pub maker_rebate_usd: f64,
    pub gross_fill_notional_usd: f64,
    pub buy_notional_usd: f64,
    pub sell_notional_usd: f64,
    #[serde(default)]
    pub invalid_fill_count: u64,
    #[serde(default)]
    pub invalid_fill_notional_usd: f64,
    pub redeemable_value_usd: f64,
    pub resolution_winner_asset_id: Option<String>,
    pub mark_source: String,
    pub unmarked_open_positions: u64,
    #[serde(default)]
    pub settlement: ReplaySettlementSummary,
    #[serde(default)]
    pub attribution: ReplayPnlAttributionSummary,
    pub open_positions: Vec<ReplayPositionSummary>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ReplayPnlAttributionSummary {
    pub fill_quality: String,
    pub strategy_validation_status: String,
    pub invalid_reasons: Vec<String>,
    pub total_pnl_check_usd: f64,
    pub attributed_path_pnl_usd: f64,
    pub unattributed_pnl_usd: f64,
    pub paired_mm: ReplayPnlAttributionBucket,
    pub late_favorite_loading: ReplayPnlAttributionBucket,
    pub cheap_tail_convexity: ReplayPnlAttributionBucket,
    pub other: ReplayPnlAttributionBucket,
    pub merge_redeem_recycling: ReplayMergeRedeemAttribution,
    pub stranded_inventory_losses: ReplayStrandedInventoryAttribution,
    pub costs: ReplayCostAttribution,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ReplayPnlAttributionBucket {
    pub decisions_count: u64,
    pub submitted_orders: u64,
    pub fill_count: u64,
    pub maker_fills: u64,
    pub taker_fills: u64,
    pub buy_notional_usd: f64,
    pub sell_notional_usd: f64,
    pub gross_notional_usd: f64,
    pub realized_pnl_usd: f64,
    pub unrealized_pnl_usd: f64,
    pub total_pnl_usd: f64,
    /// Cost basis of inventory bought through this path that resolved as the
    /// losing side. Already counted in `realized_pnl_usd` as a negative; this
    /// surfaces the gross cost so callers can answer "how much did we *spend*
    /// on losing late-convex bets" without subtracting from the P&L line.
    #[serde(default)]
    pub expired_losing_qty: f64,
    #[serde(default)]
    pub expired_losing_cost_usd: f64,
    /// Inventory bought through this path that ended the window without a
    /// resolution event observed. Marked-to-cost in `unrealized_pnl_usd`; this
    /// pair lets callers separate "MM imbalance leak" from "intentional late
    /// directional bet" by bucketing the cost basis per path.
    #[serde(default)]
    pub stranded_qty: f64,
    #[serde(default)]
    pub stranded_cost_usd: f64,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ReplayMergeRedeemAttribution {
    pub merge_success_count: u64,
    pub merged_pair_qty: f64,
    pub merge_credit_usd: f64,
    pub merge_realized_pnl_usd: f64,
    pub redeemed_winning_qty: f64,
    pub expired_losing_qty: f64,
    pub redeem_credit_usd: f64,
    pub redeem_resolution_pnl_usd: f64,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ReplayStrandedInventoryAttribution {
    pub stranded_qty_total: f64,
    pub stranded_cost_usd: f64,
    pub expired_losing_qty: f64,
    pub expired_losing_cost_usd: f64,
    pub unresolved_cost_usd: f64,
    pub unrealized_pnl_usd: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ReplayCostAttribution {
    pub fees_paid_usd: f64,
    pub merge_fee_usd: f64,
    pub merge_gas_usd: f64,
    pub redeem_fee_usd: f64,
    pub redeem_gas_usd: f64,
    pub maker_rebate_assumption_bps: f64,
    pub maker_rebate_estimate_usd: f64,
    pub taker_fee_assumption_bps: f64,
    pub taker_fee_estimate_usd: f64,
    pub assumptions: String,
}

impl Default for ReplayCostAttribution {
    fn default() -> Self {
        Self {
            fees_paid_usd: 0.0,
            merge_fee_usd: 0.0,
            merge_gas_usd: 0.0,
            redeem_fee_usd: 0.0,
            redeem_gas_usd: 0.0,
            maker_rebate_assumption_bps: 0.0,
            maker_rebate_estimate_usd: 0.0,
            taker_fee_assumption_bps: 0.0,
            taker_fee_estimate_usd: 0.0,
            assumptions:
                "replay includes explicit merge/redeem fees and gas from events; maker rebates and taker fees default to 0 until configured"
                    .to_string(),
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ReplayPositionSummary {
    pub asset_id: String,
    pub qty: f64,
    pub avg_cost_usd: f64,
    pub cost_basis_usd: f64,
    pub mark_price_usd: Option<f64>,
    pub market_value_usd: f64,
    pub unrealized_pnl_usd: f64,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ReplaySettlementSummary {
    pub pair_asset_ids: Vec<String>,
    pub merge_attempted_count: u64,
    pub merge_success_count: u64,
    pub merge_reverted_count: u64,
    pub merged_pair_qty: f64,
    pub merge_credit_usd: f64,
    #[serde(default)]
    pub merge_fee_usd: f64,
    #[serde(default)]
    pub merge_gas_usd: f64,
    pub pairable_qty_before_resolution: f64,
    pub unmerged_pairable_qty: f64,
    #[serde(default)]
    pub unmerged_pairable_cost_usd: f64,
    #[serde(default)]
    pub unmerged_pairable_merge_value_usd: f64,
    #[serde(default)]
    pub unmerged_pairable_net_gain_usd: f64,
    pub redeemed_winning_qty: f64,
    pub expired_losing_qty: f64,
    #[serde(default)]
    pub redeem_credit_usd: f64,
    #[serde(default)]
    pub redeem_fee_usd: f64,
    #[serde(default)]
    pub redeem_gas_usd: f64,
    pub stranded_qty_total: f64,
    pub stranded_cost_usd: f64,
    pub stranded_inventory: Vec<ReplaySettlementAssetInventory>,
    pub status: String,
    #[serde(default)]
    pub status_reason: String,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ReplaySettlementAssetInventory {
    pub asset_id: String,
    pub qty: f64,
    pub cost_basis_usd: f64,
    pub role: String,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ReplayQueueCalibrationSummary {
    pub sample_size: u64,
    pub filled_orders: u64,
    pub fill_rate: f64,
    pub partial_fill_rate: f64,
    pub median_seconds_to_first_fill: Option<f64>,
    pub median_same_side_depth_at_entry: Option<f64>,
    pub median_estimated_queue_position_fraction: Option<f64>,
}

#[derive(Debug, Clone, Default)]
struct ReplayPositionAccounting {
    qty: f64,
    avg_cost: f64,
}

impl ReplayPositionAccounting {
    fn buy(&mut self, price: f64, size: f64) {
        let existing_cost = self.qty * self.avg_cost;
        self.qty += size;
        self.avg_cost = if self.qty > 0.0 {
            (existing_cost + price * size) / self.qty
        } else {
            0.0
        };
    }

    fn sell(&mut self, size: f64) -> f64 {
        let closed = size.min(self.qty.max(0.0));
        self.qty = (self.qty - size).max(0.0);
        if self.qty <= f64::EPSILON {
            self.avg_cost = 0.0;
        }
        closed
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WindowStatus {
    Ok,
    Failed,
    Panicked,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReplayJournalMode {
    #[default]
    Full,
    None,
}

/// Decision emitted by a strategy on each event.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ReplayDecision {
    pub submits: Vec<StrategyOrderIntent>,
    pub cancels: Vec<String>,
    /// Simulator fills rejected by the strategy/runtime accounting layer.
    /// Candidate fills are excluded from accepted replay fills, journal fill
    /// rows, and PnL accounting when their client order id appears here.
    pub rejected_fills: Vec<String>,
    /// Risk-engine rejections produced while filtering this decision.
    /// Carried up to the runner for trace-strand emission.
    pub risk_rejections: Vec<RiskRejection>,
    /// Journal events emitted by the strategy/adapter for the replay
    /// audit trail. Strategy-decision rows, intent-submit rows, replace
    /// rows, and inventory snapshots are all populated here; fills are
    /// synthesized by the runner from the simulator output, not by the
    /// adapter.
    pub journal_events: Vec<JournalEvent>,
    /// Replay-generated lifecycle events accepted by the strategy/runtime
    /// adapter. These are appended to the accounting event stream so
    /// non-order actions such as simulated merges affect PnL/equity.
    pub accounting_events: Vec<Event>,
}

/// Trait the runner depends on. Implemented by a thin adapter over
/// `StrategyRegistry` (Phase 3b) and by the test fixture for Phase 3a.
///
/// `on_event` runs first per event. `on_fill` runs after the simulator
/// matches; both can return additional intents.
pub trait ReplayStrategy {
    fn on_event(&mut self, event: &Event) -> ReplayDecision;
    fn on_fill(&mut self, fill: &SimulatedFill) -> ReplayDecision;
    fn on_ioc_expired(&mut self, _client_order_id: &str, _now_ms: u64) -> ReplayDecision {
        ReplayDecision::default()
    }
}

/// Configuration for the replay run.
#[derive(Debug, Clone)]
pub struct RunnerConfig {
    pub window_id: String,
    pub fill_sim: FillSimConfig,
    /// Starting cash for the current replay window. The run-level driver
    /// carries prior-window ending cash into this field so accounting,
    /// strategy input snapshots, and risk evaluation share the same
    /// deployable-capital baseline.
    pub starting_cash_usd: f64,
    /// `--max-window-failures` from the CLI; used at the run level by
    /// `run_run`. A single window's `run_window` always returns whatever
    /// outcome it reaches.
    pub max_window_failures: usize,
    /// Maker rebate in basis points credited per maker fill. Default 0
    /// preserves prior accounting; set non-zero to model Polymarket's
    /// dynamic-taker-fee redistribution. The credit is added to cash and
    /// surfaced in `ReplayAccountingSummary.maker_rebate_usd` so callers
    /// can break it out separately from realised P&L.
    #[allow(dead_code)]
    pub maker_rebate_bps: f64,
}

/// Run a single window. Bug-isolated by `panic::catch_unwind` around the
/// strategy + simulator invocation so a panic does not poison adjacent
/// windows.
pub fn run_window<S: ReplayStrategy>(
    strategy: &mut S,
    events: &[Event],
    cfg: &RunnerConfig,
) -> WindowSummary {
    run_window_with_journal_mode(strategy, events, cfg, ReplayJournalMode::Full)
}

/// Run a single window with explicit journal capture mode. `run_window`
/// remains audit-grade/full by default for existing callers.
pub fn run_window_with_journal_mode<S: ReplayStrategy>(
    strategy: &mut S,
    events: &[Event],
    cfg: &RunnerConfig,
    journal_mode: ReplayJournalMode,
) -> WindowSummary {
    let input_hash = compute_window_input_hash(&cfg.window_id, events);
    let outcome = panic::catch_unwind(AssertUnwindSafe(|| {
        let mut sim = FillSimulator::new(cfg.fill_sim.clone());
        let mut synthesizer = EventSynthesizer::new();
        let mut intents_submitted: u64 = 0;
        let mut risk_rejections: Vec<RiskRejection> = Vec::new();
        let mut journal_events: Vec<JournalEvent> = Vec::new();
        let capture_journal = journal_mode == ReplayJournalMode::Full;
        let mut accepted_fills: Vec<SimulatedFill> = Vec::new();
        let mut intent_remaining: BTreeMap<String, f64> = BTreeMap::new();
        let queue_assumption = format!("{:?}", cfg.fill_sim.fill_quality);
        let mut accounting_events: Vec<Event> = Vec::with_capacity(events.len());
        for event in events {
            // Synthesise any window-open / window-close markers triggered
            // by this event. Dispatch the input event FIRST and the synthetic
            // events SECOND so causality matches: a `price_to_beat` derived
            // from a fresh `market_meta` cannot reach the strategy before the
            // `market_meta` itself, otherwise the adapter has no registered
            // market to attach the strike to and `fair_value` never escapes
            // NoSignal::StrikeInvalid (74352/74352 in the 2026-05-03 sweep).
            let synthetic_events = synthesizer.on_event(event);
            dispatch_event(
                strategy,
                &mut sim,
                event,
                &mut intents_submitted,
                &mut risk_rejections,
                &mut journal_events,
                capture_journal,
                &mut accepted_fills,
                &mut intent_remaining,
                &mut accounting_events,
                &queue_assumption,
            );
            if capture_journal {
                emit_market_event_journal_rows(event, &mut journal_events);
            }
            accounting_events.push(event.clone());
            for synth in &synthetic_events {
                dispatch_event(
                    strategy,
                    &mut sim,
                    synth,
                    &mut intents_submitted,
                    &mut risk_rejections,
                    &mut journal_events,
                    capture_journal,
                    &mut accepted_fills,
                    &mut intent_remaining,
                    &mut accounting_events,
                    &queue_assumption,
                );
                if capture_journal {
                    emit_market_event_journal_rows(synth, &mut journal_events);
                }
                accounting_events.push(synth.clone());
            }
        }
        for synth in synthesizer.flush_due(i64::MAX) {
            dispatch_event(
                strategy,
                &mut sim,
                &synth,
                &mut intents_submitted,
                &mut risk_rejections,
                &mut journal_events,
                capture_journal,
                &mut accepted_fills,
                &mut intent_remaining,
                &mut accounting_events,
                &queue_assumption,
            );
            if capture_journal {
                emit_market_event_journal_rows(&synth, &mut journal_events);
            }
            accounting_events.push(synth);
        }
        let mut accounting = compute_accounting(
            &accounting_events,
            &accepted_fills,
            cfg.starting_cash_usd,
            cfg.maker_rebate_bps,
        );
        accounting.attribution = compute_pnl_attribution(
            &accounting_events,
            &accepted_fills,
            &journal_events,
            &accounting,
            cfg.fill_sim.fill_quality,
        );
        (
            accepted_fills.clone(),
            compute_queue_calibration(sim.submissions(), &accepted_fills),
            sim.submissions()
                .iter()
                .take(64)
                .cloned()
                .collect::<Vec<_>>(),
            sim.rejections().to_vec(),
            intents_submitted,
            risk_rejections,
            journal_events,
            accounting,
        )
    }));

    match outcome {
        Ok((
            fills,
            queue_calibration,
            submitted_order_samples,
            post_only_rejections,
            intents_submitted,
            risk_rejections,
            journal_events,
            accounting,
        )) => WindowSummary {
            window_id: cfg.window_id.clone(),
            input_hash: input_hash.clone(),
            events_replayed: events.len() as u64,
            intents_submitted,
            queue_calibration,
            submitted_order_samples,
            fills,
            accounting,
            post_only_rejections,
            risk_rejections,
            journal_events,
            status: WindowStatus::Ok,
        },
        Err(_panic) => WindowSummary {
            window_id: cfg.window_id.clone(),
            input_hash: input_hash.clone(),
            events_replayed: events.len() as u64,
            intents_submitted: 0,
            queue_calibration: ReplayQueueCalibrationSummary::default(),
            submitted_order_samples: Vec::new(),
            fills: Vec::new(),
            accounting: ReplayAccountingSummary {
                starting_cash_usd: cfg.starting_cash_usd,
                ending_cash_usd: cfg.starting_cash_usd,
                ending_equity_usd: cfg.starting_cash_usd,
                mark_source: "panic_no_fills".to_string(),
                ..ReplayAccountingSummary::default()
            },
            post_only_rejections: Vec::new(),
            risk_rejections: Vec::new(),
            journal_events: Vec::new(),
            status: WindowStatus::Panicked,
        },
    }
}

fn compute_accounting(
    events: &[Event],
    fills: &[SimulatedFill],
    starting_cash_usd: f64,
    maker_rebate_bps: f64,
) -> ReplayAccountingSummary {
    let mut cash = starting_cash_usd;
    let mut realized_pnl = 0.0;
    let mut buy_notional = 0.0;
    let mut sell_notional = 0.0;
    let mut invalid_fill_count = 0u64;
    let mut invalid_fill_notional = 0.0;
    let mut maker_rebate_total = 0.0;
    let mut positions: BTreeMap<String, ReplayPositionAccounting> = BTreeMap::new();
    let rebate_factor = (maker_rebate_bps / 10_000.0).max(0.0);

    for fill in fills {
        let notional = fill.price * fill.size;
        let pos = positions.entry(fill.asset_id.clone()).or_default();
        match fill.side {
            Side::Buy => {
                cash -= notional;
                buy_notional += notional;
                pos.buy(fill.price, fill.size);
            }
            Side::Sell => {
                let avg_cost = pos.avg_cost;
                let closed = pos.sell(fill.size);
                let credited_notional = fill.price * closed;
                cash += credited_notional;
                sell_notional += credited_notional;
                realized_pnl += (fill.price - avg_cost) * closed;
                let excess = (fill.size - closed).max(0.0);
                if excess > f64::EPSILON {
                    invalid_fill_count += 1;
                    invalid_fill_notional += fill.price * excess;
                }
            }
        }
        if matches!(fill.maker_or_taker, MakerOrTaker::Maker) && rebate_factor > 0.0 {
            let rebate = notional * rebate_factor;
            cash += rebate;
            realized_pnl += rebate;
            maker_rebate_total += rebate;
        }
    }

    let (marks, raw_winner_asset_id) = replay_marks(events);
    let pair_asset_ids = binary_asset_ids(events, &positions);
    let winner_asset_id =
        resolve_winner_asset_id(raw_winner_asset_id.as_deref(), pair_asset_ids.as_deref());
    let merge_events = replay_merge_events(events);
    let redeem_events = replay_redeem_events(events);
    let pairable_qty_before_resolution = pair_asset_ids
        .as_ref()
        .map(|pair| pairable_qty(&positions, pair))
        .unwrap_or(0.0);
    let merge_apply = pair_asset_ids
        .as_ref()
        .map(|pair| apply_successful_merges(&mut positions, pair, &merge_events))
        .unwrap_or_default();
    cash += merge_apply.credit_usd;
    realized_pnl += merge_apply.realized_pnl_usd;
    let resolution_apply = winner_asset_id
        .as_deref()
        .map(|winner| apply_resolution_settlement(&mut positions, winner, &redeem_events))
        .unwrap_or_default();
    cash += resolution_apply.credit_usd;
    realized_pnl += resolution_apply.realized_pnl_usd;

    let mut market_value = 0.0;
    let mut open_cost_basis = 0.0;
    let mut unmarked_open_positions = 0u64;
    let mut open_positions = Vec::new();
    let settlement = compute_settlement_summary(
        pair_asset_ids.unwrap_or_default(),
        &merge_events,
        merge_apply.applied_qty,
        merge_apply.fee_usd,
        merge_apply.gas_usd,
        pairable_qty_before_resolution,
        &positions,
        winner_asset_id.as_deref(),
        &resolution_apply,
    );

    for (asset_id, pos) in positions {
        if pos.qty <= f64::EPSILON {
            continue;
        }
        let cost_basis = pos.qty * pos.avg_cost;
        let mark = if let Some(winner) = winner_asset_id.as_deref() {
            Some(if winner == asset_id { 1.0 } else { 0.0 })
        } else {
            marks.get(&asset_id).copied()
        };
        let value = mark.map(|p| p * pos.qty).unwrap_or(0.0);
        if mark.is_none() {
            unmarked_open_positions += 1;
        }
        market_value += value;
        open_cost_basis += cost_basis;
        open_positions.push(ReplayPositionSummary {
            asset_id,
            qty: pos.qty,
            avg_cost_usd: pos.avg_cost,
            cost_basis_usd: cost_basis,
            mark_price_usd: mark,
            market_value_usd: value,
            unrealized_pnl_usd: value - cost_basis,
        });
    }

    let ending_equity = cash + market_value;
    let unrealized_pnl = market_value - open_cost_basis;
    ReplayAccountingSummary {
        starting_cash_usd,
        ending_cash_usd: cash,
        market_value_usd: market_value,
        ending_equity_usd: ending_equity,
        total_pnl_usd: ending_equity - starting_cash_usd,
        realized_pnl_usd: realized_pnl,
        unrealized_pnl_usd: unrealized_pnl,
        fees_paid_usd: merge_apply.fee_usd
            + merge_apply.gas_usd
            + resolution_apply.fee_usd
            + resolution_apply.gas_usd,
        maker_rebate_usd: maker_rebate_total,
        gross_fill_notional_usd: buy_notional + sell_notional,
        buy_notional_usd: buy_notional,
        sell_notional_usd: sell_notional,
        invalid_fill_count,
        invalid_fill_notional_usd: invalid_fill_notional,
        redeemable_value_usd: resolution_apply.gross_credit_usd,
        resolution_winner_asset_id: winner_asset_id.clone(),
        mark_source: if winner_asset_id.is_some() {
            "resolution".to_string()
        } else {
            "last_replayed_market_price".to_string()
        },
        unmarked_open_positions,
        settlement,
        attribution: ReplayPnlAttributionSummary::default(),
        open_positions,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum AttributionPath {
    PairedMm,
    LateFavorite,
    CheapTailConvexity,
    Other,
}

impl AttributionPath {
    fn bucket_mut<'a>(
        self,
        summary: &'a mut ReplayPnlAttributionSummary,
    ) -> &'a mut ReplayPnlAttributionBucket {
        match self {
            Self::PairedMm => &mut summary.paired_mm,
            Self::LateFavorite => &mut summary.late_favorite_loading,
            Self::CheapTailConvexity => &mut summary.cheap_tail_convexity,
            Self::Other => &mut summary.other,
        }
    }
}

#[derive(Debug, Clone)]
struct AttributionLot {
    path: AttributionPath,
    qty: f64,
    avg_cost: f64,
}

fn compute_pnl_attribution(
    events: &[Event],
    fills: &[SimulatedFill],
    journal_events: &[JournalEvent],
    accounting: &ReplayAccountingSummary,
    fill_quality: FillQuality,
) -> ReplayPnlAttributionSummary {
    let mut summary = ReplayPnlAttributionSummary {
        fill_quality: fill_quality.as_str().to_string(),
        total_pnl_check_usd: accounting.total_pnl_usd,
        costs: ReplayCostAttribution {
            fees_paid_usd: accounting.fees_paid_usd,
            merge_fee_usd: accounting.settlement.merge_fee_usd,
            merge_gas_usd: accounting.settlement.merge_gas_usd,
            redeem_fee_usd: accounting.settlement.redeem_fee_usd,
            redeem_gas_usd: accounting.settlement.redeem_gas_usd,
            ..ReplayCostAttribution::default()
        },
        merge_redeem_recycling: ReplayMergeRedeemAttribution {
            merge_success_count: accounting.settlement.merge_success_count,
            merged_pair_qty: accounting.settlement.merged_pair_qty,
            merge_credit_usd: accounting.settlement.merge_credit_usd,
            redeemed_winning_qty: accounting.settlement.redeemed_winning_qty,
            expired_losing_qty: accounting.settlement.expired_losing_qty,
            redeem_credit_usd: accounting.settlement.redeem_credit_usd,
            ..ReplayMergeRedeemAttribution::default()
        },
        stranded_inventory_losses: ReplayStrandedInventoryAttribution {
            stranded_qty_total: accounting.settlement.stranded_qty_total,
            stranded_cost_usd: accounting.settlement.stranded_cost_usd,
            expired_losing_qty: accounting.settlement.expired_losing_qty,
            ..ReplayStrandedInventoryAttribution::default()
        },
        ..ReplayPnlAttributionSummary::default()
    };

    let intent_paths = classify_journal_intents(journal_events, &mut summary);
    let mut lots: BTreeMap<String, Vec<AttributionLot>> = BTreeMap::new();
    for fill in fills {
        let path = intent_paths
            .get(&fill.client_order_id)
            .copied()
            .unwrap_or_else(|| classify_tag(&fill.client_order_id));
        let bucket = path.bucket_mut(&mut summary);
        bucket.fill_count += 1;
        match fill.maker_or_taker {
            MakerOrTaker::Maker => bucket.maker_fills += 1,
            MakerOrTaker::Taker => bucket.taker_fills += 1,
        }
        let notional = fill.price * fill.size;
        bucket.gross_notional_usd += notional;
        match fill.side {
            Side::Buy => {
                bucket.buy_notional_usd += notional;
                lots.entry(fill.asset_id.clone())
                    .or_default()
                    .push(AttributionLot {
                        path,
                        qty: fill.size,
                        avg_cost: fill.price,
                    });
            }
            Side::Sell => {
                bucket.sell_notional_usd += notional;
                realize_sell_against_lots(
                    &mut lots,
                    &fill.asset_id,
                    fill.size,
                    fill.price,
                    &mut summary,
                );
            }
        }
    }

    let pair_asset_ids = binary_asset_ids_from_events_or_lots(events, &lots);
    let merge_events = replay_merge_events(events);
    if let Some(pair) = pair_asset_ids.as_ref() {
        let merge_pnl = attribute_successful_merges(&mut lots, pair, &merge_events, &mut summary);
        summary.merge_redeem_recycling.merge_realized_pnl_usd = merge_pnl;
    }

    let (marks, raw_winner_asset_id) = replay_marks(events);
    let winner_asset_id =
        resolve_winner_asset_id(raw_winner_asset_id.as_deref(), pair_asset_ids.as_deref());
    if let Some(winner) = winner_asset_id.as_deref() {
        let resolution_pnl = attribute_resolution(&mut lots, winner, &mut summary);
        summary.merge_redeem_recycling.redeem_resolution_pnl_usd = resolution_pnl;
    }

    attribute_unrealized_lots(&lots, winner_asset_id.as_deref(), &marks, &mut summary);
    finalize_attribution_totals(&mut summary);
    finalize_strategy_validation_status(&mut summary, accounting, journal_events);
    summary
}

fn classify_journal_intents(
    journal_events: &[JournalEvent],
    summary: &mut ReplayPnlAttributionSummary,
) -> BTreeMap<String, AttributionPath> {
    let mut out = BTreeMap::new();
    for event in journal_events {
        match event {
            JournalEvent::StrategyDecision {
                decision_type,
                reason_tag,
                ..
            } => {
                let path = classify_tag_pair(decision_type, reason_tag);
                path.bucket_mut(summary).decisions_count += 1;
            }
            JournalEvent::IntentSubmit {
                intent_id,
                reason_tag,
                ..
            } => {
                let path = classify_tag(reason_tag);
                path.bucket_mut(summary).submitted_orders += 1;
                out.insert(intent_id.clone(), path);
            }
            _ => {}
        }
    }
    out
}

fn classify_tag_pair(a: &str, b: &str) -> AttributionPath {
    let first = classify_tag(a);
    if first != AttributionPath::Other {
        return first;
    }
    classify_tag(b)
}

fn classify_tag(tag: &str) -> AttributionPath {
    let tag = tag.to_ascii_lowercase();
    // Order matters: late-favorite specializations of convex-accum must
    // be detected before the generic convex-accum cheap-tail bucket.
    if tag.contains("late-fav-tail") {
        AttributionPath::CheapTailConvexity
    } else if tag.contains("paired-mm-convex")
        || tag.contains("late-bar-core")
        || tag.contains("late-fav-climb")
        || tag.contains("late_favorite")
        || tag.contains("late-favorite")
        || tag.contains("late-fav")
        || tag.contains("late favorite")
        || tag.contains("favorite_loading")
        || tag.contains("favorite-loading")
    {
        AttributionPath::LateFavorite
    } else if tag.contains("paired-mm-tail")
        || tag.contains("ultra-cheap-tail")
        || tag.contains("cheap-tail")
        || tag.contains("cheap_tail")
        || tag.contains("cheap-leg")
        || tag.contains("mm-convex-accum")
    {
        AttributionPath::CheapTailConvexity
    } else if tag.contains("mm-paired-bid")
        || tag.contains("pairedentry")
        || tag.contains("paired_entry")
        || tag.contains("paired-mm:")
        || tag.contains("paired-mm-merge")
        || tag.contains("capital-recycle")
        || tag.contains("core-hedge")
        || tag.contains("core_hedge")
    {
        AttributionPath::PairedMm
    } else {
        AttributionPath::Other
    }
}

fn realize_sell_against_lots(
    lots: &mut BTreeMap<String, Vec<AttributionLot>>,
    asset_id: &str,
    mut qty: f64,
    sell_price: f64,
    summary: &mut ReplayPnlAttributionSummary,
) {
    let Some(asset_lots) = lots.get_mut(asset_id) else {
        return;
    };
    for lot in asset_lots.iter_mut() {
        if qty <= f64::EPSILON {
            break;
        }
        if lot.qty <= f64::EPSILON {
            continue;
        }
        let take = qty.min(lot.qty);
        let pnl = (sell_price - lot.avg_cost) * take;
        lot.path.bucket_mut(summary).realized_pnl_usd += pnl;
        lot.qty -= take;
        qty -= take;
    }
    asset_lots.retain(|lot| lot.qty > f64::EPSILON);
}

fn attribute_successful_merges(
    lots: &mut BTreeMap<String, Vec<AttributionLot>>,
    pair_asset_ids: &[String],
    merge_events: &[ReplayMergeEvent],
    summary: &mut ReplayPnlAttributionSummary,
) -> f64 {
    if pair_asset_ids.len() != 2 {
        return 0.0;
    }
    let requested_qty: f64 = merge_events
        .iter()
        .filter(|event| event.status == ReplayMergeStatus::Success)
        .map(|event| event.size)
        .sum();
    let applied_qty = requested_qty.min(pairable_qty_from_lots(lots, pair_asset_ids));
    if applied_qty <= f64::EPSILON {
        return 0.0;
    }
    let mut total_pnl = 0.0;
    for asset_id in pair_asset_ids {
        total_pnl += consume_lots_with_terminal_value(lots, asset_id, applied_qty, 0.5, summary);
    }
    total_pnl
}

fn attribute_resolution(
    lots: &mut BTreeMap<String, Vec<AttributionLot>>,
    winner_asset_id: &str,
    summary: &mut ReplayPnlAttributionSummary,
) -> f64 {
    let mut total_pnl = 0.0;
    let asset_ids = lots.keys().cloned().collect::<Vec<_>>();
    for asset_id in asset_ids {
        let terminal_value = if asset_id == winner_asset_id {
            1.0
        } else {
            0.0
        };
        let qty = lots
            .get(&asset_id)
            .map(|asset_lots| asset_lots.iter().map(|lot| lot.qty.max(0.0)).sum())
            .unwrap_or(0.0);
        if terminal_value == 0.0 {
            let losing_cost = lots
                .get(&asset_id)
                .map(|asset_lots| {
                    asset_lots
                        .iter()
                        .map(|lot| lot.qty.max(0.0) * lot.avg_cost)
                        .sum::<f64>()
                })
                .unwrap_or(0.0);
            summary.stranded_inventory_losses.expired_losing_cost_usd += losing_cost;
            // Bucket the losing-side cost basis per path so callers can read
            // "we spent $X on losing late-convex bets" directly.
            if let Some(asset_lots) = lots.get(&asset_id) {
                let mut per_path: BTreeMap<AttributionPath, (f64, f64)> = BTreeMap::new();
                for lot in asset_lots {
                    if lot.qty <= f64::EPSILON {
                        continue;
                    }
                    let entry = per_path.entry(lot.path).or_insert((0.0, 0.0));
                    entry.0 += lot.qty;
                    entry.1 += lot.qty * lot.avg_cost;
                }
                for (path, (path_qty, path_cost)) in per_path {
                    let bucket = path.bucket_mut(summary);
                    bucket.expired_losing_qty += path_qty;
                    bucket.expired_losing_cost_usd += path_cost;
                }
            }
        }
        total_pnl +=
            consume_lots_with_terminal_value(lots, &asset_id, qty, terminal_value, summary);
    }
    total_pnl
}

fn consume_lots_with_terminal_value(
    lots: &mut BTreeMap<String, Vec<AttributionLot>>,
    asset_id: &str,
    mut qty: f64,
    terminal_value: f64,
    summary: &mut ReplayPnlAttributionSummary,
) -> f64 {
    let Some(asset_lots) = lots.get_mut(asset_id) else {
        return 0.0;
    };
    let mut total_pnl = 0.0;
    for lot in asset_lots.iter_mut() {
        if qty <= f64::EPSILON {
            break;
        }
        if lot.qty <= f64::EPSILON {
            continue;
        }
        let take = qty.min(lot.qty);
        let pnl = (terminal_value - lot.avg_cost) * take;
        lot.path.bucket_mut(summary).realized_pnl_usd += pnl;
        total_pnl += pnl;
        lot.qty -= take;
        qty -= take;
    }
    asset_lots.retain(|lot| lot.qty > f64::EPSILON);
    total_pnl
}

fn attribute_unrealized_lots(
    lots: &BTreeMap<String, Vec<AttributionLot>>,
    winner_asset_id: Option<&str>,
    marks: &BTreeMap<String, f64>,
    summary: &mut ReplayPnlAttributionSummary,
) {
    for (asset_id, asset_lots) in lots {
        let mark = if let Some(winner) = winner_asset_id {
            Some(if winner == asset_id { 1.0 } else { 0.0 })
        } else {
            marks.get(asset_id).copied()
        };
        for lot in asset_lots {
            if lot.qty <= f64::EPSILON {
                continue;
            }
            let cost = lot.qty * lot.avg_cost;
            let value = mark.map(|price| price * lot.qty).unwrap_or(0.0);
            let pnl = value - cost;
            let bucket = lot.path.bucket_mut(summary);
            bucket.unrealized_pnl_usd += pnl;
            // Stranded = inventory still in lots after merges + resolution
            // settled what they could. Bucket cost basis per path so callers
            // can split MM imbalance from intentional directional bets.
            bucket.stranded_qty += lot.qty;
            bucket.stranded_cost_usd += cost;
            if mark.is_none() {
                summary.stranded_inventory_losses.unresolved_cost_usd += cost;
            }
            summary.stranded_inventory_losses.unrealized_pnl_usd += pnl;
        }
    }
}

fn finalize_attribution_totals(summary: &mut ReplayPnlAttributionSummary) {
    for bucket in [
        &mut summary.paired_mm,
        &mut summary.late_favorite_loading,
        &mut summary.cheap_tail_convexity,
        &mut summary.other,
    ] {
        bucket.total_pnl_usd = bucket.realized_pnl_usd + bucket.unrealized_pnl_usd;
    }
    summary.attributed_path_pnl_usd = summary.paired_mm.total_pnl_usd
        + summary.late_favorite_loading.total_pnl_usd
        + summary.cheap_tail_convexity.total_pnl_usd
        + summary.other.total_pnl_usd
        - summary.costs.fees_paid_usd;
    summary.unattributed_pnl_usd = summary.total_pnl_check_usd - summary.attributed_path_pnl_usd;
}

fn finalize_strategy_validation_status(
    summary: &mut ReplayPnlAttributionSummary,
    accounting: &ReplayAccountingSummary,
    journal_events: &[JournalEvent],
) {
    let submitted_orders = summary.paired_mm.submitted_orders
        + summary.late_favorite_loading.submitted_orders
        + summary.cheap_tail_convexity.submitted_orders
        + summary.other.submitted_orders;
    let decision_events = journal_events
        .iter()
        .filter(|event| matches!(event, JournalEvent::StrategyDecision { .. }))
        .count() as u64;
    let fill_count = summary.paired_mm.fill_count
        + summary.late_favorite_loading.fill_count
        + summary.cheap_tail_convexity.fill_count
        + summary.other.fill_count;
    let mut reasons = Vec::new();
    if submitted_orders == 0 {
        reasons.push("no_intents_submitted".to_string());
    }
    if decision_events == 0 {
        reasons.push("no_strategy_decisions_journaled".to_string());
    }
    if fill_count == 0 {
        reasons.push("no_accepted_fills".to_string());
    }
    if accounting.settlement.status == "no_binary_pair_detected" {
        reasons.push("no_binary_pair_detected".to_string());
    }
    summary.strategy_validation_status = if reasons.is_empty() {
        "valid_strategy_replay".to_string()
    } else {
        "invalid_non_strategy_validation".to_string()
    };
    summary.invalid_reasons = reasons;
}

fn pairable_qty_from_lots(
    lots: &BTreeMap<String, Vec<AttributionLot>>,
    pair_asset_ids: &[String],
) -> f64 {
    if pair_asset_ids.len() != 2 {
        return 0.0;
    }
    pair_asset_ids
        .iter()
        .map(|asset_id| {
            lots.get(asset_id)
                .map(|asset_lots| asset_lots.iter().map(|lot| lot.qty.max(0.0)).sum::<f64>())
                .unwrap_or(0.0)
        })
        .fold(f64::INFINITY, f64::min)
}

fn binary_asset_ids_from_events_or_lots(
    events: &[Event],
    lots: &BTreeMap<String, Vec<AttributionLot>>,
) -> Option<Vec<String>> {
    for event in events {
        let Some(values) = event.raw.get("asset_ids").and_then(|v| v.as_array()) else {
            continue;
        };
        let ids: Vec<String> = values
            .iter()
            .filter_map(|v| v.as_str())
            .filter(|v| !v.is_empty())
            .map(ToString::to_string)
            .collect();
        if ids.len() == 2 {
            return Some(ids);
        }
    }
    let ids = lots
        .iter()
        .filter_map(|(asset_id, asset_lots)| {
            let qty: f64 = asset_lots.iter().map(|lot| lot.qty.max(0.0)).sum();
            (qty > f64::EPSILON).then_some(asset_id.clone())
        })
        .collect::<Vec<_>>();
    (ids.len() == 2).then_some(ids)
}

#[derive(Debug, Clone, Default)]
struct ReplayMergeEvent {
    size: f64,
    status: ReplayMergeStatus,
    fee_usd: f64,
    gas_usd: f64,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
enum ReplayMergeStatus {
    Success,
    Reverted,
    #[default]
    PendingOrUnknown,
}

#[derive(Debug, Clone, Copy, Default)]
struct AppliedMergeSummary {
    applied_qty: f64,
    credit_usd: f64,
    fee_usd: f64,
    gas_usd: f64,
    realized_pnl_usd: f64,
}

#[derive(Debug, Clone, Copy, Default)]
struct ReplaySettlementFeeEvent {
    fee_usd: f64,
    gas_usd: f64,
}

#[derive(Debug, Clone, Copy, Default)]
struct AppliedResolutionSummary {
    winning_qty: f64,
    losing_qty: f64,
    winning_cost_usd: f64,
    losing_cost_usd: f64,
    gross_credit_usd: f64,
    credit_usd: f64,
    fee_usd: f64,
    gas_usd: f64,
    realized_pnl_usd: f64,
}

fn binary_asset_ids(
    events: &[Event],
    positions: &BTreeMap<String, ReplayPositionAccounting>,
) -> Option<Vec<String>> {
    for event in events {
        let Some(values) = event.raw.get("asset_ids").and_then(|v| v.as_array()) else {
            continue;
        };
        let ids: Vec<String> = values
            .iter()
            .filter_map(|v| v.as_str())
            .filter(|v| !v.is_empty())
            .map(ToString::to_string)
            .collect();
        if ids.len() == 2 {
            return Some(ids);
        }
    }
    let ids: Vec<String> = positions
        .iter()
        .filter_map(|(asset_id, pos)| (pos.qty > f64::EPSILON).then_some(asset_id.clone()))
        .collect();
    (ids.len() == 2).then_some(ids)
}

fn pairable_qty(
    positions: &BTreeMap<String, ReplayPositionAccounting>,
    pair_asset_ids: &[String],
) -> f64 {
    if pair_asset_ids.len() != 2 {
        return 0.0;
    }
    let a = positions
        .get(&pair_asset_ids[0])
        .map(|p| p.qty.max(0.0))
        .unwrap_or(0.0);
    let b = positions
        .get(&pair_asset_ids[1])
        .map(|p| p.qty.max(0.0))
        .unwrap_or(0.0);
    a.min(b)
}

fn apply_successful_merges(
    positions: &mut BTreeMap<String, ReplayPositionAccounting>,
    pair_asset_ids: &[String],
    merge_events: &[ReplayMergeEvent],
) -> AppliedMergeSummary {
    if pair_asset_ids.len() != 2 {
        return AppliedMergeSummary::default();
    }
    let requested_qty: f64 = merge_events
        .iter()
        .filter(|event| event.status == ReplayMergeStatus::Success)
        .map(|event| event.size)
        .sum();
    if requested_qty <= f64::EPSILON {
        return AppliedMergeSummary::default();
    }
    let applied_qty = requested_qty.min(pairable_qty(positions, pair_asset_ids));
    if applied_qty <= f64::EPSILON {
        return AppliedMergeSummary::default();
    }
    let avg_a = positions
        .get(&pair_asset_ids[0])
        .map(|p| p.avg_cost)
        .unwrap_or(0.0);
    let avg_b = positions
        .get(&pair_asset_ids[1])
        .map(|p| p.avg_cost)
        .unwrap_or(0.0);
    let fee_usd: f64 = merge_events
        .iter()
        .filter(|event| event.status == ReplayMergeStatus::Success)
        .map(|event| event.fee_usd.max(0.0))
        .sum();
    let gas_usd: f64 = merge_events
        .iter()
        .filter(|event| event.status == ReplayMergeStatus::Success)
        .map(|event| event.gas_usd.max(0.0))
        .sum();
    for asset_id in pair_asset_ids {
        if let Some(pos) = positions.get_mut(asset_id) {
            pos.sell(applied_qty);
        }
    }
    let gross_credit_usd = applied_qty;
    let credit_usd = gross_credit_usd - fee_usd - gas_usd;
    AppliedMergeSummary {
        applied_qty,
        credit_usd,
        fee_usd,
        gas_usd,
        realized_pnl_usd: credit_usd - applied_qty * (avg_a + avg_b),
    }
}

fn apply_resolution_settlement(
    positions: &mut BTreeMap<String, ReplayPositionAccounting>,
    winner_asset_id: &str,
    redeem_events: &[ReplaySettlementFeeEvent],
) -> AppliedResolutionSummary {
    let fee_usd: f64 = redeem_events
        .iter()
        .map(|event| event.fee_usd.max(0.0))
        .sum();
    let gas_usd: f64 = redeem_events
        .iter()
        .map(|event| event.gas_usd.max(0.0))
        .sum();
    let mut out = AppliedResolutionSummary {
        fee_usd,
        gas_usd,
        ..AppliedResolutionSummary::default()
    };
    let mut asset_ids = positions.keys().cloned().collect::<Vec<_>>();
    for asset_id in asset_ids.drain(..) {
        let Some(pos) = positions.get_mut(&asset_id) else {
            continue;
        };
        if pos.qty <= f64::EPSILON {
            continue;
        }
        let qty = pos.qty;
        let cost = pos.qty * pos.avg_cost;
        if asset_id == winner_asset_id {
            out.winning_qty += qty;
            out.winning_cost_usd += cost;
            out.gross_credit_usd += qty;
        } else {
            out.losing_qty += qty;
            out.losing_cost_usd += cost;
        }
        pos.sell(qty);
    }
    positions.retain(|_, pos| pos.qty > f64::EPSILON);
    out.credit_usd = out.gross_credit_usd - out.fee_usd - out.gas_usd;
    out.realized_pnl_usd = out.credit_usd - out.winning_cost_usd - out.losing_cost_usd;
    out
}

fn compute_settlement_summary(
    pair_asset_ids: Vec<String>,
    merge_events: &[ReplayMergeEvent],
    merged_pair_qty: f64,
    merge_fee_usd: f64,
    merge_gas_usd: f64,
    pairable_qty_before_resolution: f64,
    positions: &BTreeMap<String, ReplayPositionAccounting>,
    winner_asset_id: Option<&str>,
    resolution_apply: &AppliedResolutionSummary,
) -> ReplaySettlementSummary {
    let unmerged_pairable_qty = if pair_asset_ids.len() == 2 {
        pairable_qty(positions, &pair_asset_ids)
    } else {
        0.0
    };
    let (unmerged_pairable_cost_usd, unmerged_pairable_merge_value_usd) =
        pairable_cost_and_value(positions, &pair_asset_ids, unmerged_pairable_qty);
    let unmerged_pairable_net_gain_usd =
        unmerged_pairable_merge_value_usd - unmerged_pairable_cost_usd;
    let mut stranded_inventory = Vec::new();
    let mut stranded_qty_total = 0.0;
    let mut stranded_cost_usd = 0.0;

    for (asset_id, pos) in positions {
        if pos.qty <= f64::EPSILON {
            continue;
        }
        let cost_basis = pos.qty * pos.avg_cost;
        let role = match winner_asset_id {
            Some(winner) if winner == asset_id => "winner",
            Some(_) => "loser",
            None => "unresolved",
        };
        stranded_qty_total += pos.qty;
        stranded_cost_usd += cost_basis;
        stranded_inventory.push(ReplaySettlementAssetInventory {
            asset_id: asset_id.clone(),
            qty: pos.qty,
            cost_basis_usd: cost_basis,
            role: role.to_string(),
        });
    }

    let merge_attempted_count = merge_events.len() as u64;
    let merge_success_count = merge_events
        .iter()
        .filter(|event| event.status == ReplayMergeStatus::Success)
        .count() as u64;
    let merge_reverted_count = merge_events
        .iter()
        .filter(|event| event.status == ReplayMergeStatus::Reverted)
        .count() as u64;
    let (status, status_reason) = if pair_asset_ids.len() != 2 {
        (
            "no_binary_pair_detected",
            "no YES/NO pair asset ids found in market metadata or open positions".to_string(),
        )
    } else if winner_asset_id.is_some() {
        (
            "resolved_settled",
            "resolution event present; open inventory settled by winning leg".to_string(),
        )
    } else if pairable_qty_before_resolution <= f64::EPSILON && stranded_qty_total <= f64::EPSILON {
        (
            "flat",
            "no open inventory and no pairable inventory".to_string(),
        )
    } else if merge_reverted_count > 0 {
        (
            "merge_reverted",
            format!("{merge_reverted_count} merge event(s) reverted"),
        )
    } else if pairable_qty_before_resolution > f64::EPSILON && merged_pair_qty <= f64::EPSILON {
        if unmerged_pairable_net_gain_usd < -1e-9 {
            (
                "merge_would_crystallize_loss",
                format!(
                    "unmerged pairable qty {:.4} would merge for {:.4} against cost {:.4}, net_gain {:.4}",
                    unmerged_pairable_qty,
                    unmerged_pairable_merge_value_usd,
                    unmerged_pairable_cost_usd,
                    unmerged_pairable_net_gain_usd
                ),
            )
        } else {
            (
                "profitable_merge_opportunity_unexecuted",
                format!(
                    "unmerged pairable qty {:.4} has non-negative merge net_gain {:.4} but no merge event was observed",
                    unmerged_pairable_qty,
                    unmerged_pairable_net_gain_usd
                ),
            )
        }
    } else if unmerged_pairable_qty > f64::EPSILON {
        (
            "partially_merged",
            format!(
                "merged {:.4} pair(s) but {:.4} pairable qty remains with net_gain {:.4}",
                merged_pair_qty, unmerged_pairable_qty, unmerged_pairable_net_gain_usd
            ),
        )
    } else if merged_pair_qty > f64::EPSILON {
        ("merged", format!("merged {:.4} pair(s)", merged_pair_qty))
    } else {
        (
            "unpaired_inventory",
            "open inventory exists but no pairable quantity is available".to_string(),
        )
    };

    ReplaySettlementSummary {
        pair_asset_ids,
        merge_attempted_count,
        merge_success_count,
        merge_reverted_count,
        merged_pair_qty,
        merge_credit_usd: merged_pair_qty,
        merge_fee_usd,
        merge_gas_usd,
        pairable_qty_before_resolution,
        unmerged_pairable_qty,
        unmerged_pairable_cost_usd,
        unmerged_pairable_merge_value_usd,
        unmerged_pairable_net_gain_usd,
        redeemed_winning_qty: resolution_apply.winning_qty,
        expired_losing_qty: resolution_apply.losing_qty,
        redeem_credit_usd: resolution_apply.gross_credit_usd,
        redeem_fee_usd: resolution_apply.fee_usd,
        redeem_gas_usd: resolution_apply.gas_usd,
        stranded_qty_total,
        stranded_cost_usd,
        stranded_inventory,
        status: status.to_string(),
        status_reason,
    }
}

fn pairable_cost_and_value(
    positions: &BTreeMap<String, ReplayPositionAccounting>,
    pair_asset_ids: &[String],
    pairable_qty: f64,
) -> (f64, f64) {
    if pair_asset_ids.len() != 2 || pairable_qty <= f64::EPSILON {
        return (0.0, 0.0);
    }
    let avg_a = positions
        .get(&pair_asset_ids[0])
        .map(|p| p.avg_cost)
        .unwrap_or(0.0);
    let avg_b = positions
        .get(&pair_asset_ids[1])
        .map(|p| p.avg_cost)
        .unwrap_or(0.0);
    (pairable_qty * (avg_a + avg_b), pairable_qty)
}

fn replay_merge_events(events: &[Event]) -> Vec<ReplayMergeEvent> {
    events
        .iter()
        .filter(|event| raw_kind_is(event, "merge"))
        .map(|event| ReplayMergeEvent {
            size: parse_event_size(event).unwrap_or(0.0),
            status: parse_merge_status(event),
            fee_usd: parse_fee_usd(event),
            gas_usd: parse_gas_usd(event),
        })
        .collect()
}

fn replay_redeem_events(events: &[Event]) -> Vec<ReplaySettlementFeeEvent> {
    events
        .iter()
        .filter(|event| event.event_type == EventType::Resolution || raw_kind_is(event, "redeem"))
        .map(|event| ReplaySettlementFeeEvent {
            fee_usd: parse_fee_usd(event),
            gas_usd: parse_gas_usd(event),
        })
        .filter(|event| event.fee_usd > 0.0 || event.gas_usd > 0.0)
        .collect()
}

fn raw_kind_is(event: &Event, expected: &str) -> bool {
    ["type", "event_type", "action", "kind"]
        .iter()
        .filter_map(|key| event.raw.get(*key).and_then(|v| v.as_str()))
        .any(|value| value.eq_ignore_ascii_case(expected))
}

fn parse_event_size(event: &Event) -> Option<f64> {
    event
        .size
        .as_deref()
        .and_then(|value| parse_price(Some(value)))
        .or_else(|| parse_raw_f64(&event.raw, &["size", "amount", "qty", "shares"]))
}

fn parse_raw_f64(raw: &serde_json::Value, keys: &[&str]) -> Option<f64> {
    for key in keys {
        let Some(value) = raw.get(*key) else {
            continue;
        };
        if let Some(number) = value.as_f64() {
            if number.is_finite() {
                return Some(number);
            }
        }
        if let Some(text) = value.as_str().and_then(|value| parse_price(Some(value))) {
            return Some(text);
        }
    }
    None
}

fn parse_fee_usd(event: &Event) -> f64 {
    parse_raw_f64(
        &event.raw,
        &[
            "fee_usd",
            "fees_usd",
            "merge_fee_usd",
            "redeem_fee_usd",
            "settlement_fee_usd",
            "tx_fee_usd",
        ],
    )
    .unwrap_or(0.0)
    .max(0.0)
}

fn parse_gas_usd(event: &Event) -> f64 {
    parse_raw_f64(
        &event.raw,
        &[
            "gas_usd",
            "gas_fee_usd",
            "merge_gas_usd",
            "redeem_gas_usd",
            "settlement_gas_usd",
        ],
    )
    .unwrap_or(0.0)
    .max(0.0)
}

fn parse_merge_status(event: &Event) -> ReplayMergeStatus {
    let status = ["status", "state", "tx_status"]
        .iter()
        .filter_map(|key| event.raw.get(*key).and_then(|v| v.as_str()))
        .next()
        .unwrap_or_default()
        .to_ascii_lowercase();
    match status.as_str() {
        "confirmed" | "mined" | "success" | "succeeded" | "settled" => ReplayMergeStatus::Success,
        "reverted" | "failed" | "failure" | "error" => ReplayMergeStatus::Reverted,
        _ => ReplayMergeStatus::PendingOrUnknown,
    }
}

fn compute_queue_calibration(
    submissions: &[SimulatedOrderSubmission],
    fills: &[SimulatedFill],
) -> ReplayQueueCalibrationSummary {
    let mut filled_by_order: BTreeMap<String, (f64, u64)> = BTreeMap::new();
    for fill in fills {
        let entry = filled_by_order
            .entry(fill.client_order_id.clone())
            .or_insert((0.0, fill.fill_ms));
        entry.0 += fill.size;
        if fill.fill_ms < entry.1 {
            entry.1 = fill.fill_ms;
        }
    }

    let mut seconds_to_first_fill = Vec::new();
    let mut same_side_depth = Vec::new();
    let mut queue_position_fraction = Vec::new();
    let mut filled_orders = 0u64;
    let mut partial_fills = 0u64;

    for submission in submissions {
        same_side_depth.push(submission.book_depth_at_rest);
        if submission.size > 0.0 {
            queue_position_fraction
                .push((submission.book_depth_at_rest / submission.size).clamp(0.0, 1.0));
        }
        let (filled_size, first_fill_ms) = filled_by_order
            .get(&submission.client_order_id)
            .copied()
            .unwrap_or((0.0, 0));
        if filled_size > 0.0 {
            filled_orders += 1;
            if filled_size + f64::EPSILON < submission.size {
                partial_fills += 1;
            }
            if first_fill_ms >= submission.arrival_ms {
                seconds_to_first_fill
                    .push((first_fill_ms - submission.arrival_ms) as f64 / 1_000.0);
            }
        }
    }

    let sample_size = submissions.len() as u64;
    ReplayQueueCalibrationSummary {
        sample_size,
        filled_orders,
        fill_rate: if sample_size > 0 {
            filled_orders as f64 / sample_size as f64
        } else {
            0.0
        },
        partial_fill_rate: if filled_orders > 0 {
            partial_fills as f64 / filled_orders as f64
        } else {
            0.0
        },
        median_seconds_to_first_fill: median_f64(seconds_to_first_fill),
        median_same_side_depth_at_entry: median_f64(same_side_depth),
        median_estimated_queue_position_fraction: median_f64(queue_position_fraction),
    }
}

fn median_f64(mut values: Vec<f64>) -> Option<f64> {
    values.retain(|v| v.is_finite());
    if values.is_empty() {
        return None;
    }
    values.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let mid = values.len() / 2;
    if values.len() % 2 == 0 {
        Some((values[mid - 1] + values[mid]) / 2.0)
    } else {
        Some(values[mid])
    }
}

fn replay_marks(events: &[Event]) -> (BTreeMap<String, f64>, Option<String>) {
    let mut marks = BTreeMap::new();
    let mut winner_asset_id = None;
    for event in events {
        if matches!(
            event.event_type,
            EventType::BookSnapshot | EventType::BookDelta | EventType::Trade
        ) {
            if let (Some(asset_id), Some(price)) = (
                event.asset_id.as_deref(),
                parse_price(event.price.as_deref()),
            ) {
                marks.insert(asset_id.to_string(), price);
            }
        }
        if event.event_type == EventType::Resolution {
            winner_asset_id = resolution_winner_asset_id(event).or_else(|| event.asset_id.clone());
        }
    }
    (marks, winner_asset_id)
}

fn resolve_winner_asset_id(
    raw_winner: Option<&str>,
    pair_asset_ids: Option<&[String]>,
) -> Option<String> {
    let raw_winner = raw_winner?.trim();
    if raw_winner.is_empty() {
        return None;
    }
    if let Some(pair) = pair_asset_ids {
        if pair.iter().any(|asset_id| asset_id == raw_winner) {
            return Some(raw_winner.to_string());
        }
        if pair.len() == 2 {
            let normalized = raw_winner.to_ascii_lowercase();
            if matches!(normalized.as_str(), "yes" | "up" | "home" | "1") {
                return Some(pair[0].clone());
            }
            if matches!(normalized.as_str(), "no" | "down" | "away" | "0") {
                return Some(pair[1].clone());
            }
        }
    }
    Some(raw_winner.to_string())
}

fn parse_price(value: Option<&str>) -> Option<f64> {
    let parsed = value?.parse::<f64>().ok()?;
    parsed.is_finite().then_some(parsed)
}

fn resolution_winner_asset_id(event: &Event) -> Option<String> {
    for key in [
        "winner_asset_id",
        "winning_asset_id",
        "resolved_asset_id",
        "asset_id",
        "winning_outcome",
        "winner_outcome",
        "outcome",
    ] {
        if let Some(value) = event.raw.get(key).and_then(|v| v.as_str()) {
            if !value.is_empty() {
                return Some(value.to_string());
            }
        }
    }
    None
}

/// Dispatch a single `event` (real or synthetic) through the strategy
/// and simulator. Hoisted out of `run_window` so the synthesizer's
/// derived events flow through the same code path as real ones.
fn dispatch_event<S: ReplayStrategy>(
    strategy: &mut S,
    sim: &mut FillSimulator,
    event: &Event,
    intents_submitted: &mut u64,
    risk_rejections: &mut Vec<RiskRejection>,
    journal_events: &mut Vec<JournalEvent>,
    capture_journal: bool,
    accepted_fills: &mut Vec<SimulatedFill>,
    intent_remaining: &mut BTreeMap<String, f64>,
    accounting_events: &mut Vec<Event>,
    queue_assumption: &str,
) {
    let event_ms = (event.received_ns / 1_000_000) as u64;
    let fills_before_event = sim.fills().len();
    sim.on_event(event);
    process_new_simulated_fills(
        strategy,
        sim,
        fills_before_event,
        event_ms,
        intents_submitted,
        risk_rejections,
        journal_events,
        capture_journal,
        accepted_fills,
        intent_remaining,
        accounting_events,
        queue_assumption,
    );

    let mut decision = strategy.on_event(event);
    risk_rejections.append(&mut decision.risk_rejections);
    if capture_journal {
        journal_events.append(&mut decision.journal_events);
    }
    accounting_events.append(&mut decision.accounting_events);
    for coid in decision.cancels {
        sim.cancel(&coid, event_ms);
    }
    submit_replay_intents(
        strategy,
        sim,
        decision.submits,
        event_ms,
        intents_submitted,
        risk_rejections,
        journal_events,
        capture_journal,
        accepted_fills,
        intent_remaining,
        accounting_events,
        queue_assumption,
    );
}

#[allow(clippy::too_many_arguments)]
fn submit_replay_intents<S: ReplayStrategy>(
    strategy: &mut S,
    sim: &mut FillSimulator,
    submits: Vec<StrategyOrderIntent>,
    event_ms: u64,
    intents_submitted: &mut u64,
    risk_rejections: &mut Vec<RiskRejection>,
    journal_events: &mut Vec<JournalEvent>,
    capture_journal: bool,
    accepted_fills: &mut Vec<SimulatedFill>,
    intent_remaining: &mut BTreeMap<String, f64>,
    accounting_events: &mut Vec<Event>,
    queue_assumption: &str,
) {
    if submits.is_empty() {
        return;
    }
    let mut expire_ioc = Vec::new();
    let fills_before_submit = sim.fills().len();
    for intent in submits {
        if intent.aggressive {
            expire_ioc.push(intent.client_order_id.clone());
        }
        intent_remaining.insert(intent.client_order_id.clone(), intent.size);
        sim.submit(intent);
        *intents_submitted += 1;
    }
    process_new_simulated_fills(
        strategy,
        sim,
        fills_before_submit,
        event_ms,
        intents_submitted,
        risk_rejections,
        journal_events,
        capture_journal,
        accepted_fills,
        intent_remaining,
        accounting_events,
        queue_assumption,
    );
    for client_order_id in expire_ioc {
        let mut decision = strategy.on_ioc_expired(&client_order_id, event_ms);
        risk_rejections.append(&mut decision.risk_rejections);
        if capture_journal {
            journal_events.append(&mut decision.journal_events);
        }
        accounting_events.append(&mut decision.accounting_events);
        for coid in decision.cancels {
            sim.cancel(&coid, event_ms);
        }
        submit_replay_intents(
            strategy,
            sim,
            decision.submits,
            event_ms,
            intents_submitted,
            risk_rejections,
            journal_events,
            capture_journal,
            accepted_fills,
            intent_remaining,
            accounting_events,
            queue_assumption,
        );
    }
}

#[allow(clippy::too_many_arguments)]
fn process_new_simulated_fills<S: ReplayStrategy>(
    strategy: &mut S,
    sim: &mut FillSimulator,
    fills_before: usize,
    event_ms: u64,
    intents_submitted: &mut u64,
    risk_rejections: &mut Vec<RiskRejection>,
    journal_events: &mut Vec<JournalEvent>,
    capture_journal: bool,
    accepted_fills: &mut Vec<SimulatedFill>,
    intent_remaining: &mut BTreeMap<String, f64>,
    accounting_events: &mut Vec<Event>,
    queue_assumption: &str,
) {
    let fills_after = sim.fills().len();
    #[allow(clippy::unnecessary_to_owned)]
    let new_fills = sim.fills()[fills_before..fills_after].to_vec();
    for fill in new_fills {
        let mut decision = strategy.on_fill(&fill);
        let rejected = decision
            .rejected_fills
            .iter()
            .any(|client_order_id| client_order_id == &fill.client_order_id);
        risk_rejections.append(&mut decision.risk_rejections);
        if capture_journal {
            journal_events.append(&mut decision.journal_events);
        }
        accounting_events.append(&mut decision.accounting_events);
        if !rejected {
            if capture_journal {
                emit_fill_journal_row(&fill, intent_remaining, queue_assumption, journal_events);
            }
            accepted_fills.push(fill.clone());
        }
        for coid in decision.cancels {
            sim.cancel(&coid, event_ms);
        }
        submit_replay_intents(
            strategy,
            sim,
            decision.submits,
            event_ms,
            intents_submitted,
            risk_rejections,
            journal_events,
            capture_journal,
            accepted_fills,
            intent_remaining,
            accounting_events,
            queue_assumption,
        );
    }
}

fn emit_fill_journal_row(
    fill: &SimulatedFill,
    intent_remaining: &mut BTreeMap<String, f64>,
    queue_assumption: &str,
    journal_events: &mut Vec<JournalEvent>,
) {
    let ts_ns = (fill.fill_ms as i64).saturating_mul(1_000_000);
    let prior = intent_remaining
        .get(&fill.client_order_id)
        .copied()
        .unwrap_or(fill.size);
    let remaining = (prior - fill.size).max(0.0);
    if remaining > f64::EPSILON {
        intent_remaining.insert(fill.client_order_id.clone(), remaining);
        journal_events.push(JournalEvent::PartialFill {
            ts_ns,
            intent_id: fill.client_order_id.clone(),
            fill_qty: fill.size,
            fill_price: fill.price,
            remaining_qty: remaining,
        });
    } else {
        intent_remaining.remove(&fill.client_order_id);
        journal_events.push(JournalEvent::Fill {
            ts_ns,
            intent_id: fill.client_order_id.clone(),
            fill_qty: fill.size,
            fill_price: fill.price,
            queue_assumption: queue_assumption.to_string(),
            was_simulated_fill: true,
        });
    }
}

/// Inspect a raw `Event` and emit `merge_event` / `redeem_event` /
/// `accounting_event` journal rows where applicable. The Polymarket user
/// websocket emits merge/redeem actions inside `EventType::UserOrder` /
/// `Resolution`; the schema mirrors what the live runtime sees.
fn emit_market_event_journal_rows(event: &Event, journal_events: &mut Vec<JournalEvent>) {
    let market_slug = match event.market_slug.as_deref() {
        Some(slug) => slug.to_string(),
        None => return,
    };
    if raw_kind_is(event, "merge") {
        let size = parse_event_size(event).unwrap_or(0.0);
        let fee = parse_fee_usd(event);
        let gas = parse_gas_usd(event);
        let credit = size - fee - gas;
        journal_events.push(JournalEvent::MergeEvent {
            ts_ns: event.received_ns,
            market_slug: market_slug.clone(),
            qty_yes_burned: size,
            qty_no_burned: size,
            usd_credited: credit,
        });
        if fee > 0.0 {
            journal_events.push(JournalEvent::AccountingEvent {
                ts_ns: event.received_ns,
                kind: "fee".to_string(),
                market_slug: Some(market_slug.clone()),
                asset_id: None,
                usd_amount: fee,
            });
        }
        if gas > 0.0 {
            journal_events.push(JournalEvent::AccountingEvent {
                ts_ns: event.received_ns,
                kind: "gas".to_string(),
                market_slug: Some(market_slug),
                asset_id: None,
                usd_amount: gas,
            });
        }
        return;
    }
    if event.event_type == EventType::Resolution || raw_kind_is(event, "redeem") {
        let asset_id = event.asset_id.clone().unwrap_or_default();
        let qty = parse_event_size(event).unwrap_or(0.0);
        let credit = qty;
        journal_events.push(JournalEvent::RedeemEvent {
            ts_ns: event.received_ns,
            market_slug: market_slug.clone(),
            asset_id,
            qty_redeemed: qty,
            usd_credited: credit,
        });
        let fee = parse_fee_usd(event);
        let gas = parse_gas_usd(event);
        if fee > 0.0 {
            journal_events.push(JournalEvent::AccountingEvent {
                ts_ns: event.received_ns,
                kind: "fee".to_string(),
                market_slug: Some(market_slug.clone()),
                asset_id: None,
                usd_amount: fee,
            });
        }
        if gas > 0.0 {
            journal_events.push(JournalEvent::AccountingEvent {
                ts_ns: event.received_ns,
                kind: "gas".to_string(),
                market_slug: Some(market_slug),
                asset_id: None,
                usd_amount: gas,
            });
        }
    }
}

/// Run a set of windows in deterministic order (sorted by `window_id`).
/// Aborts with `Err` if `failed_count > max_window_failures`.
pub fn run_run<S, F>(
    windows: BTreeMap<String, Vec<Event>>,
    cfg: &RunnerConfig,
    strategy_factory: F,
) -> Result<Vec<WindowSummary>>
where
    S: ReplayStrategy,
    F: FnMut(&str, f64) -> S,
{
    run_run_with_journal_mode(windows, cfg, ReplayJournalMode::Full, strategy_factory)
}

/// Run a set of windows in deterministic order with explicit journal mode.
/// Existing `run_run` callers keep full audit-grade journal capture.
pub fn run_run_with_journal_mode<S, F>(
    windows: BTreeMap<String, Vec<Event>>,
    cfg: &RunnerConfig,
    journal_mode: ReplayJournalMode,
    mut strategy_factory: F,
) -> Result<Vec<WindowSummary>>
where
    S: ReplayStrategy,
    F: FnMut(&str, f64) -> S,
{
    let mut out = Vec::with_capacity(windows.len());
    let mut failed = 0usize;
    let mut carried_cash_usd = cfg.starting_cash_usd;
    for (window_id, events) in windows {
        let mut win_cfg = cfg.clone();
        win_cfg.window_id = window_id;
        win_cfg.starting_cash_usd = carried_cash_usd;
        let mut strategy = strategy_factory(&win_cfg.window_id, carried_cash_usd);
        let summary = run_window_with_journal_mode(&mut strategy, &events, &win_cfg, journal_mode);
        if summary.status != WindowStatus::Ok {
            failed += 1;
            if failed > cfg.max_window_failures {
                anyhow::bail!(
                    "exceeded --max-window-failures ({}); aborting",
                    cfg.max_window_failures
                );
            }
        } else {
            carried_cash_usd = summary.accounting.ending_cash_usd;
        }
        out.push(summary);
    }
    Ok(out)
}

/// Run windows in parallel, one strategy per window.
///
/// This is an intentionally independent-mode runner: each window starts from the
/// same `cfg.starting_cash_usd`. Fold with `fold_cash_carry` to enforce
/// carry-forward semantics if desired.
pub fn run_run_parallel_with_journal_mode<S, F>(
    windows: BTreeMap<String, Vec<Event>>,
    cfg: &RunnerConfig,
    journal_mode: ReplayJournalMode,
    max_threads: usize,
    strategy_factory: F,
) -> Result<Vec<WindowSummary>>
where
    S: ReplayStrategy + Send,
    F: Fn(&str, f64) -> S + Send + Sync,
{
    let mut plans = Vec::with_capacity(windows.len());
    for (index, (window_id, events)) in windows.into_iter().enumerate() {
        plans.push(WindowPlan {
            window_id,
            events,
            index,
        });
    }

    let cfg = cfg.clone();
    let thread_count = max_threads.max(1);
    let pool = rayon::ThreadPoolBuilder::new()
        .num_threads(thread_count)
        .build()
        .map_err(|err| anyhow::anyhow!("failed to build thread pool: {err}"))?;

    let mut out = pool.install(|| {
        plans
            .into_par_iter()
            .map(|plan| {
                let mut win_cfg = cfg.clone();
                win_cfg.window_id = plan.window_id.clone();
                win_cfg.starting_cash_usd = cfg.starting_cash_usd;
                let mut strategy = strategy_factory(&plan.window_id, cfg.starting_cash_usd);
                let summary = run_window_with_journal_mode(
                    &mut strategy,
                    &plan.events,
                    &win_cfg,
                    journal_mode,
                );
                (plan.index, summary)
            })
            .collect::<Vec<(usize, WindowSummary)>>()
    });

    out.sort_by_key(|(index, _)| *index);
    let summaries = out.into_iter().map(|(_, s)| s).collect();
    Ok(summaries)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    use crate::collector::schema::{EventType, Source};
    use crate::replay::fill_sim::{FillQuality, LatencyPreset, Side};

    fn evt(
        received_ns: i64,
        et: EventType,
        asset: &str,
        side: &str,
        price: &str,
        size: &str,
    ) -> Event {
        Event {
            v: 1,
            ts_ns: received_ns - 1,
            received_ns,
            event_type: et,
            market_type: "btc_5m".into(),
            market_slug: Some("btc-up-or-down".into()),
            asset_id: Some(asset.into()),
            side: Some(side.into()),
            price: Some(price.into()),
            size: Some(size.into()),
            sequence: Some(received_ns),
            source: Source::PolymarketMarketWs,
            raw: json!({}),
        }
    }

    fn market_meta(received_ns: i64, end_time_ms: i64) -> Event {
        Event {
            v: 1,
            ts_ns: received_ns - 1,
            received_ns,
            event_type: EventType::MarketMeta,
            market_type: "btc_5m".into(),
            market_slug: Some("btc-up-or-down".into()),
            asset_id: None,
            side: None,
            price: None,
            size: None,
            sequence: Some(received_ns),
            source: Source::PolymarketDataApi,
            raw: json!({
                "asset_ids": ["asset-a", "asset-b"],
                "strike": 100.0,
                "end_time_ms": end_time_ms
            }),
        }
    }

    fn btc_tick(received_ns: i64, price: &str) -> Event {
        Event {
            v: 1,
            ts_ns: received_ns - 1,
            received_ns,
            event_type: EventType::BtcTick,
            market_type: "btc_ref".into(),
            market_slug: Some("btcusdt".into()),
            asset_id: Some("BTC".into()),
            side: None,
            price: Some(price.into()),
            size: Some("0.01".into()),
            sequence: Some(received_ns),
            source: Source::BinanceAggtrade,
            raw: json!({}),
        }
    }

    /// Test fake: places one resting sell order on the first event, then
    /// passively counts fills. Used to verify the runner end-to-end.
    struct PassiveAskStrategy {
        placed: bool,
        on_fill_count: u32,
    }

    impl ReplayStrategy for PassiveAskStrategy {
        fn on_event(&mut self, event: &Event) -> ReplayDecision {
            if !self.placed {
                self.placed = true;
                return ReplayDecision {
                    submits: vec![StrategyOrderIntent::passive(
                        "passive-1",
                        event.asset_id.clone().unwrap_or_default(),
                        Side::Sell,
                        0.55,
                        100.0,
                        (event.received_ns / 1_000_000) as u64,
                    )],
                    cancels: vec![],
                    rejected_fills: vec![],
                    risk_rejections: vec![],
                    journal_events: vec![],
                    accounting_events: vec![],
                    ..ReplayDecision::default()
                };
            }
            ReplayDecision::default()
        }
        fn on_fill(&mut self, _fill: &SimulatedFill) -> ReplayDecision {
            self.on_fill_count += 1;
            ReplayDecision::default()
        }
    }

    struct PassiveBidStrategy {
        placed: bool,
    }

    impl ReplayStrategy for PassiveBidStrategy {
        fn on_event(&mut self, event: &Event) -> ReplayDecision {
            if !self.placed && event.asset_id.as_deref() == Some("asset-a") {
                self.placed = true;
                return ReplayDecision {
                    submits: vec![StrategyOrderIntent::passive(
                        "passive-bid-1",
                        "asset-a",
                        Side::Buy,
                        0.55,
                        10.0,
                        (event.received_ns / 1_000_000) as u64,
                    )],
                    cancels: vec![],
                    rejected_fills: vec![],
                    risk_rejections: vec![],
                    journal_events: vec![],
                    accounting_events: vec![],
                    ..ReplayDecision::default()
                };
            }
            ReplayDecision::default()
        }

        fn on_fill(&mut self, _fill: &SimulatedFill) -> ReplayDecision {
            ReplayDecision::default()
        }
    }

    struct DelayedPassiveAskStrategy {
        seen_events: u32,
    }

    impl ReplayStrategy for DelayedPassiveAskStrategy {
        fn on_event(&mut self, event: &Event) -> ReplayDecision {
            self.seen_events += 1;
            if self.seen_events == 2 {
                return ReplayDecision {
                    submits: vec![StrategyOrderIntent::passive(
                        "delayed-ask-1",
                        "asset-a",
                        Side::Sell,
                        0.55,
                        10.0,
                        (event.received_ns / 1_000_000) as u64,
                    )],
                    cancels: vec![],
                    rejected_fills: vec![],
                    risk_rejections: vec![],
                    journal_events: vec![],
                    accounting_events: vec![],
                    ..ReplayDecision::default()
                };
            }
            ReplayDecision::default()
        }

        fn on_fill(&mut self, _fill: &SimulatedFill) -> ReplayDecision {
            ReplayDecision::default()
        }
    }

    #[test]
    fn run_window_executes_event_loop_and_collects_fills() {
        let events = vec![
            evt(
                1_000_000_000,
                EventType::BookSnapshot,
                "asset-a",
                "buy",
                "0.55",
                "100",
            ),
            evt(
                2_000_000_000,
                EventType::Trade,
                "asset-a",
                "buy",
                "0.55",
                "60",
            ),
            evt(
                3_000_000_000,
                EventType::Trade,
                "asset-a",
                "buy",
                "0.55",
                "40",
            ),
        ];
        let cfg = RunnerConfig {
            window_id: "w1".into(),
            fill_sim: FillSimConfig {
                latency: LatencyPreset::Instant,
                ..Default::default()
            },
            max_window_failures: 0,
            starting_cash_usd: 1_000.0,
            maker_rebate_bps: 0.0,
        };
        let mut strategy = PassiveAskStrategy {
            placed: false,
            on_fill_count: 0,
        };
        let summary = run_window(&mut strategy, &events, &cfg);
        assert_eq!(summary.status, WindowStatus::Ok);
        assert_eq!(summary.fills.len(), 2);
        assert_eq!(summary.fills[0].size, 30.0);
        assert_eq!(summary.fills[1].size, 20.0);
        assert_eq!(strategy.on_fill_count, 2);
        assert_eq!(summary.events_replayed, 3);
        assert_eq!(summary.intents_submitted, 1);
        assert_eq!(summary.queue_calibration.sample_size, 1);
        assert_eq!(summary.queue_calibration.filled_orders, 1);
        assert_eq!(summary.queue_calibration.fill_rate, 1.0);
        assert_eq!(
            summary.queue_calibration.median_same_side_depth_at_entry,
            Some(0.0)
        );
    }

    #[test]
    fn run_window_reports_submission_samples_and_queue_calibration() {
        let events = vec![
            evt(
                1_000_000_000,
                EventType::BookSnapshot,
                "asset-a",
                "sell",
                "0.55",
                "20",
            ),
            evt(
                2_000_000_000,
                EventType::BookDelta,
                "asset-a",
                "sell",
                "0.55",
                "20",
            ),
            evt(
                3_000_000_000,
                EventType::Trade,
                "asset-a",
                "buy",
                "0.55",
                "25",
            ),
        ];
        let cfg = RunnerConfig {
            window_id: "w-queue".into(),
            fill_sim: FillSimConfig {
                latency: LatencyPreset::Instant,
                fill_quality: FillQuality::Base,
                ..Default::default()
            },
            max_window_failures: 0,
            starting_cash_usd: 1_000.0,
            maker_rebate_bps: 0.0,
        };
        let mut strategy = DelayedPassiveAskStrategy { seen_events: 0 };

        let summary = run_window(&mut strategy, &events, &cfg);

        assert_eq!(summary.status, WindowStatus::Ok);
        assert_eq!(summary.submitted_order_samples.len(), 1);
        assert_eq!(
            summary.submitted_order_samples[0].client_order_id,
            "delayed-ask-1"
        );
        assert_eq!(summary.submitted_order_samples[0].book_depth_at_rest, 20.0);
        assert_eq!(summary.queue_calibration.sample_size, 1);
        assert_eq!(summary.queue_calibration.filled_orders, 1);
        assert_eq!(summary.queue_calibration.fill_rate, 1.0);
        assert_eq!(summary.queue_calibration.partial_fill_rate, 1.0);
        assert_eq!(
            summary.queue_calibration.median_seconds_to_first_fill,
            Some(1.0)
        );
        assert_eq!(
            summary.queue_calibration.median_same_side_depth_at_entry,
            Some(20.0)
        );
        assert_eq!(
            summary
                .queue_calibration
                .median_estimated_queue_position_fraction,
            Some(1.0)
        );
        assert_eq!(summary.fills.len(), 1);
        assert_eq!(summary.fills[0].size, 2.5);
    }

    #[test]
    fn run_window_accounting_uses_synthesized_resolution_events() {
        let events = vec![
            market_meta(1_000_000_000, 300_000),
            evt(
                2_000_000_000,
                EventType::BookSnapshot,
                "asset-a",
                "sell",
                "0.55",
                "100",
            ),
            evt(
                3_000_000_000,
                EventType::Trade,
                "asset-a",
                "sell",
                "0.55",
                "10",
            ),
            btc_tick(301_000_000_000, "101.0"),
        ];
        let cfg = RunnerConfig {
            window_id: "w-resolved".into(),
            fill_sim: FillSimConfig {
                latency: LatencyPreset::Instant,
                ..Default::default()
            },
            max_window_failures: 0,
            starting_cash_usd: 1_000.0,
            maker_rebate_bps: 0.0,
        };
        let mut strategy = PassiveBidStrategy { placed: false };

        let summary = run_window(&mut strategy, &events, &cfg);

        assert_eq!(summary.status, WindowStatus::Ok);
        assert_eq!(summary.fills.len(), 1);
        assert_eq!(
            summary.accounting.resolution_winner_asset_id,
            Some("asset-a".to_string())
        );
        assert_eq!(summary.accounting.mark_source, "resolution");
        assert_eq!(summary.accounting.redeemable_value_usd, 5.0);
        assert_eq!(summary.accounting.ending_equity_usd, 1_002.25);
    }

    #[test]
    fn run_window_flushes_synthesized_resolution_without_post_close_event() {
        let events = vec![
            market_meta(1_000_000_000, 300_000),
            evt(
                2_000_000_000,
                EventType::BookSnapshot,
                "asset-a",
                "sell",
                "0.55",
                "100",
            ),
            evt(
                3_000_000_000,
                EventType::Trade,
                "asset-a",
                "sell",
                "0.55",
                "10",
            ),
            btc_tick(299_999_000_000, "101.0"),
        ];
        let cfg = RunnerConfig {
            window_id: "w-resolved-flush".into(),
            fill_sim: FillSimConfig {
                latency: LatencyPreset::Instant,
                ..Default::default()
            },
            max_window_failures: 0,
            starting_cash_usd: 1_000.0,
            maker_rebate_bps: 0.0,
        };
        let mut strategy = PassiveBidStrategy { placed: false };

        let summary = run_window(&mut strategy, &events, &cfg);

        assert_eq!(summary.status, WindowStatus::Ok);
        assert_eq!(summary.fills.len(), 1);
        assert_eq!(
            summary.accounting.resolution_winner_asset_id,
            Some("asset-a".to_string())
        );
        assert_eq!(summary.accounting.mark_source, "resolution");
        assert_eq!(summary.accounting.redeemable_value_usd, 5.0);
        assert_eq!(summary.accounting.settlement.status, "resolved_settled");
    }

    /// Strategy that panics on second event. Used to verify panic isolation.
    struct PanicAfter(usize);
    impl ReplayStrategy for PanicAfter {
        fn on_event(&mut self, _event: &Event) -> ReplayDecision {
            self.0 = self.0.saturating_sub(1);
            if self.0 == 0 {
                panic!("synthetic strategy panic");
            }
            ReplayDecision::default()
        }
        fn on_fill(&mut self, _fill: &SimulatedFill) -> ReplayDecision {
            ReplayDecision::default()
        }
    }

    #[test]
    fn run_window_catches_strategy_panic_and_marks_window() {
        let events = vec![
            evt(
                1_000_000_000,
                EventType::BookDelta,
                "asset-a",
                "buy",
                "0.5",
                "10",
            ),
            evt(
                2_000_000_000,
                EventType::Trade,
                "asset-a",
                "buy",
                "0.5",
                "10",
            ),
        ];
        let cfg = RunnerConfig {
            window_id: "w-bad".into(),
            fill_sim: FillSimConfig::default(),
            max_window_failures: 0,
            starting_cash_usd: 1_000.0,
            maker_rebate_bps: 0.0,
        };
        let mut s = PanicAfter(2);
        let summary = run_window(&mut s, &events, &cfg);
        assert_eq!(summary.status, WindowStatus::Panicked);
    }

    #[test]
    fn run_run_aborts_after_threshold() {
        let mut windows = BTreeMap::new();
        windows.insert(
            "a".to_string(),
            vec![evt(
                1_000_000_000,
                EventType::BookDelta,
                "a",
                "buy",
                "0.5",
                "1",
            )],
        );
        windows.insert(
            "b".to_string(),
            vec![evt(
                2_000_000_000,
                EventType::BookDelta,
                "a",
                "buy",
                "0.5",
                "1",
            )],
        );
        let cfg = RunnerConfig {
            window_id: String::new(),
            fill_sim: FillSimConfig::default(),
            max_window_failures: 0,
            starting_cash_usd: 1_000.0,
            maker_rebate_bps: 0.0,
        };
        let result = run_run(windows, &cfg, |_, _| PanicAfter(1));
        assert!(result.is_err());
    }

    #[test]
    fn run_run_carries_ending_cash_into_next_window() {
        let losing_window = vec![
            market_meta(1_000_000_000, 300_000),
            evt(
                2_000_000_000,
                EventType::BookSnapshot,
                "asset-a",
                "sell",
                "0.55",
                "100",
            ),
            evt(
                3_000_000_000,
                EventType::Trade,
                "asset-a",
                "sell",
                "0.55",
                "10",
            ),
            btc_tick(301_000_000_000, "99.0"),
        ];
        let idle_window = vec![evt(
            302_000_000_000,
            EventType::BookSnapshot,
            "asset-a",
            "sell",
            "0.55",
            "100",
        )];
        let mut windows = BTreeMap::new();
        windows.insert("a".to_string(), losing_window);
        windows.insert("b".to_string(), idle_window);
        let cfg = RunnerConfig {
            window_id: String::new(),
            fill_sim: FillSimConfig {
                latency: LatencyPreset::Instant,
                ..Default::default()
            },
            max_window_failures: 0,
            starting_cash_usd: 1_000.0,
            maker_rebate_bps: 0.0,
        };

        let summaries = run_run(windows, &cfg, |_, _| PassiveBidStrategy { placed: false })
            .expect("run succeeds");

        assert_eq!(summaries.len(), 2);
        assert_eq!(summaries[0].accounting.total_pnl_usd, -2.75);
        assert_eq!(summaries[0].accounting.ending_cash_usd, 997.25);
        assert_eq!(summaries[1].accounting.starting_cash_usd, 997.25);
        assert_eq!(summaries[1].accounting.ending_cash_usd, 997.25);
    }

    #[test]
    fn fold_cash_carry_rebases_window_starting_cash_and_equity() {
        let mut windows = vec![
            WindowSummary {
                window_id: "w1".into(),
                input_hash: "h1".into(),
                events_replayed: 0,
                intents_submitted: 0,
                queue_calibration: ReplayQueueCalibrationSummary::default(),
                submitted_order_samples: Vec::new(),
                fills: Vec::new(),
                accounting: ReplayAccountingSummary {
                    starting_cash_usd: 1_000.0,
                    ending_cash_usd: 900.0,
                    ending_equity_usd: 900.0,
                    ..ReplayAccountingSummary::default()
                },
                post_only_rejections: Vec::new(),
                risk_rejections: Vec::new(),
                journal_events: Vec::new(),
                status: WindowStatus::Ok,
            },
            WindowSummary {
                window_id: "w2".into(),
                input_hash: "h2".into(),
                events_replayed: 0,
                intents_submitted: 0,
                queue_calibration: ReplayQueueCalibrationSummary::default(),
                submitted_order_samples: Vec::new(),
                fills: Vec::new(),
                accounting: ReplayAccountingSummary {
                    starting_cash_usd: 1_000.0,
                    ending_cash_usd: 1_025.0,
                    ending_equity_usd: 1_025.0,
                    ..ReplayAccountingSummary::default()
                },
                post_only_rejections: Vec::new(),
                risk_rejections: Vec::new(),
                journal_events: Vec::new(),
                status: WindowStatus::Ok,
            },
        ];
        let ending_cash = fold_cash_carry(&mut windows, 1_000.0);
        assert_eq!(windows[0].accounting.starting_cash_usd, 1_000.0);
        assert_eq!(windows[0].accounting.ending_cash_usd, 900.0);
        assert_eq!(windows[1].accounting.starting_cash_usd, 900.0);
        assert_eq!(windows[1].accounting.ending_cash_usd, 925.0);
        assert_eq!(ending_cash, 925.0);
    }

    #[test]
    fn run_run_parallel_window_order_is_stable_and_cash_seed_is_window_agnostic() {
        let mut windows = BTreeMap::new();
        windows.insert(
            "b-window".to_string(),
            vec![
                evt(
                    2_000_000_000,
                    EventType::BookSnapshot,
                    "asset-a",
                    "buy",
                    "0.55",
                    "1",
                ),
                evt(
                    2_500_000_000,
                    EventType::Trade,
                    "asset-a",
                    "buy",
                    "0.55",
                    "1",
                ),
            ],
        );
        windows.insert(
            "a-window".to_string(),
            vec![
                evt(
                    1_000_000_000,
                    EventType::MarketMeta,
                    "asset-b",
                    "sell",
                    "0.55",
                    "1",
                ),
                evt(
                    1_500_000_000,
                    EventType::Trade,
                    "asset-b",
                    "sell",
                    "0.55",
                    "1",
                ),
            ],
        );
        let cfg = RunnerConfig {
            window_id: String::new(),
            fill_sim: FillSimConfig {
                latency: LatencyPreset::Instant,
                ..Default::default()
            },
            max_window_failures: 0,
            starting_cash_usd: 1_000.0,
            maker_rebate_bps: 0.0,
        };

        let summaries = run_run_parallel_with_journal_mode(
            windows,
            &cfg,
            ReplayJournalMode::None,
            2,
            |_window_id, starting_cash| {
                assert_eq!(starting_cash, 1_000.0);
                PassiveBidStrategy { placed: false }
            },
        )
        .expect("parallel run succeeds");

        assert_eq!(summaries[0].window_id, "a-window");
        assert_eq!(summaries[1].window_id, "b-window");
        assert_eq!(summaries[0].input_hash.len(), 64);
        assert_eq!(summaries[1].input_hash.len(), 64);
    }

    #[test]
    fn deterministic_runs_produce_identical_summaries() {
        // Same inputs twice → byte-identical fills vector.
        let events = vec![
            evt(
                1_000_000_000,
                EventType::BookSnapshot,
                "asset-a",
                "buy",
                "0.55",
                "100",
            ),
            evt(
                2_000_000_000,
                EventType::Trade,
                "asset-a",
                "buy",
                "0.55",
                "60",
            ),
            evt(
                3_000_000_000,
                EventType::Trade,
                "asset-a",
                "buy",
                "0.55",
                "40",
            ),
        ];
        let cfg = RunnerConfig {
            window_id: "w1".into(),
            fill_sim: FillSimConfig {
                latency: LatencyPreset::Instant,
                ..Default::default()
            },
            max_window_failures: 0,
            starting_cash_usd: 1_000.0,
            maker_rebate_bps: 0.0,
        };
        let mut s1 = PassiveAskStrategy {
            placed: false,
            on_fill_count: 0,
        };
        let mut s2 = PassiveAskStrategy {
            placed: false,
            on_fill_count: 0,
        };
        let r1 = run_window(&mut s1, &events, &cfg);
        let r2 = run_window(&mut s2, &events, &cfg);
        assert_eq!(r1, r2);
    }

    /// Journal-flow guard: every successful intent submission produces an
    /// `intent_submit` row and every fill produces a `fill` row. This is
    /// the minimum invariant downstream audit relies on.
    #[test]
    fn run_window_emits_journal_rows_for_submits_and_fills() {
        use crate::replay::journal::JournalEvent;
        let events = vec![
            evt(
                1_000_000_000,
                EventType::BookSnapshot,
                "asset-a",
                "buy",
                "0.55",
                "100",
            ),
            evt(
                2_000_000_000,
                EventType::Trade,
                "asset-a",
                "buy",
                "0.55",
                "100",
            ),
        ];
        let cfg = RunnerConfig {
            window_id: "w-journal".into(),
            fill_sim: FillSimConfig {
                latency: LatencyPreset::Instant,
                ..Default::default()
            },
            max_window_failures: 0,
            starting_cash_usd: 1_000.0,
            maker_rebate_bps: 0.0,
        };

        // The PassiveAskStrategy here populates only `submits`; the runner
        // should still synthesize a `Fill` journal row for the simulator
        // match. Strategy-side journal rows (StrategyDecision, IntentSubmit
        // etc.) are exercised by the strategy_adapter tests; this case
        // exercises the runner-owned synthesis path.
        let mut strategy = PassiveAskStrategy {
            placed: false,
            on_fill_count: 0,
        };
        let summary = run_window(&mut strategy, &events, &cfg);
        assert_eq!(summary.status, WindowStatus::Ok);
        let fill_count = summary
            .journal_events
            .iter()
            .filter(|e| {
                matches!(
                    e,
                    JournalEvent::Fill { .. } | JournalEvent::PartialFill { .. }
                )
            })
            .count();
        assert!(
            fill_count >= 1,
            "expected at least one Fill/PartialFill journal row, got {fill_count} of {} total",
            summary.journal_events.len()
        );
    }

    #[test]
    fn run_window_none_journal_mode_preserves_fills_and_accounting_without_rows() {
        let events = vec![
            evt(
                1_000_000_000,
                EventType::BookSnapshot,
                "asset-a",
                "buy",
                "0.55",
                "100",
            ),
            evt(
                2_000_000_000,
                EventType::Trade,
                "asset-a",
                "buy",
                "0.55",
                "100",
            ),
        ];
        let cfg = RunnerConfig {
            window_id: "w-no-journal".into(),
            fill_sim: FillSimConfig {
                latency: LatencyPreset::Instant,
                ..Default::default()
            },
            max_window_failures: 0,
            starting_cash_usd: 1_000.0,
            maker_rebate_bps: 0.0,
        };
        let mut strategy = PassiveAskStrategy {
            placed: false,
            on_fill_count: 0,
        };

        let summary =
            run_window_with_journal_mode(&mut strategy, &events, &cfg, ReplayJournalMode::None);

        assert_eq!(summary.status, WindowStatus::Ok);
        assert_eq!(summary.journal_events.len(), 0);
        assert_eq!(summary.fills.len(), 1);
        assert_eq!(summary.intents_submitted, 1);
        assert_eq!(strategy.on_fill_count, 1);
        assert_eq!(summary.accounting.starting_cash_usd, 1_000.0);
        assert_eq!(summary.accounting.ending_cash_usd, 1_000.0);
        assert_eq!(summary.accounting.invalid_fill_count, 1);
    }

    #[test]
    fn generic_convex_decision_label_is_not_cheap_tail_attribution() {
        assert_eq!(
            classify_tag("paired-mm decision_label=late_asymmetric_convex mode=convex_tilt"),
            AttributionPath::Other
        );
        assert_eq!(
            classify_tag("paired-mm decision_label=late_favorite_loading mode=convex_tilt"),
            AttributionPath::LateFavorite
        );
        assert_eq!(
            classify_tag("mm-convex-accum:ultra-cheap-tail:no:convex_accum"),
            AttributionPath::CheapTailConvexity
        );
        assert_eq!(classify_tag("core-hedge:core"), AttributionPath::PairedMm);
        assert_eq!(
            classify_tag("core_hedge geometry mismatch yes_ask=None no_ask=Some(0.55)"),
            AttributionPath::PairedMm
        );
        assert_eq!(
            classify_tag("late-fav-climb"),
            AttributionPath::LateFavorite
        );
        assert_eq!(
            classify_tag("late-fav-tail"),
            AttributionPath::CheapTailConvexity
        );
    }
}
