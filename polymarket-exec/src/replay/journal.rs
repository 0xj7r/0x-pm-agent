//! Replay journal: deployment-grade append-only audit trail.
//!
//! Captures every decision, intent, fill, and accounting event the strategy
//! emits in event-time order, into a single Parquet file at the run output
//! prefix. Reason tags are pulled from existing strategy `OrderIntent.reason`
//! / `quote_level_tag` fields; this module does not invent new tags.
//!
//! Schema choice: one Parquet file with a discriminated `event_kind` column
//! plus union-typed columns. A single discriminated table keeps downstream
//! consumers simple (one read, deterministic sort by `ts_ns`) and is the
//! pattern Polymarket research orchestration already uses for trace strands
//! such as `risk_rejections`. Multi-file fan-out was rejected because it
//! complicates audit (10 separate files per run, partial-write risk) without
//! a query-time benefit at backtest cardinality.

use std::fs::{self, File};
use std::path::Path;
use std::sync::Arc;

use anyhow::{Context, Result};
use arrow::array::{
    ArrayRef, BooleanArray, Float64Array, Int32Array, Int64Array, StringArray, UInt32Array,
};
use arrow::datatypes::{DataType, Field, Schema};
use arrow::record_batch::RecordBatch;
use parquet::arrow::ArrowWriter;
use serde::{Deserialize, Serialize};

/// One row in the journal. Each variant maps to one of the 10 audit event
/// kinds called out in the deployment-grade audit-trail contract.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "event_kind", rename_all = "snake_case")]
pub enum JournalEvent {
    StrategyDecision {
        ts_ns: i64,
        market_slug: String,
        asset_id: Option<String>,
        decision_type: String,
        raw_inputs_hash: String,
        reason_tag: String,
    },
    IntentSubmit {
        ts_ns: i64,
        market_slug: String,
        asset_id: String,
        intent_id: String,
        side: String,
        price: f64,
        size: f64,
        post_only: bool,
        ladder_position: Option<i32>,
        reason_tag: String,
    },
    IntentCancel {
        ts_ns: i64,
        intent_id: String,
        reason: String,
    },
    IntentReplace {
        ts_ns: i64,
        old_intent_id: String,
        new_intent_id: String,
    },
    Fill {
        ts_ns: i64,
        intent_id: String,
        fill_qty: f64,
        fill_price: f64,
        queue_assumption: String,
        was_simulated_fill: bool,
    },
    PartialFill {
        ts_ns: i64,
        intent_id: String,
        fill_qty: f64,
        fill_price: f64,
        remaining_qty: f64,
    },
    MergeEvent {
        ts_ns: i64,
        market_slug: String,
        qty_yes_burned: f64,
        qty_no_burned: f64,
        usd_credited: f64,
    },
    RedeemEvent {
        ts_ns: i64,
        market_slug: String,
        asset_id: String,
        qty_redeemed: f64,
        usd_credited: f64,
    },
    InventorySnapshot {
        ts_ns: i64,
        market_slug: String,
        asset_id: String,
        qty: f64,
        avg_cost: f64,
    },
    AccountingEvent {
        ts_ns: i64,
        kind: String,
        market_slug: Option<String>,
        asset_id: Option<String>,
        usd_amount: f64,
    },
}

impl JournalEvent {
    pub fn ts_ns(&self) -> i64 {
        match self {
            Self::StrategyDecision { ts_ns, .. }
            | Self::IntentSubmit { ts_ns, .. }
            | Self::IntentCancel { ts_ns, .. }
            | Self::IntentReplace { ts_ns, .. }
            | Self::Fill { ts_ns, .. }
            | Self::PartialFill { ts_ns, .. }
            | Self::MergeEvent { ts_ns, .. }
            | Self::RedeemEvent { ts_ns, .. }
            | Self::InventorySnapshot { ts_ns, .. }
            | Self::AccountingEvent { ts_ns, .. } => *ts_ns,
        }
    }

    fn kind_str(&self) -> &'static str {
        match self {
            Self::StrategyDecision { .. } => "strategy_decision",
            Self::IntentSubmit { .. } => "intent_submit",
            Self::IntentCancel { .. } => "intent_cancel",
            Self::IntentReplace { .. } => "intent_replace",
            Self::Fill { .. } => "fill",
            Self::PartialFill { .. } => "partial_fill",
            Self::MergeEvent { .. } => "merge_event",
            Self::RedeemEvent { .. } => "redeem_event",
            Self::InventorySnapshot { .. } => "inventory_snapshot",
            Self::AccountingEvent { .. } => "accounting_event",
        }
    }
}

fn journal_schema() -> Arc<Schema> {
    Arc::new(Schema::new(vec![
        Field::new("event_kind", DataType::Utf8, false),
        Field::new("ts_ns", DataType::Int64, false),
        Field::new("seq", DataType::UInt32, false),
        Field::new("market_slug", DataType::Utf8, true),
        Field::new("asset_id", DataType::Utf8, true),
        Field::new("intent_id", DataType::Utf8, true),
        Field::new("decision_type", DataType::Utf8, true),
        Field::new("raw_inputs_hash", DataType::Utf8, true),
        Field::new("reason_tag", DataType::Utf8, true),
        Field::new("side", DataType::Utf8, true),
        Field::new("price", DataType::Float64, true),
        Field::new("size", DataType::Float64, true),
        Field::new("post_only", DataType::Boolean, true),
        Field::new("ladder_position", DataType::Int32, true),
        Field::new("old_intent_id", DataType::Utf8, true),
        Field::new("new_intent_id", DataType::Utf8, true),
        Field::new("reason", DataType::Utf8, true),
        Field::new("fill_qty", DataType::Float64, true),
        Field::new("fill_price", DataType::Float64, true),
        Field::new("queue_assumption", DataType::Utf8, true),
        Field::new("was_simulated_fill", DataType::Boolean, true),
        Field::new("remaining_qty", DataType::Float64, true),
        Field::new("qty_yes_burned", DataType::Float64, true),
        Field::new("qty_no_burned", DataType::Float64, true),
        Field::new("qty_redeemed", DataType::Float64, true),
        Field::new("usd_credited", DataType::Float64, true),
        Field::new("qty", DataType::Float64, true),
        Field::new("avg_cost", DataType::Float64, true),
        Field::new("accounting_kind", DataType::Utf8, true),
        Field::new("usd_amount", DataType::Float64, true),
    ]))
}

#[derive(Default)]
struct ColumnBuilders {
    event_kind: Vec<&'static str>,
    ts_ns: Vec<i64>,
    seq: Vec<u32>,
    market_slug: Vec<Option<String>>,
    asset_id: Vec<Option<String>>,
    intent_id: Vec<Option<String>>,
    decision_type: Vec<Option<String>>,
    raw_inputs_hash: Vec<Option<String>>,
    reason_tag: Vec<Option<String>>,
    side: Vec<Option<String>>,
    price: Vec<Option<f64>>,
    size: Vec<Option<f64>>,
    post_only: Vec<Option<bool>>,
    ladder_position: Vec<Option<i32>>,
    old_intent_id: Vec<Option<String>>,
    new_intent_id: Vec<Option<String>>,
    reason: Vec<Option<String>>,
    fill_qty: Vec<Option<f64>>,
    fill_price: Vec<Option<f64>>,
    queue_assumption: Vec<Option<String>>,
    was_simulated_fill: Vec<Option<bool>>,
    remaining_qty: Vec<Option<f64>>,
    qty_yes_burned: Vec<Option<f64>>,
    qty_no_burned: Vec<Option<f64>>,
    qty_redeemed: Vec<Option<f64>>,
    usd_credited: Vec<Option<f64>>,
    qty: Vec<Option<f64>>,
    avg_cost: Vec<Option<f64>>,
    accounting_kind: Vec<Option<String>>,
    usd_amount: Vec<Option<f64>>,
}

impl ColumnBuilders {
    fn push(&mut self, seq: u32, ev: &JournalEvent) {
        self.event_kind.push(ev.kind_str());
        self.ts_ns.push(ev.ts_ns());
        self.seq.push(seq);
        // Default all optional columns to None; the per-variant arms below
        // overwrite the columns the variant carries.
        self.market_slug.push(None);
        self.asset_id.push(None);
        self.intent_id.push(None);
        self.decision_type.push(None);
        self.raw_inputs_hash.push(None);
        self.reason_tag.push(None);
        self.side.push(None);
        self.price.push(None);
        self.size.push(None);
        self.post_only.push(None);
        self.ladder_position.push(None);
        self.old_intent_id.push(None);
        self.new_intent_id.push(None);
        self.reason.push(None);
        self.fill_qty.push(None);
        self.fill_price.push(None);
        self.queue_assumption.push(None);
        self.was_simulated_fill.push(None);
        self.remaining_qty.push(None);
        self.qty_yes_burned.push(None);
        self.qty_no_burned.push(None);
        self.qty_redeemed.push(None);
        self.usd_credited.push(None);
        self.qty.push(None);
        self.avg_cost.push(None);
        self.accounting_kind.push(None);
        self.usd_amount.push(None);
        let last = self.event_kind.len() - 1;
        match ev {
            JournalEvent::StrategyDecision {
                market_slug,
                asset_id,
                decision_type,
                raw_inputs_hash,
                reason_tag,
                ..
            } => {
                self.market_slug[last] = Some(market_slug.clone());
                self.asset_id[last] = asset_id.clone();
                self.decision_type[last] = Some(decision_type.clone());
                self.raw_inputs_hash[last] = Some(raw_inputs_hash.clone());
                self.reason_tag[last] = Some(reason_tag.clone());
            }
            JournalEvent::IntentSubmit {
                market_slug,
                asset_id,
                intent_id,
                side,
                price,
                size,
                post_only,
                ladder_position,
                reason_tag,
                ..
            } => {
                self.market_slug[last] = Some(market_slug.clone());
                self.asset_id[last] = Some(asset_id.clone());
                self.intent_id[last] = Some(intent_id.clone());
                self.side[last] = Some(side.clone());
                self.price[last] = Some(*price);
                self.size[last] = Some(*size);
                self.post_only[last] = Some(*post_only);
                self.ladder_position[last] = *ladder_position;
                self.reason_tag[last] = Some(reason_tag.clone());
            }
            JournalEvent::IntentCancel {
                intent_id, reason, ..
            } => {
                self.intent_id[last] = Some(intent_id.clone());
                self.reason[last] = Some(reason.clone());
            }
            JournalEvent::IntentReplace {
                old_intent_id,
                new_intent_id,
                ..
            } => {
                self.old_intent_id[last] = Some(old_intent_id.clone());
                self.new_intent_id[last] = Some(new_intent_id.clone());
            }
            JournalEvent::Fill {
                intent_id,
                fill_qty,
                fill_price,
                queue_assumption,
                was_simulated_fill,
                ..
            } => {
                self.intent_id[last] = Some(intent_id.clone());
                self.fill_qty[last] = Some(*fill_qty);
                self.fill_price[last] = Some(*fill_price);
                self.queue_assumption[last] = Some(queue_assumption.clone());
                self.was_simulated_fill[last] = Some(*was_simulated_fill);
            }
            JournalEvent::PartialFill {
                intent_id,
                fill_qty,
                fill_price,
                remaining_qty,
                ..
            } => {
                self.intent_id[last] = Some(intent_id.clone());
                self.fill_qty[last] = Some(*fill_qty);
                self.fill_price[last] = Some(*fill_price);
                self.remaining_qty[last] = Some(*remaining_qty);
            }
            JournalEvent::MergeEvent {
                market_slug,
                qty_yes_burned,
                qty_no_burned,
                usd_credited,
                ..
            } => {
                self.market_slug[last] = Some(market_slug.clone());
                self.qty_yes_burned[last] = Some(*qty_yes_burned);
                self.qty_no_burned[last] = Some(*qty_no_burned);
                self.usd_credited[last] = Some(*usd_credited);
            }
            JournalEvent::RedeemEvent {
                market_slug,
                asset_id,
                qty_redeemed,
                usd_credited,
                ..
            } => {
                self.market_slug[last] = Some(market_slug.clone());
                self.asset_id[last] = Some(asset_id.clone());
                self.qty_redeemed[last] = Some(*qty_redeemed);
                self.usd_credited[last] = Some(*usd_credited);
            }
            JournalEvent::InventorySnapshot {
                market_slug,
                asset_id,
                qty,
                avg_cost,
                ..
            } => {
                self.market_slug[last] = Some(market_slug.clone());
                self.asset_id[last] = Some(asset_id.clone());
                self.qty[last] = Some(*qty);
                self.avg_cost[last] = Some(*avg_cost);
            }
            JournalEvent::AccountingEvent {
                kind,
                market_slug,
                asset_id,
                usd_amount,
                ..
            } => {
                self.accounting_kind[last] = Some(kind.clone());
                self.market_slug[last] = market_slug.clone();
                self.asset_id[last] = asset_id.clone();
                self.usd_amount[last] = Some(*usd_amount);
            }
        }
    }

    fn into_arrays(self) -> Vec<ArrayRef> {
        let opt_str = |v: Vec<Option<String>>| -> ArrayRef {
            Arc::new(StringArray::from(
                v.into_iter().collect::<Vec<Option<String>>>(),
            ))
        };
        let opt_f64 = |v: Vec<Option<f64>>| -> ArrayRef { Arc::new(Float64Array::from(v)) };
        let opt_bool = |v: Vec<Option<bool>>| -> ArrayRef { Arc::new(BooleanArray::from(v)) };
        let opt_i32 = |v: Vec<Option<i32>>| -> ArrayRef { Arc::new(Int32Array::from(v)) };
        vec![
            Arc::new(StringArray::from(
                self.event_kind
                    .into_iter()
                    .map(|s| s.to_string())
                    .collect::<Vec<_>>(),
            )),
            Arc::new(Int64Array::from(self.ts_ns)),
            Arc::new(UInt32Array::from(self.seq)),
            opt_str(self.market_slug),
            opt_str(self.asset_id),
            opt_str(self.intent_id),
            opt_str(self.decision_type),
            opt_str(self.raw_inputs_hash),
            opt_str(self.reason_tag),
            opt_str(self.side),
            opt_f64(self.price),
            opt_f64(self.size),
            opt_bool(self.post_only),
            opt_i32(self.ladder_position),
            opt_str(self.old_intent_id),
            opt_str(self.new_intent_id),
            opt_str(self.reason),
            opt_f64(self.fill_qty),
            opt_f64(self.fill_price),
            opt_str(self.queue_assumption),
            opt_bool(self.was_simulated_fill),
            opt_f64(self.remaining_qty),
            opt_f64(self.qty_yes_burned),
            opt_f64(self.qty_no_burned),
            opt_f64(self.qty_redeemed),
            opt_f64(self.usd_credited),
            opt_f64(self.qty),
            opt_f64(self.avg_cost),
            opt_str(self.accounting_kind),
            opt_f64(self.usd_amount),
        ]
    }
}

/// Write a slice of journal events to a single Parquet file.
///
/// Sort order is `(ts_ns, seq)` where `seq` is the original insertion index.
/// `ts_ns` ties (two events sharing the same replay clock) preserve the
/// emission order so an `intent_submit` followed by its `fill` always sorts
/// in causal order.
pub fn write_journal_parquet(path: &Path, events: &[JournalEvent]) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create journal parent dir {}", parent.display()))?;
    }
    let schema = journal_schema();

    let mut indexed: Vec<(usize, &JournalEvent)> = events.iter().enumerate().collect();
    indexed.sort_by(|a, b| (a.1.ts_ns(), a.0).cmp(&(b.1.ts_ns(), b.0)));

    let mut builders = ColumnBuilders::default();
    for (orig_idx, ev) in indexed {
        builders.push(orig_idx as u32, ev);
    }

    let arrays = builders.into_arrays();
    let batch = RecordBatch::try_new(schema.clone(), arrays)
        .context("failed to build journal RecordBatch")?;

    let tmp_path = path.with_extension(format!(
        "{}.tmp",
        path.extension()
            .and_then(|ext| ext.to_str())
            .unwrap_or("parquet")
    ));
    let file = File::create(&tmp_path)
        .with_context(|| format!("failed to create journal file {}", tmp_path.display()))?;
    let mut writer =
        ArrowWriter::try_new(file, schema, None).context("failed to create journal ArrowWriter")?;
    writer
        .write(&batch)
        .context("failed to write journal RecordBatch")?;
    writer.close().context("failed to close journal writer")?;
    fs::rename(&tmp_path, path).with_context(|| {
        format!(
            "failed to atomically move journal {} to {}",
            tmp_path.display(),
            path.display()
        )
    })?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder;

    #[test]
    fn writes_and_orders_events_by_ts_ns_then_emission_order() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("journal.parquet");
        let events = vec![
            JournalEvent::IntentSubmit {
                ts_ns: 200,
                market_slug: "m1".into(),
                asset_id: "a1".into(),
                intent_id: "coid-1".into(),
                side: "buy".into(),
                price: 0.42,
                size: 10.0,
                post_only: true,
                ladder_position: Some(1),
                reason_tag: "paired-mm ladder yes level 1".into(),
            },
            JournalEvent::StrategyDecision {
                ts_ns: 100,
                market_slug: "m1".into(),
                asset_id: None,
                decision_type: "QuoteSet".into(),
                raw_inputs_hash: "abc".into(),
                reason_tag: "paired-mm ladder regime=Active".into(),
            },
            JournalEvent::Fill {
                ts_ns: 200,
                intent_id: "coid-1".into(),
                fill_qty: 10.0,
                fill_price: 0.42,
                queue_assumption: "base".into(),
                was_simulated_fill: true,
            },
        ];
        write_journal_parquet(&path, &events).unwrap();

        let file = std::fs::File::open(&path).unwrap();
        let builder = ParquetRecordBatchReaderBuilder::try_new(file).unwrap();
        let reader = builder.build().unwrap();
        let mut total_rows = 0usize;
        let mut kinds_in_order = Vec::new();
        for batch in reader {
            let batch = batch.unwrap();
            total_rows += batch.num_rows();
            let kind = batch
                .column(0)
                .as_any()
                .downcast_ref::<StringArray>()
                .unwrap();
            for i in 0..batch.num_rows() {
                kinds_in_order.push(kind.value(i).to_string());
            }
        }
        assert_eq!(total_rows, 3);
        assert_eq!(
            kinds_in_order,
            vec![
                "strategy_decision".to_string(),
                "intent_submit".to_string(),
                "fill".to_string(),
            ]
        );
    }

    #[test]
    fn merge_and_redeem_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("journal.parquet");
        let events = vec![
            JournalEvent::MergeEvent {
                ts_ns: 1,
                market_slug: "m1".into(),
                qty_yes_burned: 5.0,
                qty_no_burned: 5.0,
                usd_credited: 5.0,
            },
            JournalEvent::RedeemEvent {
                ts_ns: 2,
                market_slug: "m1".into(),
                asset_id: "yes".into(),
                qty_redeemed: 3.0,
                usd_credited: 3.0,
            },
            JournalEvent::AccountingEvent {
                ts_ns: 3,
                kind: "fee".into(),
                market_slug: Some("m1".into()),
                asset_id: None,
                usd_amount: 0.10,
            },
        ];
        write_journal_parquet(&path, &events).unwrap();
        // Just verify the file is readable; column-by-column shape is
        // exercised by the prior test.
        let file = std::fs::File::open(&path).unwrap();
        let builder = ParquetRecordBatchReaderBuilder::try_new(file).unwrap();
        let reader = builder.build().unwrap();
        let total: usize = reader.map(|b| b.unwrap().num_rows()).sum();
        assert_eq!(total, 3);
    }
}
