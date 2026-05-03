use crate::config::AppConfig;
use crate::types::ClientOrderId;
use crate::wire::execution_adapter::{VenueFill, VenuePosition};

#[derive(Debug, Default)]
pub(super) struct LiveSafetyState {
    pub(super) consecutive_submit_errors: usize,
    pub(super) consecutive_cancel_errors: usize,
    pub(super) consecutive_reconcile_mismatches: usize,
    pub(super) last_venue_cash_usd: Option<f64>,
    pub(super) last_venue_position_count: usize,
    pub(super) last_venue_balance_observed_at_ms: Option<u64>,
}

#[derive(Debug, Default)]
pub(super) struct ExecutionSyncReport {
    pub(super) open_order_count: usize,
    pub(super) balance_synced: bool,
    pub(super) venue_cash_usd: Option<f64>,
    pub(super) venue_position_count: usize,
    pub(super) venue_positions_authoritative: bool,
    pub(super) venue_balance_observed_at_ms: Option<u64>,
    pub(super) venue_fills: Vec<VenueFill>,
    pub(super) venue_positions: Vec<VenuePosition>,
    pub(super) errors: usize,
    pub(super) missing_local_orders: Vec<ClientOrderId>,
    pub(super) pending_missing_local_orders: Vec<ClientOrderId>,
}

#[derive(Debug, Clone)]
pub(super) struct ExecutionPolicy {
    pub(super) paper_mode: bool,
    pub(super) live_post_only: bool,
    pub(super) live_order_ttl_ms: u64,
    pub(super) live_order_max_age_ms: u64,
    pub(super) live_reconcile_missing_grace_ms: u64,
    pub(super) live_max_submit_errors: usize,
    pub(super) live_max_cancel_errors: usize,
    pub(super) live_kill_on_reconcile_mismatch: bool,
    pub(super) paper_min_fill_notional_usd: f64,
    pub(super) paper_max_fills_per_order: usize,
    pub(super) paper_min_fill_interval_ms: u64,
    pub(super) paper_market_close_at_ms: Option<u64>,
    pub(super) paper_market_resolution_price: Option<f64>,
    pub(super) paper_submit_latency_ms: u64,
    pub(super) paper_queue_depth_fraction: f64,
    pub(super) paper_post_only_reject_probability: f64,
    pub(super) paper_cancel_race_window_ms: u64,
    pub(super) paper_maker_rebate_coeff: f64,
    pub(super) paper_taker_fee_coeff_override: Option<f64>,
}

impl ExecutionPolicy {
    pub(super) fn from_config(config: &AppConfig) -> Self {
        Self {
            paper_mode: config.paper_mode,
            live_post_only: config.live_post_only,
            live_order_ttl_ms: config.live_order_ttl.as_millis() as u64,
            live_order_max_age_ms: config.live_order_max_age.as_millis() as u64,
            live_reconcile_missing_grace_ms: config.live_reconcile_missing_grace.as_millis() as u64,
            live_max_submit_errors: config.live_max_submit_errors,
            live_max_cancel_errors: config.live_max_cancel_errors,
            live_kill_on_reconcile_mismatch: config.live_kill_on_reconcile_mismatch,
            paper_min_fill_notional_usd: config.paper_min_fill_notional_usd,
            paper_max_fills_per_order: config.paper_max_fills_per_order,
            paper_min_fill_interval_ms: config.paper_min_fill_interval.as_millis() as u64,
            paper_market_close_at_ms: config.paper_market_close_at_ms,
            paper_market_resolution_price: config.paper_market_resolution_price,
            paper_submit_latency_ms: config.paper_submit_latency_ms,
            paper_queue_depth_fraction: config.paper_queue_depth_fraction,
            paper_post_only_reject_probability: config.paper_post_only_reject_probability,
            paper_cancel_race_window_ms: config.paper_cancel_race_window_ms,
            paper_maker_rebate_coeff: config.paper_maker_rebate_coeff,
            paper_taker_fee_coeff_override: config.paper_taker_fee_coeff_override,
        }
    }
}
