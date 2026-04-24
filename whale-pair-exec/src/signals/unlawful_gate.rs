use crate::signals::{BtcRegimeSnapshot, MarketActivitySignal};
use crate::types::BookLevel;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionBucket {
    Preferred,
    Neutral,
    Opportunistic,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnlawfulExecutionMode {
    Standby,
    Entry,
    Manage,
    Cleanup,
    Flatten,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnlawfulAggressionTier {
    Suppressed,
    Light,
    Normal,
    Press,
}

#[derive(Debug, Clone)]
pub struct PairedBookSignal {
    pub cheap_instrument_id: String,
    pub expensive_instrument_id: String,
    pub cheap_bid: Option<BookLevel>,
    pub cheap_ask: Option<BookLevel>,
    pub expensive_bid: Option<BookLevel>,
    pub expensive_ask: Option<BookLevel>,
    pub price_gap: Option<f64>,
    pub observed_at_ms: u64,
    pub books_fresh: bool,
    pub both_sides_present: bool,
}

#[derive(Debug, Clone)]
pub struct UnlawfulSignalSnapshot {
    pub session_bucket: SessionBucket,
    pub mode: UnlawfulExecutionMode,
    pub aggression_tier: UnlawfulAggressionTier,
    pub clip_scale: f64,
    pub gate_reasons: Vec<String>,
    pub btc: BtcRegimeSnapshot,
    pub book: PairedBookSignal,
    pub activity: MarketActivitySignal,
    pub first_fill_ms: Option<u64>,
    pub first_merge_ms: Option<u64>,
    pub elapsed_s: Option<u64>,
    pub time_remaining_s: Option<u64>,
}

#[derive(Debug, Clone)]
pub struct UnlawfulGateConfig {
    pub regime_primary_hours_utc: Vec<u8>,
    pub regime_secondary_hours_utc: Vec<u8>,
    pub allow_extreme_offhour_override: bool,
    pub entry_window_seconds: u64,
    pub cleanup_start_seconds: u64,
    pub close_start_seconds: u64,
    pub merge_stall_seconds: u64,
    pub entry_book_max_age_ms: u64,
    pub entry_btc_signal_max_age_ms: u64,
    pub primary_min_btc_realized_vol_5m_bps: f64,
    pub primary_min_btc_realized_vol_15m_bps: f64,
    pub primary_min_btc_trade_count_5m: u64,
    pub secondary_min_btc_realized_vol_5m_bps: f64,
    pub secondary_min_btc_realized_vol_15m_bps: f64,
    pub secondary_min_btc_trade_count_5m: u64,
    pub override_min_btc_realized_vol_5m_bps: f64,
    pub override_min_btc_realized_vol_15m_bps: f64,
    pub override_min_btc_trade_count_5m: u64,
    pub entry_cheap_ask_max: f64,
    pub entry_expensive_ask_min: f64,
    pub entry_expensive_ask_max: f64,
    pub entry_price_gap_min: f64,
    pub preferred_cheap_ask_max: f64,
    pub preferred_expensive_ask_min: f64,
    pub preferred_expensive_ask_max: f64,
    pub preferred_price_gap_min: f64,
    pub hard_shock_return_30s_bps: f64,
    pub hard_shock_return_60s_bps: f64,
    pub soft_shock_return_30s_bps: f64,
    pub soft_shock_return_60s_bps: f64,
}

#[derive(Debug, Clone)]
pub struct UnlawfulGateInputs {
    pub session_bucket: SessionBucket,
    pub now_ms: u64,
    pub market_start_ms: Option<u64>,
    pub market_end_ms: Option<u64>,
    pub market_context_present: bool,
    pub has_inventory: bool,
    pub cleanup_backlog_exceeded: bool,
    pub first_fill_ms: Option<u64>,
    pub first_merge_ms: Option<u64>,
    pub btc: BtcRegimeSnapshot,
    pub book: PairedBookSignal,
    pub activity: MarketActivitySignal,
}

impl Default for UnlawfulGateConfig {
    fn default() -> Self {
        Self {
            regime_primary_hours_utc: vec![10, 11, 19, 22, 23],
            regime_secondary_hours_utc: vec![9, 12, 20, 21, 0],
            allow_extreme_offhour_override: false,
            entry_window_seconds: 30,
            cleanup_start_seconds: 210,
            close_start_seconds: 270,
            merge_stall_seconds: 60,
            entry_book_max_age_ms: 1_200,
            entry_btc_signal_max_age_ms: 2_000,
            primary_min_btc_realized_vol_5m_bps: 5.0,
            primary_min_btc_realized_vol_15m_bps: 11.0,
            primary_min_btc_trade_count_5m: 5_000,
            secondary_min_btc_realized_vol_5m_bps: 8.0,
            secondary_min_btc_realized_vol_15m_bps: 15.0,
            secondary_min_btc_trade_count_5m: 8_000,
            override_min_btc_realized_vol_5m_bps: 12.0,
            override_min_btc_realized_vol_15m_bps: 20.0,
            override_min_btc_trade_count_5m: 10_000,
            entry_cheap_ask_max: 0.47,
            entry_expensive_ask_min: 0.56,
            entry_expensive_ask_max: 0.84,
            entry_price_gap_min: 0.22,
            preferred_cheap_ask_max: 0.40,
            preferred_expensive_ask_min: 0.62,
            preferred_expensive_ask_max: 0.78,
            preferred_price_gap_min: 0.35,
            hard_shock_return_30s_bps: 25.0,
            hard_shock_return_60s_bps: 30.0,
            soft_shock_return_30s_bps: 15.0,
            soft_shock_return_60s_bps: 20.0,
        }
    }
}

impl UnlawfulGateInputs {
    pub fn elapsed_s(&self) -> Option<u64> {
        Some(self.now_ms.saturating_sub(self.market_start_ms?) / 1_000)
    }

    fn cheap_ask(&self) -> Option<f64> {
        self.book.cheap_ask.as_ref().and_then(|level| {
            if level.quantity > 0.0 && level.price > 0.0 {
                Some(level.price)
            } else {
                None
            }
        })
    }

    fn expensive_ask(&self) -> Option<f64> {
        self.book.expensive_ask.as_ref().and_then(|level| {
            if level.quantity > 0.0 && level.price > 0.0 {
                Some(level.price)
            } else {
                None
            }
        })
    }

    fn gap(&self) -> Option<f64> {
        self.book.price_gap.or_else(|| {
            let cheap = self.cheap_ask()?;
            let expensive = self.expensive_ask()?;
            Some(expensive - cheap)
        })
    }
}

fn btc_signal_fresh(inputs: &UnlawfulGateInputs, cfg: &UnlawfulGateConfig) -> bool {
    if inputs.btc.observed_at_ms == 0 {
        return false;
    }
    let age_ms = inputs.now_ms.abs_diff(inputs.btc.observed_at_ms);
    age_ms <= cfg.entry_btc_signal_max_age_ms
}

fn book_fresh(inputs: &UnlawfulGateInputs, cfg: &UnlawfulGateConfig) -> bool {
    if !inputs.book.books_fresh {
        return false;
    }
    let age_ms = inputs.now_ms.abs_diff(inputs.book.observed_at_ms);
    age_ms <= cfg.entry_book_max_age_ms
}

fn btc_gate_ok(
    session_bucket: SessionBucket,
    inputs: &UnlawfulGateInputs,
    cfg: &UnlawfulGateConfig,
) -> bool {
    let vol5 = inputs.btc.realized_vol_5m_bps.unwrap_or(0.0);
    let vol15 = inputs.btc.realized_vol_15m_bps.unwrap_or(0.0);
    let trade5 = inputs.btc.trade_count_5m;

    match session_bucket {
        SessionBucket::Preferred => {
            vol5 >= cfg.primary_min_btc_realized_vol_5m_bps
                && vol15 >= cfg.primary_min_btc_realized_vol_15m_bps
                && trade5 >= cfg.primary_min_btc_trade_count_5m
        }
        SessionBucket::Neutral => {
            vol5 >= cfg.secondary_min_btc_realized_vol_5m_bps
                && vol15 >= cfg.secondary_min_btc_realized_vol_15m_bps
                && trade5 >= cfg.secondary_min_btc_trade_count_5m
        }
        SessionBucket::Opportunistic => {
            cfg.allow_extreme_offhour_override
                && vol5 >= cfg.override_min_btc_realized_vol_5m_bps
                && vol15 >= cfg.override_min_btc_realized_vol_15m_bps
                && trade5 >= cfg.override_min_btc_trade_count_5m
        }
    }
}

fn strong_market_activity(inputs: &UnlawfulGateInputs) -> bool {
    let age_ok = inputs
        .activity
        .last_trade_event_age_ms
        .is_none_or(|age_ms| age_ms <= 2_500);
    age_ok
        && inputs.activity.last_trade_event_count_10s >= 500
        && inputs.activity.last_trade_event_count_30s >= 1_500
}

fn geometry_ok(inputs: &UnlawfulGateInputs, cfg: &UnlawfulGateConfig) -> (bool, bool) {
    let cheap = inputs.cheap_ask();
    let expensive = inputs.expensive_ask();
    let gap = inputs.gap();

    let (Some(cheap), Some(expensive), Some(gap)) = (cheap, expensive, gap) else {
        return (false, false);
    };

    let hard = cheap <= cfg.entry_cheap_ask_max
        && expensive >= cfg.entry_expensive_ask_min
        && expensive <= cfg.entry_expensive_ask_max
        && gap >= cfg.entry_price_gap_min;

    let preferred = hard
        && cheap <= cfg.preferred_cheap_ask_max
        && expensive >= cfg.preferred_expensive_ask_min
        && expensive <= cfg.preferred_expensive_ask_max
        && gap >= cfg.preferred_price_gap_min;

    (hard, preferred)
}

fn shock_state(inputs: &UnlawfulGateInputs, cfg: &UnlawfulGateConfig) -> Option<&'static str> {
    let shock30 = inputs.btc.return_30s_bps.unwrap_or(0.0).abs();
    let shock60 = inputs.btc.return_60s_bps.unwrap_or(0.0).abs();
    if shock30 >= cfg.hard_shock_return_30s_bps || shock60 >= cfg.hard_shock_return_60s_bps {
        Some("hard")
    } else if shock30 >= cfg.soft_shock_return_30s_bps || shock60 >= cfg.soft_shock_return_60s_bps {
        Some("soft")
    } else {
        None
    }
}

fn close_if_eligible(
    mode: UnlawfulExecutionMode,
    inputs: &UnlawfulGateInputs,
    elapsed_s: u64,
    cfg: &UnlawfulGateConfig,
) -> UnlawfulExecutionMode {
    match mode {
        UnlawfulExecutionMode::Entry | UnlawfulExecutionMode::Manage
            if elapsed_s > cfg.entry_window_seconds =>
        {
            if inputs.has_inventory {
                UnlawfulExecutionMode::Manage
            } else {
                UnlawfulExecutionMode::Standby
            }
        }
        _ => mode,
    }
}

fn baseline_result(
    inputs: &UnlawfulGateInputs,
    mode: UnlawfulExecutionMode,
    aggression: UnlawfulAggressionTier,
    clip_scale: f64,
    elapsed_s: u64,
    time_remaining_s: Option<u64>,
    reasons: Vec<String>,
) -> UnlawfulSignalSnapshot {
    UnlawfulSignalSnapshot {
        session_bucket: inputs.session_bucket,
        mode,
        aggression_tier: aggression,
        clip_scale,
        gate_reasons: reasons,
        btc: inputs.btc.clone(),
        book: inputs.book.clone(),
        activity: inputs.activity.clone(),
        first_fill_ms: inputs.first_fill_ms,
        first_merge_ms: inputs.first_merge_ms,
        elapsed_s: Some(elapsed_s),
        time_remaining_s,
    }
}

pub fn evaluate_unlawful_mode(
    inputs: &UnlawfulGateInputs,
    cfg: &UnlawfulGateConfig,
) -> UnlawfulSignalSnapshot {
    const PRE_START_GRACE_MS: u64 = 5_000;
    let mut reasons: Vec<String> = Vec::new();

    let Some(start_ms) = inputs.market_start_ms else {
        reasons.push("market context missing start".to_string());
        return baseline_result(
            inputs,
            UnlawfulExecutionMode::Standby,
            UnlawfulAggressionTier::Suppressed,
            0.0,
            0,
            None,
            reasons,
        );
    };

    let Some(end_ms) = inputs.market_end_ms else {
        reasons.push("market context missing end".to_string());
        return baseline_result(
            inputs,
            UnlawfulExecutionMode::Standby,
            UnlawfulAggressionTier::Suppressed,
            0.0,
            0,
            None,
            reasons,
        );
    };

    if !inputs.market_context_present {
        reasons.push("market context missing".to_string());
        return baseline_result(
            inputs,
            UnlawfulExecutionMode::Standby,
            UnlawfulAggressionTier::Suppressed,
            0.0,
            0,
            None,
            reasons,
        );
    }

    let elapsed_s = inputs.elapsed_s().unwrap_or(0);
    let time_remaining_s = end_ms.checked_sub(inputs.now_ms).map(|value| value / 1_000);

    if inputs.now_ms.saturating_add(PRE_START_GRACE_MS) < start_ms {
        reasons.push("pre-start window".to_string());
        return baseline_result(
            inputs,
            UnlawfulExecutionMode::Standby,
            UnlawfulAggressionTier::Suppressed,
            0.0,
            elapsed_s,
            time_remaining_s,
            reasons,
        );
    }

    if inputs.now_ms >= end_ms {
        reasons.push("window closed".to_string());
        return baseline_result(
            inputs,
            UnlawfulExecutionMode::Flatten,
            UnlawfulAggressionTier::Suppressed,
            0.0,
            elapsed_s,
            Some(0),
            reasons,
        );
    }

    if !btc_signal_fresh(inputs, cfg) {
        reasons.push("stale btc signal".to_string());
        return baseline_result(
            inputs,
            if inputs.has_inventory {
                UnlawfulExecutionMode::Cleanup
            } else {
                UnlawfulExecutionMode::Standby
            },
            UnlawfulAggressionTier::Suppressed,
            0.0,
            elapsed_s,
            time_remaining_s,
            reasons,
        );
    }

    if !book_fresh(inputs, cfg) || !inputs.book.both_sides_present {
        reasons.push("stale or incomplete book".to_string());
        return baseline_result(
            inputs,
            if inputs.has_inventory {
                UnlawfulExecutionMode::Cleanup
            } else {
                UnlawfulExecutionMode::Standby
            },
            UnlawfulAggressionTier::Suppressed,
            0.0,
            elapsed_s,
            time_remaining_s,
            reasons,
        );
    }

    let (hard_geometry, preferred_geometry) = geometry_ok(inputs, cfg);
    if !hard_geometry {
        reasons.push("geometry rejected".to_string());
        return baseline_result(
            inputs,
            if inputs.has_inventory {
                UnlawfulExecutionMode::Cleanup
            } else {
                UnlawfulExecutionMode::Standby
            },
            UnlawfulAggressionTier::Suppressed,
            0.0,
            elapsed_s,
            time_remaining_s,
            reasons,
        );
    }

    let btc_gate_passed = btc_gate_ok(inputs.session_bucket, inputs, cfg);
    let microstructure_override = !btc_gate_passed
        && strong_market_activity(inputs)
        && matches!(
            inputs.session_bucket,
            SessionBucket::Preferred | SessionBucket::Neutral
        );

    if !btc_gate_passed && !microstructure_override {
        reasons.push("btc gate rejected".to_string());
        return baseline_result(
            inputs,
            if inputs.has_inventory {
                UnlawfulExecutionMode::Cleanup
            } else {
                UnlawfulExecutionMode::Standby
            },
            UnlawfulAggressionTier::Suppressed,
            0.0,
            elapsed_s,
            time_remaining_s,
            reasons,
        );
    }

    if microstructure_override {
        reasons.push("btc gate softened by market activity".to_string());
    }

    let mut mode = if elapsed_s <= cfg.entry_window_seconds {
        if inputs.has_inventory {
            UnlawfulExecutionMode::Manage
        } else {
            UnlawfulExecutionMode::Entry
        }
    } else if elapsed_s < cfg.cleanup_start_seconds {
        UnlawfulExecutionMode::Manage
    } else if elapsed_s < cfg.close_start_seconds {
        UnlawfulExecutionMode::Cleanup
    } else {
        UnlawfulExecutionMode::Flatten
    };

    if inputs.cleanup_backlog_exceeded {
        reasons.push("cleanup backlog exceeded".to_string());
        mode = UnlawfulExecutionMode::Cleanup;
    }

    if elapsed_s >= cfg.close_start_seconds {
        mode = UnlawfulExecutionMode::Flatten;
    } else if inputs.cleanup_backlog_exceeded {
        mode = UnlawfulExecutionMode::Cleanup;
    }

    if let Some(shock) = shock_state(inputs, cfg) {
        reasons.push(format!("btc shock {shock}"));
        match shock {
            "hard" => mode = UnlawfulExecutionMode::Cleanup,
            "soft" => mode = UnlawfulExecutionMode::Manage,
            _ => {}
        }
    }

    if inputs.has_inventory
        && inputs.first_fill_ms.is_some()
        && inputs.first_merge_ms.is_none()
        && inputs.first_fill_ms.is_some_and(|fill_ms| {
            inputs.now_ms.saturating_sub(fill_ms) >= cfg.merge_stall_seconds * 1000
        })
    {
        reasons.push("merge stalled".to_string());
        mode = UnlawfulExecutionMode::Cleanup;
    }

    let mode = if mode == UnlawfulExecutionMode::Cleanup || mode == UnlawfulExecutionMode::Flatten {
        mode
    } else {
        close_if_eligible(mode, inputs, elapsed_s, cfg)
    };

    let (aggression_tier, clip_scale) = match mode {
        UnlawfulExecutionMode::Entry => {
            if microstructure_override && preferred_geometry {
                (UnlawfulAggressionTier::Light, 0.6)
            } else if microstructure_override {
                (UnlawfulAggressionTier::Light, 0.45)
            } else if preferred_geometry {
                (UnlawfulAggressionTier::Normal, 1.0)
            } else {
                (UnlawfulAggressionTier::Light, 0.6)
            }
        }
        UnlawfulExecutionMode::Manage => {
            if microstructure_override && preferred_geometry {
                (UnlawfulAggressionTier::Light, 0.6)
            } else if microstructure_override {
                (UnlawfulAggressionTier::Light, 0.45)
            } else if preferred_geometry {
                (UnlawfulAggressionTier::Normal, 1.0)
            } else {
                (UnlawfulAggressionTier::Light, 0.6)
            }
        }
        UnlawfulExecutionMode::Standby
        | UnlawfulExecutionMode::Cleanup
        | UnlawfulExecutionMode::Flatten => (UnlawfulAggressionTier::Suppressed, 0.0),
    };

    baseline_result(
        inputs,
        mode,
        aggression_tier,
        clip_scale,
        elapsed_s,
        time_remaining_s,
        reasons,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::BookLevel;

    fn book(cheap_ask: f64, expensive_ask: f64, gap: f64, fresh: bool) -> PairedBookSignal {
        PairedBookSignal {
            cheap_instrument_id: "down".to_string(),
            expensive_instrument_id: "up".to_string(),
            cheap_bid: Some(BookLevel::new((cheap_ask - 0.01).max(0.0), 100.0)),
            cheap_ask: Some(BookLevel::new(cheap_ask, 100.0)),
            expensive_bid: Some(BookLevel::new((expensive_ask - 0.01).max(0.0), 100.0)),
            expensive_ask: Some(BookLevel::new(expensive_ask, 100.0)),
            price_gap: Some(gap),
            observed_at_ms: 10_000,
            books_fresh: fresh,
            both_sides_present: true,
        }
    }

    fn activity() -> MarketActivitySignal {
        MarketActivitySignal {
            last_trade_event_count_10s: 100,
            last_trade_event_count_30s: 310,
            last_trade_event_count_60s: 900,
            last_trade_event_age_ms: None,
        }
    }

    fn hot_activity() -> MarketActivitySignal {
        MarketActivitySignal {
            last_trade_event_count_10s: 800,
            last_trade_event_count_30s: 2_400,
            last_trade_event_count_60s: 6_000,
            last_trade_event_age_ms: Some(150),
        }
    }

    fn btc(v5: f64, v15: f64, trades: u64) -> BtcRegimeSnapshot {
        BtcRegimeSnapshot {
            last_price: Some(50_000.0),
            realized_vol_5m_bps: Some(v5),
            realized_vol_15m_bps: Some(v15),
            trade_count_5m: trades,
            trade_count_15m: trades,
            return_30s_bps: Some(1.0),
            return_60s_bps: Some(1.0),
            observed_at_ms: 10_000,
        }
    }

    fn scenario(
        session_bucket: SessionBucket,
        elapsed_s: u64,
        has_inventory: bool,
        first_fill_ms: Option<u64>,
        first_merge_ms: Option<u64>,
    ) -> UnlawfulGateInputs {
        let now_ms = 10_000 + elapsed_s * 1000;
        let mut btc_signal = btc(6.0, 12.0, 9_000);
        btc_signal.observed_at_ms = now_ms;
        let mut book_signal = book(0.38, 0.65, 0.27, true);
        book_signal.observed_at_ms = now_ms;
        UnlawfulGateInputs {
            session_bucket,
            now_ms,
            market_start_ms: Some(10_000),
            market_end_ms: Some(1_000_000),
            market_context_present: true,
            has_inventory,
            cleanup_backlog_exceeded: false,
            first_fill_ms,
            first_merge_ms,
            btc: btc_signal,
            book: book_signal,
            activity: activity(),
        }
    }

    #[test]
    fn invariant_missing_or_stale_book_suppresses_new_risk() {
        let cfg = UnlawfulGateConfig::default();

        let mut missing_book = scenario(SessionBucket::Preferred, 10, true, None, None);
        missing_book.book.both_sides_present = false;
        let result = evaluate_unlawful_mode(&missing_book, &cfg);
        assert_eq!(result.mode, UnlawfulExecutionMode::Cleanup);
        assert_eq!(result.aggression_tier, UnlawfulAggressionTier::Suppressed);
        assert!(result
            .gate_reasons
            .iter()
            .any(|reason| reason.contains("stale or incomplete book")));

        let mut stale_books = scenario(SessionBucket::Preferred, 10, false, None, None);
        stale_books.book.observed_at_ms = 10_000;
        stale_books.now_ms = 25_000;
        stale_books.btc.observed_at_ms = stale_books.now_ms;
        let result = evaluate_unlawful_mode(&stale_books, &cfg);
        assert_eq!(result.mode, UnlawfulExecutionMode::Standby);
        assert_eq!(result.aggression_tier, UnlawfulAggressionTier::Suppressed);
        assert!(result
            .gate_reasons
            .iter()
            .any(|reason| reason.contains("stale or incomplete book")));
    }

    #[test]
    fn invariant_stale_btc_signal_blocks_new_risk() {
        let mut cfg = UnlawfulGateConfig::default();
        cfg.entry_btc_signal_max_age_ms = 1_000;
        let mut input = scenario(SessionBucket::Preferred, 10, false, None, None);
        input.btc.observed_at_ms = 5_000;

        let result = evaluate_unlawful_mode(&input, &cfg);
        assert_eq!(result.mode, UnlawfulExecutionMode::Standby);
        assert_eq!(result.aggression_tier, UnlawfulAggressionTier::Suppressed);
        assert!(result
            .gate_reasons
            .iter()
            .any(|reason| reason.contains("stale btc signal")));
    }

    #[test]
    fn invariant_backlog_drives_cleanup_or_flatten() {
        let cfg = UnlawfulGateConfig::default();

        let mut backlog_cleanup = scenario(SessionBucket::Preferred, 10, true, Some(5_000), None);
        backlog_cleanup.cleanup_backlog_exceeded = true;
        let cleanup_result = evaluate_unlawful_mode(&backlog_cleanup, &cfg);
        assert_eq!(cleanup_result.mode, UnlawfulExecutionMode::Cleanup);
        assert_eq!(
            cleanup_result.aggression_tier,
            UnlawfulAggressionTier::Suppressed
        );
        assert!(cleanup_result
            .gate_reasons
            .iter()
            .any(|reason| reason.contains("cleanup backlog exceeded")));

        let mut backlog_flatten = scenario(SessionBucket::Preferred, 300, false, None, None);
        backlog_flatten.cleanup_backlog_exceeded = true;
        let flatten_result = evaluate_unlawful_mode(&backlog_flatten, &cfg);
        assert_eq!(flatten_result.mode, UnlawfulExecutionMode::Flatten);
        assert_eq!(
            flatten_result.aggression_tier,
            UnlawfulAggressionTier::Suppressed
        );
        assert!(flatten_result
            .gate_reasons
            .iter()
            .any(|reason| reason.contains("cleanup backlog exceeded")));
    }

    #[test]
    fn scenario_1_primary_hour_and_btc_gate_hard_geometry_enters() {
        let cfg = UnlawfulGateConfig::default();
        let result = evaluate_unlawful_mode(
            &scenario(SessionBucket::Preferred, 10, false, None, None),
            &cfg,
        );
        assert_eq!(result.mode, UnlawfulExecutionMode::Entry);
        assert_eq!(result.clip_scale, 0.6);
        assert_eq!(result.aggression_tier, UnlawfulAggressionTier::Light);
    }

    #[test]
    fn scenario_2_secondary_hour_weak_btc_remains_standby() {
        let cfg = UnlawfulGateConfig::default();
        let mut input = scenario(SessionBucket::Neutral, 10, false, None, None);
        input.btc = btc(3.0, 9.0, 2_000);
        let result = evaluate_unlawful_mode(&input, &cfg);
        assert_eq!(result.mode, UnlawfulExecutionMode::Standby);
        assert_eq!(result.aggression_tier, UnlawfulAggressionTier::Suppressed);
    }

    #[test]
    fn scenario_2b_preferred_hour_weak_btc_but_hot_market_enters_light() {
        let cfg = UnlawfulGateConfig::default();
        let mut input = scenario(SessionBucket::Preferred, 10, false, None, None);
        input.btc = btc(0.2, 0.5, 250);
        input.btc.observed_at_ms = input.now_ms;
        input.activity = hot_activity();
        let result = evaluate_unlawful_mode(&input, &cfg);
        assert_eq!(result.mode, UnlawfulExecutionMode::Entry);
        assert_eq!(result.aggression_tier, UnlawfulAggressionTier::Light);
        assert_eq!(result.clip_scale, 0.45);
        assert!(result
            .gate_reasons
            .iter()
            .any(|reason| reason.contains("btc gate softened by market activity")));
    }

    #[test]
    fn scenario_3_off_hour_without_override_stays_standby() {
        let cfg = UnlawfulGateConfig::default();
        let mut input = scenario(SessionBucket::Opportunistic, 10, false, None, None);
        input.btc = btc(20.0, 30.0, 20_000);
        input.btc.observed_at_ms = input.now_ms;
        let result = evaluate_unlawful_mode(&input, &cfg);
        assert_eq!(result.mode, UnlawfulExecutionMode::Standby);
        assert_eq!(result.aggression_tier, UnlawfulAggressionTier::Suppressed);
    }

    #[test]
    fn scenario_3b_off_hour_hot_market_without_override_still_standby() {
        let cfg = UnlawfulGateConfig::default();
        let mut input = scenario(SessionBucket::Opportunistic, 10, false, None, None);
        input.btc = btc(0.2, 0.5, 250);
        input.btc.observed_at_ms = input.now_ms;
        input.activity = hot_activity();
        let result = evaluate_unlawful_mode(&input, &cfg);
        assert_eq!(result.mode, UnlawfulExecutionMode::Standby);
        assert!(result
            .gate_reasons
            .iter()
            .any(|reason| reason.contains("btc gate rejected")));
    }

    #[test]
    fn scenario_4_off_hour_override_allows_entry_when_enabled() {
        let mut cfg = UnlawfulGateConfig::default();
        cfg.allow_extreme_offhour_override = true;
        let mut input = scenario(SessionBucket::Opportunistic, 10, false, None, None);
        input.btc = btc(20.0, 30.0, 20_000);
        input.btc.observed_at_ms = input.now_ms;
        let result = evaluate_unlawful_mode(&input, &cfg);
        assert_eq!(result.mode, UnlawfulExecutionMode::Entry);
        assert_eq!(result.clip_scale, 0.6);
        assert_eq!(result.aggression_tier, UnlawfulAggressionTier::Light);
    }

    #[test]
    fn scenario_5_zero_inventory_missed_entry_window() {
        let cfg = UnlawfulGateConfig::default();
        let result = evaluate_unlawful_mode(
            &scenario(SessionBucket::Preferred, 40, false, None, None),
            &cfg,
        );
        assert_eq!(result.mode, UnlawfulExecutionMode::Standby);
        assert_eq!(result.aggression_tier, UnlawfulAggressionTier::Suppressed);
    }

    #[test]
    fn scenario_6_inventory_merge_stall_drives_cleanup() {
        let cfg = UnlawfulGateConfig::default();
        let result = evaluate_unlawful_mode(
            &scenario_with_stalled_merge(SessionBucket::Preferred, true, 65),
            &cfg,
        );
        assert_eq!(result.mode, UnlawfulExecutionMode::Cleanup);
        assert!(result
            .gate_reasons
            .iter()
            .any(|reason| reason.contains("merge stalled")));
    }

    #[test]
    fn scenario_7_existing_inventory_soft_btc_shock_stays_manage() {
        let cfg = UnlawfulGateConfig::default();
        let mut input = scenario(SessionBucket::Preferred, 10, true, None, None);
        input.btc.return_30s_bps = Some(16.0);
        let result = evaluate_unlawful_mode(&input, &cfg);
        assert_eq!(result.mode, UnlawfulExecutionMode::Manage);
        assert_eq!(result.aggression_tier, UnlawfulAggressionTier::Light);
    }

    #[test]
    fn scenario_8_existing_inventory_hard_btc_shock_drives_cleanup() {
        let cfg = UnlawfulGateConfig::default();
        let mut input = scenario(SessionBucket::Preferred, 10, true, None, None);
        input.btc.return_60s_bps = Some(31.0);
        let result = evaluate_unlawful_mode(&input, &cfg);
        assert_eq!(result.mode, UnlawfulExecutionMode::Cleanup);
        assert_eq!(result.aggression_tier, UnlawfulAggressionTier::Suppressed);
    }

    #[test]
    fn scenario_9_elapsed_past_close_window_forces_flatten() {
        let cfg = UnlawfulGateConfig::default();
        let result = evaluate_unlawful_mode(
            &scenario(SessionBucket::Preferred, 271, false, None, None),
            &cfg,
        );
        assert_eq!(result.mode, UnlawfulExecutionMode::Flatten);
        assert_eq!(result.aggression_tier, UnlawfulAggressionTier::Suppressed);
    }

    fn scenario_with_stalled_merge(
        session_bucket: SessionBucket,
        has_inventory: bool,
        now_elapsed_s: u64,
    ) -> UnlawfulGateInputs {
        let now_ms = 10_000 + now_elapsed_s * 1000;
        let fill_ms = now_ms - 65_000;
        UnlawfulGateInputs {
            session_bucket,
            now_ms,
            market_start_ms: Some(10_000),
            market_end_ms: Some(1_000_000),
            market_context_present: true,
            has_inventory,
            cleanup_backlog_exceeded: false,
            first_fill_ms: Some(fill_ms),
            first_merge_ms: None,
            btc: {
                let mut signal = btc(6.0, 12.0, 9_000);
                signal.observed_at_ms = now_ms;
                signal
            },
            book: {
                let mut signal = book(0.38, 0.65, 0.27, true);
                signal.observed_at_ms = now_ms;
                signal
            },
            activity: activity(),
        }
    }
}
