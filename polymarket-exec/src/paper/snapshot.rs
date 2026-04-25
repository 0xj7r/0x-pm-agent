//! Compact JSONL writer for book snapshots. Phase 5 of the paper env
//! design: enables deterministic post-hoc replay of a recorded session
//! through the engine.

use std::fs::{self, File, OpenOptions};
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::Serialize;

use crate::book::BookState;

/// One JSONL line per book update. Compact field names to keep file size
/// manageable for multi-day sessions (~1-2 updates/sec/asset → ~10MB/24h).
#[derive(Debug, Serialize)]
pub struct BookSnapshotRecord<'a> {
    /// `last_update_unix_ms` of the book at write time.
    pub t: u64,
    pub asset: &'a str,
    /// Top-N bids as [price, size] pairs (ordered as the venue sent them;
    /// typically descending price).
    pub bids: Vec<[f64; 2]>,
    pub asks: Vec<[f64; 2]>,
    pub last_trade: f64,
}

pub struct BookSnapshotWriter {
    path: PathBuf,
    file: BufWriter<File>,
    bytes_written: u64,
}

impl BookSnapshotWriter {
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).with_context(|| {
                format!(
                    "failed to create book snapshot directory {}",
                    parent.display()
                )
            })?;
        }
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .with_context(|| format!("failed to open book snapshot log {}", path.display()))?;
        let bytes_written = file
            .metadata()
            .map(|meta| meta.len())
            .unwrap_or(0);
        Ok(Self {
            path: path.to_path_buf(),
            file: BufWriter::new(file),
            bytes_written,
        })
    }

    /// Append one record built from a `BookState`. Truncates depth to
    /// `max_levels` per side. No-op if `last_update_unix_ms` is 0
    /// (uninitialised book).
    pub fn record(&mut self, book: &BookState, max_levels: usize) -> Result<()> {
        if book.last_update_unix_ms == 0 {
            return Ok(());
        }
        let bids: Vec<[f64; 2]> = book
            .bids
            .iter()
            .take(max_levels)
            .map(|level| [level.price, level.size])
            .collect();
        let asks: Vec<[f64; 2]> = book
            .asks
            .iter()
            .take(max_levels)
            .map(|level| [level.price, level.size])
            .collect();
        let record = BookSnapshotRecord {
            t: book.last_update_unix_ms,
            asset: book.asset_id.as_str(),
            bids,
            asks,
            last_trade: book.last_trade_price,
        };
        let line = serde_json::to_string(&record)
            .context("failed to serialize BookSnapshotRecord")?;
        self.file.write_all(line.as_bytes())?;
        self.file.write_all(b"\n")?;
        self.bytes_written = self.bytes_written.saturating_add(line.len() as u64 + 1);
        Ok(())
    }

    pub fn flush(&mut self) -> Result<()> {
        self.file.flush()?;
        Ok(())
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn bytes_written(&self) -> u64 {
        self.bytes_written
    }
}

impl Drop for BookSnapshotWriter {
    fn drop(&mut self) {
        let _ = self.file.flush();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::book::Level;

    fn book_with_levels() -> BookState {
        let mut book = BookState::default();
        book.asset_id = "asset-1".to_string();
        book.best_bid = 0.42;
        book.best_bid_size = 100.0;
        book.best_ask = 0.44;
        book.best_ask_size = 80.0;
        book.last_trade_price = 0.43;
        book.last_update_unix_ms = 1_700_000_000_000;
        book.bids = vec![
            Level { price: 0.42, size: 100.0 },
            Level { price: 0.41, size: 200.0 },
            Level { price: 0.40, size: 300.0 },
        ];
        book.asks = vec![
            Level { price: 0.44, size: 80.0 },
            Level { price: 0.45, size: 150.0 },
        ];
        book
    }

    #[test]
    fn book_snapshot_writer_appends_jsonl_lines() {
        let path = std::env::temp_dir().join(format!(
            "book-snap-{}-{}.jsonl",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        let _ = std::fs::remove_file(&path);
        let book = book_with_levels();
        {
            let mut w = BookSnapshotWriter::open(&path).expect("open ok");
            w.record(&book, 5).expect("record ok");
            w.record(&book, 5).expect("record ok");
            w.flush().expect("flush ok");
        }
        let body = std::fs::read_to_string(&path).expect("read back");
        let lines: Vec<&str> = body.lines().collect();
        assert_eq!(lines.len(), 2);
        let first: serde_json::Value = serde_json::from_str(lines[0]).expect("valid json");
        assert_eq!(first["t"].as_u64(), Some(1_700_000_000_000));
        assert_eq!(first["asset"].as_str(), Some("asset-1"));
        assert_eq!(first["bids"].as_array().map(|a| a.len()), Some(3));
        assert_eq!(first["asks"].as_array().map(|a| a.len()), Some(2));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn book_snapshot_writer_truncates_to_max_levels() {
        let path = std::env::temp_dir().join(format!(
            "book-snap-trunc-{}.jsonl",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&path);
        let book = book_with_levels();
        {
            let mut w = BookSnapshotWriter::open(&path).expect("open ok");
            w.record(&book, 1).expect("record ok");
            w.flush().expect("flush ok");
        }
        let body = std::fs::read_to_string(&path).expect("read back");
        let parsed: serde_json::Value = serde_json::from_str(body.trim()).expect("valid");
        assert_eq!(parsed["bids"].as_array().map(|a| a.len()), Some(1));
        assert_eq!(parsed["asks"].as_array().map(|a| a.len()), Some(1));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn book_snapshot_writer_skips_uninitialised_book() {
        let path = std::env::temp_dir().join(format!(
            "book-snap-skip-{}.jsonl",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&path);
        let book = BookState::default();
        {
            let mut w = BookSnapshotWriter::open(&path).expect("open ok");
            w.record(&book, 5).expect("record ok");
            w.flush().expect("flush ok");
        }
        let body = std::fs::read_to_string(&path).unwrap_or_default();
        assert!(body.is_empty(), "expected no record for uninitialised book");
        let _ = std::fs::remove_file(&path);
    }
}
