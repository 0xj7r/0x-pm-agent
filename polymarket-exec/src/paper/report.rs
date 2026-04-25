//! Paper-mode session report card. Accumulates per-fill records and
//! per-reject counts during a paper run, writes a JSON summary on flush.
//! Phase 3 of the paper env design.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::Serialize;

use crate::types::{ClientOrderId, FillLiquidity, FillReport, InstrumentId, MarketId, TradeSide};

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
        file.write_all(body.as_bytes())
            .with_context(|| {
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::CloseMethod;

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

    #[test]
    fn paper_report_summary_aggregates_maker_taker_and_edge() {
        let mut writer = PaperReportWriter::new(
            "run-test",
            "paper",
            PathBuf::from("/tmp/_unused"),
            1_000,
        );
        // Maker buy at 0.50, mid was 0.52 → expected edge 0.02 * 10 = +0.20
        writer.record_submit_edge(TradeSide::Buy, 0.50, 10.0, 0.52, 1_000);
        writer.record_fill(&fill(TradeSide::Buy, 0.50, 10.0, FillLiquidity::Maker, 1_100), Some(0.52));
        // Taker buy at 0.55 (limit=0.55, mid was 0.53) → expected edge -0.02 * 5 = -0.10
        writer.record_submit_edge(TradeSide::Buy, 0.55, 5.0, 0.53, 1_200);
        writer.record_fill(&fill(TradeSide::Buy, 0.55, 5.0, FillLiquidity::Taker, 1_250), Some(0.53));

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
    fn paper_report_summary_handles_empty_session() {
        let writer = PaperReportWriter::new("empty", "paper", PathBuf::from("/tmp/_unused"), 0);
        let summary = writer.summary();
        assert_eq!(summary.fills.total_count, 0);
        assert_eq!(summary.fills.maker_fraction, 0.0);
        assert_eq!(summary.edge.expected_edge_usd, 0.0);
        assert!(summary.edge.edge_capture_ratio.is_none());
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
        let tmp = std::env::temp_dir().join(format!(
            "paper-report-flush-{}.json",
            std::process::id()
        ));
        let mut writer = PaperReportWriter::new("flush", "paper", tmp.clone(), 0);
        writer.record_fill(&fill(TradeSide::Buy, 0.5, 1.0, FillLiquidity::Maker, 1), None);
        writer.flush().expect("flush ok");
        let body = std::fs::read_to_string(&tmp).expect("read back");
        let parsed: serde_json::Value = serde_json::from_str(&body).expect("valid json");
        assert_eq!(parsed["fills"]["total_count"].as_u64(), Some(1));
        let _ = std::fs::remove_file(&tmp);
    }
}
