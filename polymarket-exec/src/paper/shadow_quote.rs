//! Shadow quote logging for paper/shadow-live calibration.
//!
//! Each record captures an order the engine would submit plus the top-N book
//! state at that exact decision point. The offline calibration tool replays
//! these records against `BookSnapshotWriter` output to estimate whether paper
//! fills are optimistic, conservative, or plausible.

use std::fs::{self, File, OpenOptions};
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::book::BookState;
use crate::types::{OrderIntent, TradeSide};

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ShadowBookLevel {
    pub price: f64,
    pub size: f64,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ShadowQuoteRecord {
    pub observed_at_ms: u64,
    pub market_id: String,
    pub instrument_id: String,
    pub client_order_id: String,
    pub side: TradeSide,
    pub limit_price: f64,
    pub quantity: f64,
    pub notional_usd: f64,
    pub intent_kind: String,
    pub quote_level_tag: Option<String>,
    pub reason: String,
    pub price_to_beat: Option<f64>,
    pub btc_spot: Option<f64>,
    pub btc_realized_vol_5m_bps: Option<f64>,
    pub book_observed_at_ms: u64,
    pub best_bid: f64,
    pub best_ask: f64,
    pub mid: Option<f64>,
    pub last_trade: f64,
    pub bids: Vec<ShadowBookLevel>,
    pub asks: Vec<ShadowBookLevel>,
}

pub struct ShadowQuoteWriter {
    path: PathBuf,
    max_levels: usize,
    file: BufWriter<File>,
}

impl ShadowQuoteWriter {
    pub fn open(path: &Path, max_levels: usize) -> Result<Self> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).with_context(|| {
                format!(
                    "failed to create shadow quote directory {}",
                    parent.display()
                )
            })?;
        }
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .with_context(|| format!("failed to open shadow quote log {}", path.display()))?;
        Ok(Self {
            path: path.to_path_buf(),
            max_levels,
            file: BufWriter::new(file),
        })
    }

    pub fn record(
        &mut self,
        intent: &OrderIntent,
        book: &BookState,
        observed_at_ms: u64,
        price_to_beat: Option<f64>,
        btc_spot: Option<f64>,
        btc_realized_vol_5m_bps: Option<f64>,
    ) -> Result<()> {
        let mid = if book.best_bid > 0.0 && book.best_ask > 0.0 {
            Some((book.best_bid + book.best_ask) * 0.5)
        } else {
            None
        };
        let record = ShadowQuoteRecord {
            observed_at_ms,
            market_id: intent.market_id.as_str().to_string(),
            instrument_id: intent.instrument_id.as_str().to_string(),
            client_order_id: intent.client_order_id.as_str().to_string(),
            side: intent.side,
            limit_price: intent.limit_price,
            quantity: intent.quantity,
            notional_usd: intent.notional_usd(),
            intent_kind: format!("{:?}", intent.kind),
            quote_level_tag: intent.quote_level_tag.clone(),
            reason: intent.reason.clone(),
            price_to_beat,
            btc_spot,
            btc_realized_vol_5m_bps,
            book_observed_at_ms: book.last_update_unix_ms,
            best_bid: book.best_bid,
            best_ask: book.best_ask,
            mid,
            last_trade: book.last_trade_price,
            bids: book
                .bids
                .iter()
                .take(self.max_levels)
                .map(|level| ShadowBookLevel {
                    price: level.price,
                    size: level.size,
                })
                .collect(),
            asks: book
                .asks
                .iter()
                .take(self.max_levels)
                .map(|level| ShadowBookLevel {
                    price: level.price,
                    size: level.size,
                })
                .collect(),
        };
        let line = serde_json::to_string(&record).context("failed to serialize shadow quote")?;
        self.file.write_all(line.as_bytes())?;
        self.file.write_all(b"\n")?;
        self.file.flush()?;
        Ok(())
    }

    pub fn flush(&mut self) -> Result<()> {
        self.file.flush()?;
        Ok(())
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for ShadowQuoteWriter {
    fn drop(&mut self) {
        let _ = self.file.flush();
    }
}
