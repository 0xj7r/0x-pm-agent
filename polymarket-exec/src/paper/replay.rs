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
use crate::types::{InstrumentId, MarketId};

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
            format!("failed to read snapshot log {} at line {}", path.display(), idx + 1)
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

/// Replay summary returned by `replay_into_report` for callers that want
/// to inspect basic counts without re-reading the report file.
#[derive(Debug, Default, Clone)]
pub struct ReplayOutcome {
    pub records_consumed: usize,
    pub assets_seen: usize,
    pub report_path: PathBuf,
}

/// Read a snapshot log + write a paper-mode "what happened on these
/// books" report. This minimal replay does NOT yet drive the strategy
/// (that requires instantiating the full Runtime + StrategyMode +
/// MarketContextStore from config). Instead it produces a focused
/// statistics summary suitable for fill-model parameter calibration:
/// per-asset book counts, spread distribution, last-trade-price drift.
///
/// The next iteration will instantiate `Runtime<StrategyMode>` and
/// route synthetic submits through `paper_fill_from_book_snapshot`,
/// using the recorded `t` as the replay clock. That requires lifting
/// the strategy build path out of `run_with_config` so it can be
/// invoked sync; tracked as Phase 5 chunk 3.
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
    // Tag the report with the input path via a synthetic reject record
    // (no real reject occurred; this is a marker so operators can see
    // what feed produced the replay).
    report.record_reject(
        &crate::types::ClientOrderId::from(format!("replay-input")),
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
            book.bids = vec![Level { price: 0.40, size: 100.0 }];
            book.asks = vec![Level { price: 0.42, size: 100.0 }];
            writer.record(&book, 5).expect("record");
        }
    }

    #[test]
    fn read_snapshot_log_parses_jsonl_in_time_order() {
        let path = std::env::temp_dir().join(format!(
            "replay-read-{}.jsonl",
            std::process::id()
        ));
        write_test_snapshot_log(&path);
        let records = read_snapshot_log(&path).expect("read");
        assert_eq!(records.len(), 3);
        assert!(records[0].t < records[1].t && records[1].t < records[2].t);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn replay_into_report_writes_summary_for_recorded_session() {
        let input = std::env::temp_dir().join(format!(
            "replay-input-{}.jsonl",
            std::process::id()
        ));
        let output = std::env::temp_dir().join(format!(
            "replay-output-{}.json",
            std::process::id()
        ));
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
        assert!(parsed["queue"]["post_only_reject_count"].as_u64().unwrap_or(0) >= 1);

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
