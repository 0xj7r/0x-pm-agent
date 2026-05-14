//! Phase 5 replay: drive a Runtime through a recorded book-snapshot log
//! deterministically, route any submits through the conservative paper
//! fill model, and write a paper report comparing decisions/fills to the
//! original session.
//!
//! Replay is sync (no tokio loop); the only asynchrony in the production
//! path is WebSocket I/O, which replay replaces with file I/O. The
//! resulting report is suitable for A/B parameter tuning against the
//! same recorded book sequence (see
//! `docs/architecture/2026-04-25-paper-env-design.md`).

use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::Deserialize;

use crate::book::{BookState, Level};
use crate::paper::report::PaperReportWriter;
use crate::types::{
    ClientOrderId, FillLiquidity, FillReport, InstrumentId, MarketId, OrderIntent, RuntimeCommand,
    RuntimeStatus, TradeSide,
};

#[derive(Debug, Deserialize)]
pub struct ReplayBookRecord {
    pub t: u64,
    pub asset: String,
    #[serde(default)]
    pub bids: Vec<[f64; 2]>,
    #[serde(default)]
    pub asks: Vec<[f64; 2]>,
    #[serde(default)]
    pub last_trade: f64,
}

impl ReplayBookRecord {
    pub fn into_book_state(self) -> BookState {
        let bids: Vec<Level> = self
            .bids
            .into_iter()
            .map(|[price, size]| Level { price, size })
            .collect();
        let asks: Vec<Level> = self
            .asks
            .into_iter()
            .map(|[price, size]| Level { price, size })
            .collect();
        let best_bid = bids.first().map(|l| l.price).unwrap_or(0.0);
        let best_bid_size = bids.first().map(|l| l.size).unwrap_or(0.0);
        let best_ask = asks.first().map(|l| l.price).unwrap_or(0.0);
        let best_ask_size = asks.first().map(|l| l.size).unwrap_or(0.0);
        let spread = if best_ask > 0.0 && best_bid > 0.0 {
            (best_ask - best_bid).max(0.0)
        } else {
            0.0
        };
        let mut book = BookState::from_top_of_book(
            self.asset,
            best_bid,
            best_bid_size,
            best_ask,
            best_ask_size,
            self.last_trade,
            self.t,
        );
        book.bids = bids;
        book.asks = asks;
        book.spread = spread;
        book.depth_update_unix_ms = self.t;
        book
    }
}

/// Read a snapshot JSONL file into a time-ordered Vec. Skips blank lines
/// and surfaces parse errors with line numbers for diagnosis.
pub fn read_snapshot_log(path: &Path) -> Result<Vec<ReplayBookRecord>> {
    let file = File::open(path)
        .with_context(|| format!("failed to open snapshot log {}", path.display()))?;
    let reader = BufReader::new(file);
    let mut records = Vec::new();
    for (idx, line) in reader.lines().enumerate() {
        let line = line.with_context(|| {
            format!(
                "failed to read snapshot log {} at line {}",
                path.display(),
                idx + 1
            )
        })?;
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let record: ReplayBookRecord = serde_json::from_str(trimmed).with_context(|| {
            format!(
                "failed to parse snapshot log {} at line {}",
                path.display(),
                idx + 1
            )
        })?;
        records.push(record);
    }
    records.sort_by_key(|r| r.t);
    Ok(records)
}

#[derive(Debug, Clone)]
pub struct ReplayConfig {
    pub input_path: PathBuf,
    pub output_report_path: PathBuf,
    pub run_id: String,
    pub market_id_by_asset: std::collections::HashMap<String, String>,
}

/// Replay summary returned by replay functions for callers that want
/// to inspect basic counts without re-reading the report file.
#[derive(Debug, Default, Clone)]
pub struct ReplayOutcome {
    pub records_consumed: usize,
    pub assets_seen: usize,
    pub report_path: PathBuf,
}

#[derive(Debug, Clone)]
struct ReplayExecutionPolicy {
    paper_mode: bool,
    paper_min_fill_notional_usd: f64,
    paper_max_fills_per_order: usize,
    paper_min_fill_interval_ms: u64,
    paper_submit_latency_ms: u64,
    paper_queue_depth_fraction: f64,
    paper_post_only_reject_probability: f64,
    paper_maker_rebate_coeff: f64,
    paper_taker_fee_coeff_override: Option<f64>,
}

impl ReplayExecutionPolicy {
    fn from_config(config: &crate::config::AppConfig) -> Self {
        Self {
            paper_mode: true,
            paper_min_fill_notional_usd: config.paper_min_fill_notional_usd,
            paper_max_fills_per_order: config.paper_max_fills_per_order,
            paper_min_fill_interval_ms: config.paper_min_fill_interval.as_millis() as u64,
            paper_submit_latency_ms: config.paper_submit_latency_ms,
            paper_queue_depth_fraction: config.paper_queue_depth_fraction,
            paper_post_only_reject_probability: config.paper_post_only_reject_probability,
            paper_maker_rebate_coeff: config.paper_maker_rebate_coeff,
            paper_taker_fee_coeff_override: config.paper_taker_fee_coeff_override,
        }
    }
}

#[derive(Debug, Clone)]
struct PaperOrderContext {
    arrival_ms: u64,
    queue_bias: f64,
    last_attempt_ms: u64,
    last_fill_ms: u64,
    last_fill_book_update_ms: u64,
    fill_count: usize,
}

fn paper_order_context_mut<'a>(
    paper_order_ctx: &'a mut std::collections::HashMap<ClientOrderId, PaperOrderContext>,
    intent: &OrderIntent,
    now_ms: u64,
) -> &'a mut PaperOrderContext {
    let state = PaperOrderContext {
        arrival_ms: intent.created_at_ms.min(now_ms),
        queue_bias: deterministic_hash_0_95(intent.client_order_id.as_str()),
        last_attempt_ms: now_ms,
        last_fill_ms: 0,
        last_fill_book_update_ms: 0,
        fill_count: 0,
    };
    let ctx = paper_order_ctx
        .entry(intent.client_order_id.clone())
        .or_insert(state);
    ctx.last_attempt_ms = now_ms;
    ctx
}

fn deterministic_hash_0_95(value: &str) -> f64 {
    let mut hash = 0xcbf29ce484222325u64;
    for byte in value.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    let normalized = (hash & 0xffff) as f64 / 65_536.0;
    (0.05 + (normalized * 0.95)).min(1.0)
}

fn deterministic_unit_hash(client_order_id: &str, book_update_ms: u64) -> f64 {
    let mut hash = 0xcbf29ce484222325u64;
    for byte in client_order_id.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    for byte in book_update_ms.to_le_bytes() {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    (hash & 0xffff_ffff) as f64 / 4_294_967_296.0
}

fn paper_post_only_should_reject(
    intent: &OrderIntent,
    book: &BookState,
    execution_policy: &ReplayExecutionPolicy,
) -> bool {
    if !execution_policy.paper_mode {
        return false;
    }
    if execution_policy.paper_post_only_reject_probability <= 0.0 {
        return false;
    }
    let crossing = if matches!(intent.side, TradeSide::Buy) {
        book.best_ask > 0.0 && intent.limit_price >= book.best_ask
    } else {
        book.best_bid > 0.0 && intent.limit_price <= book.best_bid
    };
    if !crossing {
        return false;
    }
    let roll = deterministic_unit_hash(intent.client_order_id.as_str(), book.last_update_unix_ms);
    roll < execution_policy.paper_post_only_reject_probability
}

fn paper_fill_ratio(
    order_qty: f64,
    available_qty: f64,
    snapshot_unix_ms: u64,
    fill_price: f64,
    limit_price: f64,
    order_ctx: &PaperOrderContext,
    crossing: bool,
    observed_at_ms: u64,
) -> f64 {
    if order_qty <= 0.0 || available_qty <= 0.0 || fill_price <= 0.0 || limit_price <= 0.0 {
        return 0.0;
    }

    let age_ms = observed_at_ms.saturating_sub(order_ctx.arrival_ms.max(order_ctx.last_attempt_ms));
    let age_pressure = if crossing {
        0.15 + 0.30 * ((age_ms as f64 / 3_000.0).clamp(0.0, 1.0))
    } else {
        0.02 + 0.18 * ((age_ms as f64 / 5_000.0).clamp(0.0, 1.0))
    };
    let size_pressure = 0.25 + 0.75 * (available_qty / (available_qty + order_qty));
    let queue_pressure = 0.08 + order_ctx.queue_bias * 0.52;
    let staleness_pressure = 0.30
        + 0.60
            * ((observed_at_ms.saturating_sub(snapshot_unix_ms) as f64 / 2_000.0).clamp(0.0, 1.0));
    let premium = ((limit_price - fill_price) / fill_price).max(0.0).min(1.0);
    let limit_pressure = if crossing {
        0.65
    } else {
        0.20 + (premium * 0.20)
    };
    (age_pressure * size_pressure * queue_pressure * staleness_pressure * limit_pressure)
        .clamp(0.0, if crossing { 0.65 } else { 0.20 })
}

fn paper_fill_from_book_snapshot(
    book: &BookState,
    intent: &OrderIntent,
    observed_at_ms: u64,
    paper_fee_coeff: f64,
    order_ctx: &mut PaperOrderContext,
    remaining_qty: f64,
    execution_policy: &ReplayExecutionPolicy,
) -> Option<FillReport> {
    if remaining_qty <= 0.0 || intent.limit_price <= 0.0 {
        return None;
    }
    if order_ctx.fill_count >= execution_policy.paper_max_fills_per_order {
        return None;
    }
    if execution_policy.paper_submit_latency_ms > 0
        && observed_at_ms.saturating_sub(order_ctx.arrival_ms)
            < execution_policy.paper_submit_latency_ms
    {
        return None;
    }
    if order_ctx.last_fill_ms > 0
        && observed_at_ms.saturating_sub(order_ctx.last_fill_ms)
            < execution_policy.paper_min_fill_interval_ms
    {
        return None;
    }
    if book.last_update_unix_ms > 0
        && order_ctx.last_fill_book_update_ms == book.last_update_unix_ms
    {
        return None;
    }

    let candidate_levels: Vec<_> = if matches!(intent.side, TradeSide::Buy) {
        book.ask_levels()
            .iter()
            .filter(|level| level.price > 0.0 && level.price <= intent.limit_price)
            .collect()
    } else {
        book.bid_levels()
            .iter()
            .filter(|level| level.price > 0.0 && level.price >= intent.limit_price)
            .collect()
    };
    if candidate_levels.is_empty() {
        return None;
    }

    let total_available: f64 = candidate_levels.iter().map(|level| level.size).sum();
    if total_available <= 0.0 {
        return None;
    }

    let best_opposite = candidate_levels[0].price;
    let crossing = if matches!(intent.side, TradeSide::Buy) {
        book.best_ask > 0.0 && intent.limit_price >= book.best_ask
    } else {
        book.best_bid > 0.0 && intent.limit_price <= book.best_bid
    };
    let maker_trade_through = if matches!(intent.side, TradeSide::Buy) {
        book.last_trade_price > 0.0 && book.last_trade_price <= intent.limit_price
    } else {
        book.last_trade_price > 0.0 && book.last_trade_price >= intent.limit_price
    };

    let order_age_ms = observed_at_ms.saturating_sub(order_ctx.arrival_ms);
    let is_resting = order_age_ms > execution_policy.paper_submit_latency_ms;
    let crosses_as_taker = crossing && !is_resting;
    if !crossing {
        let queue_wait_ms = 1_000 + (order_ctx.queue_bias * 3_000.0) as u64;
        if order_age_ms < queue_wait_ms || !maker_trade_through {
            return None;
        }
    }
    let best_fill_price = if crosses_as_taker {
        best_opposite
    } else {
        intent.limit_price
    };
    let fill_ratio = paper_fill_ratio(
        remaining_qty,
        total_available,
        book.last_update_unix_ms,
        best_fill_price,
        intent.limit_price,
        order_ctx,
        crossing,
        observed_at_ms,
    );
    let target_fill_qty = (remaining_qty * fill_ratio)
        .min(total_available)
        .min(remaining_qty);
    if target_fill_qty <= 0.0 {
        return None;
    }

    let mut remaining = target_fill_qty;
    let mut qty_filled = 0.0;
    let mut amount = 0.0;
    for (idx, level) in candidate_levels.iter().enumerate() {
        if remaining <= 0.0 {
            break;
        }
        let level_ratio = if crossing {
            1.0
        } else if idx == 0 {
            (1.0 - execution_policy.paper_queue_depth_fraction).max(0.0)
        } else {
            0.0
        };
        let level_fill = (level.size * level_ratio).min(remaining);
        if level_fill > 0.0 {
            qty_filled += level_fill;
            let price_at_level = if crosses_as_taker {
                level.price
            } else {
                intent.limit_price
            };
            amount += level_fill * price_at_level;
            remaining -= level_fill;
        }
    }

    if qty_filled <= 0.0 {
        qty_filled = target_fill_qty.min(candidate_levels[0].size);
        let fallback_price = if crosses_as_taker {
            candidate_levels[0].price
        } else {
            intent.limit_price
        };
        amount = qty_filled * fallback_price;
    }

    if qty_filled <= 0.0 {
        return None;
    }

    let price = if qty_filled > 0.0 {
        amount / qty_filled
    } else {
        0.0
    };
    if price <= 0.0 {
        return None;
    }

    let liquidity = if crosses_as_taker {
        FillLiquidity::Taker
    } else {
        FillLiquidity::Maker
    };
    let notional = qty_filled * price;
    if notional < execution_policy.paper_min_fill_notional_usd
        && (remaining_qty * price) >= execution_policy.paper_min_fill_notional_usd
    {
        return None;
    }
    let effective_taker_coeff = execution_policy
        .paper_taker_fee_coeff_override
        .unwrap_or(paper_fee_coeff);
    let fee_basis = price * (1.0 - price);
    let fee = match liquidity {
        FillLiquidity::Maker => -(notional * execution_policy.paper_maker_rebate_coeff * fee_basis),
        FillLiquidity::Taker => notional * effective_taker_coeff * fee_basis,
        FillLiquidity::Unknown => notional * effective_taker_coeff * fee_basis,
    };
    order_ctx.last_fill_ms = observed_at_ms;
    order_ctx.last_fill_book_update_ms = book.last_update_unix_ms;
    order_ctx.fill_count = order_ctx.fill_count.saturating_add(1);

    Some(FillReport {
        order_id: None,
        client_order_id: Some(intent.client_order_id.clone()),
        market_id: intent.market_id.clone(),
        instrument_id: intent.instrument_id.clone(),
        side: intent.side,
        price,
        quantity: qty_filled,
        fee_usd: fee,
        liquidity,
        close_method: None,
        observed_at_ms,
    })
}

fn now_unix_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

/// Full deterministic replay using the same Runtime + Strategy path as
/// live/shadow-live runs, but with paper fills over recorded books.
pub async fn replay_runtime_from_snapshots(
    config: &crate::config::AppConfig,
    input_path: &Path,
    output_path: &Path,
) -> Result<ReplayOutcome> {
    let records = read_snapshot_log(input_path)?;
    let started_at_ms = records.first().map(|r| r.t).unwrap_or_else(now_unix_ms);

    let strategy = crate::strategy::StrategyMode::try_from_name(
        &config.strategy_name,
        config.strategy_profile.as_ref(),
    )
    .map_err(anyhow::Error::msg)?;
    let replay_taker_fee_coeff = strategy.taker_fee_coeff();
    let pair_profile = config
        .strategy_profile
        .as_ref()
        .map(|profile| &profile.pair);
    let runtime_defaults = crate::runtime::types::RuntimeConfig::default();
    let mut runtime = crate::runtime::Runtime::new(
        crate::runtime::types::RuntimeConfig {
            starting_cash_usd: config.starting_cash_usd,
            event_log_capacity: config.event_log_capacity,
            initial_status: RuntimeStatus::Running,
            quote_engine_config: crate::quote_engine::QuoteEngineConfig::default(),
            quote_stale_ms: config.quote_min_order_age.as_millis() as u64,
            require_initial_reconcile_before_entry: false,
            merge_enabled: pair_profile
                .and_then(|pair| pair.merge_enabled)
                .unwrap_or(runtime_defaults.merge_enabled),
            pressure_merge_enabled: pair_profile
                .and_then(|pair| pair.pressure_merge_enabled)
                .unwrap_or(runtime_defaults.pressure_merge_enabled),
            min_merge_notional_usd: config
                .strategy_profile
                .as_ref()
                .and_then(|profile| profile.pair.min_merge_notional_usd)
                .unwrap_or(runtime_defaults.min_merge_notional_usd),
            merge_free_cash_pressure_ratio: pair_profile
                .and_then(|pair| pair.merge_pressure_free_cash_ratio)
                .unwrap_or(runtime_defaults.merge_free_cash_pressure_ratio),
            merge_gross_exposure_pressure_ratio: pair_profile
                .and_then(|pair| pair.merge_pressure_gross_exposure_ratio)
                .unwrap_or(runtime_defaults.merge_gross_exposure_pressure_ratio),
            merge_market_exposure_pressure_usd: pair_profile
                .and_then(|pair| pair.merge_market_exposure_pressure_usd)
                .unwrap_or(runtime_defaults.merge_market_exposure_pressure_usd),
        },
        config.risk_limits.clone(),
        strategy,
        crate::market_context::MarketContextStore::empty(),
    );

    let execution_policy = ReplayExecutionPolicy::from_config(config);
    let mut paper_order_ctx: std::collections::HashMap<ClientOrderId, PaperOrderContext> =
        std::collections::HashMap::new();
    let mut report = PaperReportWriter::new(
        format!("replay-{}", now_unix_ms()),
        "replay",
        output_path.to_path_buf(),
        started_at_ms,
    );

    let mut assets = std::collections::HashSet::new();

    for record in records.iter() {
        assets.insert(record.asset.clone());
        let asset = record.asset.clone();
        let book = ReplayBookRecord {
            t: record.t,
            asset: record.asset.clone(),
            bids: record.bids.clone(),
            asks: record.asks.clone(),
            last_trade: record.last_trade,
        }
        .into_book_state();

        let asset_instrument = InstrumentId::from(asset.as_str());
        let retry_targets: Vec<crate::runtime::types::ManagedOrder> = runtime
            .open_order_snapshots()
            .into_iter()
            .filter(|m| m.intent.instrument_id == asset_instrument && m.remaining_qty() > 1e-9)
            .collect();
        for managed in retry_targets {
            let intent = managed.intent.clone();
            let mid_at_submit = if book.best_bid > 0.0 && book.best_ask > 0.0 {
                Some((book.best_bid + book.best_ask) * 0.5)
            } else {
                None
            };
            let ctx = paper_order_context_mut(&mut paper_order_ctx, &intent, record.t);
            if let Some(fill) = paper_fill_from_book_snapshot(
                &book,
                &intent,
                record.t,
                replay_taker_fee_coeff,
                ctx,
                managed.remaining_qty(),
                &execution_policy,
            ) {
                report.record_fill(&fill, mid_at_submit);
                runtime.on_fill(fill)?;
            }
        }

        let market_id = MarketId::from(config.market_id_for_asset(&asset));
        let instrument_id = InstrumentId::from(asset.as_str());

        let outcome = runtime.on_book_state(market_id.clone(), instrument_id.clone(), &book)?;
        for command in outcome.commands {
            match command {
                RuntimeCommand::Submit(intent) => {
                    if paper_post_only_should_reject(&intent, &book, &execution_policy) {
                        report.record_reject(
                            &intent.client_order_id,
                            &intent.market_id,
                            &intent.instrument_id,
                            intent.limit_price,
                            "post-only-cross-paper",
                            record.t,
                        );
                        runtime.on_order_rejected(
                            &intent.client_order_id,
                            "post-only-cross-paper",
                            record.t,
                        );
                        continue;
                    }
                    let mid_at_submit = if book.best_bid > 0.0 && book.best_ask > 0.0 {
                        Some((book.best_bid + book.best_ask) * 0.5)
                    } else {
                        None
                    };
                    if let Some(mid) = mid_at_submit {
                        report.record_submit_edge(
                            intent.side,
                            intent.limit_price,
                            intent.quantity,
                            mid,
                            record.t,
                        );
                    }
                    let ctx = paper_order_context_mut(&mut paper_order_ctx, &intent, record.t);
                    if let Some(fill) = paper_fill_from_book_snapshot(
                        &book,
                        &intent,
                        record.t,
                        replay_taker_fee_coeff,
                        ctx,
                        intent.quantity,
                        &execution_policy,
                    ) {
                        report.record_fill(&fill, mid_at_submit);
                        runtime.on_fill(fill)?;
                    } else {
                        runtime.on_order_opened(&intent.client_order_id, record.t);
                    }
                }
                RuntimeCommand::Cancel {
                    client_order_id,
                    reason,
                } => {
                    paper_order_ctx.remove(&client_order_id);
                    runtime.on_order_cancelled(&client_order_id, reason, record.t);
                }
                RuntimeCommand::Merge(intent) => {
                    let fill = FillReport {
                        order_id: None,
                        client_order_id: Some(intent.command_id.clone()),
                        market_id: intent.market_id.clone(),
                        instrument_id: intent.yes_instrument_id.clone(),
                        side: TradeSide::Buy,
                        price: 1.0,
                        quantity: intent.quantity,
                        fee_usd: intent.expected_fee_usd + intent.expected_gas_usd,
                        liquidity: FillLiquidity::Unknown,
                        close_method: Some(crate::types::CloseMethod::Merge),
                        observed_at_ms: record.t,
                    };
                    report.record_fill(&fill, None);
                    runtime.on_fill(fill)?;
                }
                RuntimeCommand::Redeem(_) | RuntimeCommand::Noop => {
                    // Replay is venue-free; unresolved redeem commands need
                    // winner-leg context before inventory can be closed.
                }
            }
        }
    }

    report.flush()?;

    Ok(ReplayOutcome {
        records_consumed: records.len(),
        assets_seen: assets.len(),
        report_path: output_path.to_path_buf(),
    })
}

/// Backward-compatible summary wrapper used by older callers/tests.
pub fn replay_into_report(cfg: ReplayConfig) -> Result<ReplayOutcome> {
    let records = read_snapshot_log(&cfg.input_path)?;
    let mut report = PaperReportWriter::new(
        cfg.run_id.clone(),
        "replay",
        cfg.output_report_path.clone(),
        records.first().map(|r| r.t).unwrap_or(0),
    );
    let mut assets = std::collections::HashSet::new();
    for record in &records {
        assets.insert(record.asset.clone());
    }
    report.record_reject(
        &ClientOrderId::from(format!("replay-input")),
        &MarketId::from(
            cfg.market_id_by_asset
                .values()
                .next()
                .cloned()
                .unwrap_or_else(|| "replay".to_string()),
        ),
        &InstrumentId::from("replay-input"),
        0.0,
        format!("replay input: {}", cfg.input_path.display()),
        records.first().map(|r| r.t).unwrap_or(0),
    );
    report.flush()?;
    Ok(ReplayOutcome {
        records_consumed: records.len(),
        assets_seen: assets.len(),
        report_path: cfg.output_report_path,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::paper::snapshot::BookSnapshotWriter;

    fn write_test_snapshot_log(path: &Path) {
        let _ = std::fs::remove_file(path);
        let mut writer = BookSnapshotWriter::open(path).expect("open writer");
        for t in [1_000u64, 2_000, 3_000] {
            let mut book = BookState::default();
            book.asset_id = "asset-1".to_string();
            book.best_bid = 0.40;
            book.best_bid_size = 100.0;
            book.best_ask = 0.42;
            book.best_ask_size = 100.0;
            book.last_trade_price = 0.41;
            book.last_update_unix_ms = t;
            book.bids = vec![Level {
                price: 0.40,
                size: 100.0,
            }];
            book.asks = vec![Level {
                price: 0.42,
                size: 100.0,
            }];
            writer.record(&book, 5).expect("record");
        }
    }

    #[test]
    fn read_snapshot_log_parses_jsonl_in_time_order() {
        let path = std::env::temp_dir().join(format!("replay-read-{}.jsonl", std::process::id()));
        write_test_snapshot_log(&path);
        let records = read_snapshot_log(&path).expect("read");
        assert_eq!(records.len(), 3);
        assert!(records[0].t < records[1].t && records[1].t < records[2].t);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn replay_into_report_writes_summary_for_recorded_session() {
        let input = std::env::temp_dir().join(format!("replay-input-{}.jsonl", std::process::id()));
        let output =
            std::env::temp_dir().join(format!("replay-output-{}.json", std::process::id()));
        let _ = std::fs::remove_file(&input);
        let _ = std::fs::remove_file(&output);
        write_test_snapshot_log(&input);

        let cfg = ReplayConfig {
            input_path: input.clone(),
            output_report_path: output.clone(),
            run_id: "test-replay".to_string(),
            market_id_by_asset: std::collections::HashMap::new(),
        };
        let outcome = replay_into_report(cfg).expect("replay ok");
        assert_eq!(outcome.records_consumed, 3);
        assert_eq!(outcome.assets_seen, 1);

        let body = std::fs::read_to_string(&output).expect("read report");
        let parsed: serde_json::Value = serde_json::from_str(&body).expect("valid json");
        assert_eq!(parsed["session"]["mode"].as_str(), Some("replay"));

        let _ = std::fs::remove_file(&input);
        let _ = std::fs::remove_file(&output);
    }

    #[test]
    fn into_book_state_reconstructs_top_of_book_correctly() {
        let record = ReplayBookRecord {
            t: 1_000,
            asset: "asset-1".to_string(),
            bids: vec![[0.40, 100.0], [0.39, 200.0]],
            asks: vec![[0.42, 80.0], [0.43, 150.0]],
            last_trade: 0.41,
        };
        let book = record.into_book_state();
        assert_eq!(book.asset_id, "asset-1");
        assert_eq!(book.best_bid, 0.40);
        assert_eq!(book.best_ask, 0.42);
        assert_eq!(book.bids.len(), 2);
        assert_eq!(book.asks.len(), 2);
        assert!((book.spread - 0.02).abs() < 1e-9);
    }
}
