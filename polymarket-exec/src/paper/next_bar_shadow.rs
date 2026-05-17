//! Shadow feature logging for next-bar / early-directional calibration.
//!
//! This writer never emits orders. It records paired-market book state, recent
//! taker-flow proxies, and BTC regime features so offline calibration can answer
//! whether an aggressive mid-band directional strategy is actually predictive.

use std::collections::HashMap;
use std::fs::{self, File, OpenOptions};
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::book::BookState;
use crate::market_context::MarketContextRecord;
use crate::signals::BtcRegimeSnapshot;

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ShadowMarketPhase {
    PreOpen,
    Active,
    PostClose,
    Unknown,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ShadowBookLeg {
    pub asset_id: String,
    pub bid: f64,
    pub ask: f64,
    pub mid: Option<f64>,
    pub bid_size: f64,
    pub ask_size: f64,
    pub bid_depth_usd_5: f64,
    pub ask_depth_usd_5: f64,
    pub taker_buy_qty_60s: f64,
    pub taker_sell_qty_60s: f64,
    pub last_trade: f64,
    pub book_observed_at_ms: u64,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct NextBarShadowRecord {
    pub observed_at_ms: u64,
    pub market_id: String,
    pub event_start_time_ms: Option<u64>,
    pub event_end_time_ms: Option<u64>,
    pub ms_to_start: Option<i64>,
    pub ms_to_end: Option<i64>,
    pub phase: ShadowMarketPhase,
    pub price_to_beat: Option<f64>,
    pub final_price: Option<f64>,
    pub yes: ShadowBookLeg,
    pub no: ShadowBookLeg,
    pub yes_no_ask_sum: Option<f64>,
    pub book_favorite: Option<String>,
    pub book_favorite_ask: Option<f64>,
    pub book_cheap_ask: Option<f64>,
    pub normalized_yes_mid: Option<f64>,
    pub flow_up_minus_down_qty_60s: f64,
    pub flow_score: f64,
    pub btc_spot: Option<f64>,
    pub btc_realized_vol_5m_bps: Option<f64>,
    pub btc_realized_vol_15m_bps: Option<f64>,
    pub btc_return_30s_bps: Option<f64>,
    pub btc_return_60s_bps: Option<f64>,
    pub btc_return_120s_bps: Option<f64>,
    pub btc_return_180s_bps: Option<f64>,
    pub btc_regime: Option<String>,
    pub momentum_score: f64,
    pub composite_score: f64,
    pub candidate_side: Option<String>,
    pub candidate_reason: String,
}

pub struct NextBarShadowWriter {
    path: PathBuf,
    min_interval_ms: u64,
    last_record_by_market: HashMap<String, u64>,
    file: BufWriter<File>,
}

impl NextBarShadowWriter {
    pub fn open(path: &Path, min_interval_ms: u64) -> Result<Self> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).with_context(|| {
                format!(
                    "failed to create next-bar shadow directory {}",
                    parent.display()
                )
            })?;
        }
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .with_context(|| format!("failed to open next-bar shadow log {}", path.display()))?;
        Ok(Self {
            path: path.to_path_buf(),
            min_interval_ms,
            last_record_by_market: HashMap::new(),
            file: BufWriter::new(file),
        })
    }

    pub fn record_if_due(
        &mut self,
        market: &MarketContextRecord,
        yes: &BookState,
        no: &BookState,
        btc: &BtcRegimeSnapshot,
        observed_at_ms: u64,
    ) -> Result<bool> {
        let last = self
            .last_record_by_market
            .get(&market.market_id)
            .copied()
            .unwrap_or(0);
        if observed_at_ms.saturating_sub(last) < self.min_interval_ms {
            return Ok(false);
        }
        let record = build_record(market, yes, no, btc, observed_at_ms);
        let line =
            serde_json::to_string(&record).context("failed to serialize next-bar shadow record")?;
        self.file.write_all(line.as_bytes())?;
        self.file.write_all(b"\n")?;
        self.file.flush()?;
        self.last_record_by_market
            .insert(market.market_id.clone(), observed_at_ms);
        Ok(true)
    }

    pub fn flush(&mut self) -> Result<()> {
        self.file.flush()?;
        Ok(())
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for NextBarShadowWriter {
    fn drop(&mut self) {
        let _ = self.file.flush();
    }
}

fn build_record(
    market: &MarketContextRecord,
    yes: &BookState,
    no: &BookState,
    btc: &BtcRegimeSnapshot,
    observed_at_ms: u64,
) -> NextBarShadowRecord {
    let yes_leg = book_leg(yes, observed_at_ms);
    let no_leg = book_leg(no, observed_at_ms);
    let yes_no_ask_sum = finite_pair(yes.best_ask, no.best_ask).map(|(y, n)| y + n);
    let normalized_yes_mid = match (book_mid(yes), book_mid(no)) {
        (Some(y), Some(n)) if y + n > 0.0 => Some(y / (y + n)),
        _ => None,
    };
    let (book_favorite, book_favorite_ask, book_cheap_ask) =
        book_favorite(yes.best_ask, no.best_ask);
    let flow_up_minus_down_qty_60s = flow_up_minus_down_qty(yes, no, observed_at_ms);
    let flow_score = squash(flow_up_minus_down_qty_60s / 100.0);
    let momentum_score = momentum_score(btc);
    let composite_score = (0.55 * momentum_score) + (0.30 * flow_score) + book_skew_score(yes, no);
    let (candidate_side, candidate_reason) = candidate(
        composite_score,
        market,
        yes_no_ask_sum,
        book_favorite.as_deref(),
    );

    NextBarShadowRecord {
        observed_at_ms,
        market_id: market.market_id.clone(),
        event_start_time_ms: market.event_start_time_ms,
        event_end_time_ms: market.event_end_time_ms,
        ms_to_start: market
            .event_start_time_ms
            .map(|start| start as i64 - observed_at_ms as i64),
        ms_to_end: market
            .event_end_time_ms
            .map(|end| end as i64 - observed_at_ms as i64),
        phase: market_phase(market, observed_at_ms),
        price_to_beat: market.price_to_beat,
        final_price: market.final_price,
        yes: yes_leg,
        no: no_leg,
        yes_no_ask_sum,
        book_favorite,
        book_favorite_ask,
        book_cheap_ask,
        normalized_yes_mid,
        flow_up_minus_down_qty_60s,
        flow_score,
        btc_spot: btc.last_price,
        btc_realized_vol_5m_bps: btc.realized_vol_5m_bps,
        btc_realized_vol_15m_bps: btc.realized_vol_15m_bps,
        btc_return_30s_bps: btc.return_30s_bps,
        btc_return_60s_bps: btc.return_60s_bps,
        btc_return_120s_bps: btc.return_120s_bps,
        btc_return_180s_bps: btc.return_180s_bps,
        btc_regime: btc.regime().map(|regime| regime.to_string()),
        momentum_score,
        composite_score,
        candidate_side,
        candidate_reason,
    }
}

fn book_leg(book: &BookState, now_ms: u64) -> ShadowBookLeg {
    let (taker_buy_qty_60s, taker_sell_qty_60s) = book.taker_flow_qty_60s(now_ms);
    ShadowBookLeg {
        asset_id: book.asset_id.clone(),
        bid: book.best_bid,
        ask: book.best_ask,
        mid: book_mid(book),
        bid_size: book.best_bid_size,
        ask_size: book.best_ask_size,
        bid_depth_usd_5: depth_usd(book.bid_levels(), true, 5),
        ask_depth_usd_5: depth_usd(book.ask_levels(), false, 5),
        taker_buy_qty_60s,
        taker_sell_qty_60s,
        last_trade: book.last_trade_price,
        book_observed_at_ms: book.last_update_unix_ms,
    }
}

fn book_mid(book: &BookState) -> Option<f64> {
    finite_pair(book.best_bid, book.best_ask)
        .and_then(|(bid, ask)| (bid > 0.0 && ask > 0.0 && ask >= bid).then_some((bid + ask) * 0.5))
}

fn depth_usd(levels: &[crate::book::Level], _is_bid: bool, max_levels: usize) -> f64 {
    levels
        .iter()
        .take(max_levels)
        .filter(|level| level.price.is_finite() && level.size.is_finite())
        .filter(|level| level.price > 0.0 && level.size > 0.0)
        .map(|level| level.price * level.size)
        .sum()
}

fn finite_pair(left: f64, right: f64) -> Option<(f64, f64)> {
    (left.is_finite() && right.is_finite()).then_some((left, right))
}

fn book_favorite(yes_ask: f64, no_ask: f64) -> (Option<String>, Option<f64>, Option<f64>) {
    if !yes_ask.is_finite() || !no_ask.is_finite() || yes_ask <= 0.0 || no_ask <= 0.0 {
        return (None, None, None);
    }
    if yes_ask > no_ask {
        (Some("yes".to_string()), Some(yes_ask), Some(no_ask))
    } else if no_ask > yes_ask {
        (Some("no".to_string()), Some(no_ask), Some(yes_ask))
    } else {
        (None, Some(yes_ask), Some(no_ask))
    }
}

fn flow_up_minus_down_qty(yes: &BookState, no: &BookState, now_ms: u64) -> f64 {
    let (yes_buy, yes_sell) = yes.taker_flow_qty_60s(now_ms);
    let (no_buy, no_sell) = no.taker_flow_qty_60s(now_ms);
    (yes_buy + no_sell) - (no_buy + yes_sell)
}

fn squash(value: f64) -> f64 {
    if !value.is_finite() {
        return 0.0;
    }
    value.clamp(-1.0, 1.0)
}

fn momentum_score(btc: &BtcRegimeSnapshot) -> f64 {
    let weighted = [
        (btc.return_30s_bps, 0.20),
        (btc.return_60s_bps, 0.25),
        (btc.return_120s_bps, 0.25),
        (btc.return_180s_bps, 0.30),
    ]
    .into_iter()
    .filter_map(|(ret, weight)| ret.map(|ret| (ret, weight)))
    .filter(|(ret, _)| ret.is_finite())
    .map(|(ret, weight)| weight * (ret / 8.0).clamp(-1.0, 1.0))
    .sum::<f64>();
    weighted.clamp(-1.0, 1.0)
}

fn book_skew_score(yes: &BookState, no: &BookState) -> f64 {
    match (book_mid(yes), book_mid(no)) {
        (Some(y), Some(n)) => ((y - n) * 1.5).clamp(-0.15, 0.15),
        _ => 0.0,
    }
}

fn market_phase(market: &MarketContextRecord, now_ms: u64) -> ShadowMarketPhase {
    match (market.event_start_time_ms, market.event_end_time_ms) {
        (Some(start), Some(_)) if now_ms < start => ShadowMarketPhase::PreOpen,
        (Some(start), Some(end)) if now_ms >= start && now_ms < end => ShadowMarketPhase::Active,
        (Some(_), Some(end)) if now_ms >= end => ShadowMarketPhase::PostClose,
        _ => ShadowMarketPhase::Unknown,
    }
}

fn candidate(
    composite_score: f64,
    market: &MarketContextRecord,
    ask_sum: Option<f64>,
    book_favorite: Option<&str>,
) -> (Option<String>, String) {
    if market.price_to_beat.is_none() {
        return (None, "missing_price_to_beat".to_string());
    }
    if !matches!(ask_sum, Some(sum) if (0.90..=1.08).contains(&sum)) {
        return (None, "bad_or_missing_pair_book".to_string());
    }
    if composite_score >= 0.55 {
        return (Some("yes".to_string()), "shadow_candidate_up".to_string());
    }
    if composite_score <= -0.55 {
        return (Some("no".to_string()), "shadow_candidate_down".to_string());
    }
    if let Some(favorite) = book_favorite {
        return (None, format!("weak_composite_book_favorite_{favorite}"));
    }
    (None, "weak_composite_no_book_favorite".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::book::BookState;

    fn book(asset: &str, bid: f64, ask: f64) -> BookState {
        BookState::from_top_of_book(asset, bid, 100.0, ask, 100.0, 0.0, 1_000)
    }

    #[test]
    fn pre_open_without_price_to_beat_logs_but_never_candidates() {
        let market = MarketContextRecord {
            market_id: "m".to_string(),
            instrument_ids: vec!["yes".to_string(), "no".to_string()],
            event_start_time_ms: Some(2_000),
            event_end_time_ms: Some(302_000),
            price_to_beat: None,
            final_price: None,
        };
        let btc = BtcRegimeSnapshot {
            return_180s_bps: Some(20.0),
            return_120s_bps: Some(18.0),
            return_60s_bps: Some(12.0),
            return_30s_bps: Some(8.0),
            ..BtcRegimeSnapshot::default()
        };

        let record = build_record(
            &market,
            &book("yes", 0.48, 0.50),
            &book("no", 0.49, 0.51),
            &btc,
            1_000,
        );

        assert_eq!(record.phase, ShadowMarketPhase::PreOpen);
        assert_eq!(record.candidate_side, None);
        assert_eq!(record.candidate_reason, "missing_price_to_beat");
    }

    #[test]
    fn strong_positive_momentum_can_mark_up_candidate_after_open() {
        let market = MarketContextRecord {
            market_id: "m".to_string(),
            instrument_ids: vec!["yes".to_string(), "no".to_string()],
            event_start_time_ms: Some(1_000),
            event_end_time_ms: Some(301_000),
            price_to_beat: Some(80_000.0),
            final_price: None,
        };
        let btc = BtcRegimeSnapshot {
            return_180s_bps: Some(20.0),
            return_120s_bps: Some(18.0),
            return_60s_bps: Some(14.0),
            return_30s_bps: Some(10.0),
            realized_vol_5m_bps: Some(2.0),
            ..BtcRegimeSnapshot::default()
        };

        let record = build_record(
            &market,
            &book("yes", 0.51, 0.53),
            &book("no", 0.46, 0.48),
            &btc,
            2_000,
        );

        assert_eq!(record.phase, ShadowMarketPhase::Active);
        assert_eq!(record.candidate_side.as_deref(), Some("yes"));
        assert_eq!(record.candidate_reason, "shadow_candidate_up");
    }
}
