use anyhow::Result;

use crate::event_log::{EventLog, EventRecord};
use crate::journal::JournalFanout;
use crate::metrics::AppMetrics;
use crate::runtime::RuntimeOutcome;
use crate::types::RuntimeCommand;

pub(super) fn persist_runtime_outcome(
    journal: &mut JournalFanout,
    metrics: &AppMetrics,
    event_log: &EventLog,
    paper_report: Option<&mut crate::paper::report::PaperReportWriter>,
    source: &str,
    outcome: RuntimeOutcome,
) -> Result<()> {
    if !outcome.event_seqs.is_empty() {
        tracing::info!(
            source,
            event_count = outcome.event_seqs.len(),
            latest_seq = outcome.event_seqs.last().copied().unwrap_or_default(),
            "runtime accepted hot-path update"
        );
    }

    let records = if let Some(first_seq) = outcome.event_seqs.first().copied() {
        event_log.snapshot_since(first_seq.saturating_sub(1))
    } else {
        Vec::new()
    };
    record_strategy_attribution(metrics, &records, &outcome.commands);
    if let Some(report) = paper_report {
        report.record_runtime_outcome(&records, &outcome.commands, super::runner::now_unix_ms());
    }

    for record in &records {
        journal.append_event(record)?;
    }
    for command in &outcome.commands {
        journal.append_command(command)?;
    }
    journal.flush()?;

    Ok(())
}

fn record_strategy_attribution(
    metrics: &AppMetrics,
    records: &[EventRecord],
    commands: &[RuntimeCommand],
) {
    for record in records {
        if let Some(event) = classify_runtime_event(record.message.as_str()) {
            metrics.observe_strategy_event(event);
        }
    }
    for command in commands {
        if let Some(intent) = classify_runtime_command(command) {
            metrics.observe_strategy_intent(intent);
        }
    }
}

fn classify_runtime_event(message: &str) -> Option<&'static str> {
    if message.contains("entry-fill asymmetry cooldown")
        || message.contains("asymmetric entry-fill cooldown")
    {
        return Some("asym_fill_cooldown");
    }
    if message.contains("market mid moved") {
        return Some("market_mid_trend_gate");
    }
    if message.contains("btc regime flat") {
        return Some("btc_flat_gate");
    }
    if message.contains("btc regime trending") {
        return Some("btc_trend_gate");
    }
    if message.contains("premium fair cap") {
        return Some("premium_fair_gate");
    }
    if message.contains("hold stranded positive-asymmetry") {
        return Some("hold_positive_asym");
    }
    if message.contains("rescue stranded leg") {
        return Some("rescue_ev_selected");
    }
    if message.contains("mode=repair_first") {
        return Some("paired_mm_repair_first");
    }
    if message.contains("repair-first: no viable light-side quote") {
        return Some("paired_mm_repair_suppressed");
    }
    if message.contains("on-fill IOC rescue emitted") {
        return Some("on_fill_rescue");
    }
    if message.contains("on-fill rescue throttled") {
        return Some("rescue_throttled");
    }
    if message.contains("paired entry ladder rejected") {
        return Some("entry_ladder_rejected");
    }
    if message.contains("inventory management rejected") {
        return Some("inventory_management_rejected");
    }
    if message.contains("market context replaced") {
        return Some("market_rollover");
    }
    if message.contains("runtime degraded") {
        return Some("runtime_degraded");
    }
    if message.contains("runtime risk-off") {
        return Some("runtime_riskoff");
    }
    None
}

fn classify_runtime_command(command: &RuntimeCommand) -> Option<&'static str> {
    let RuntimeCommand::Submit(intent) = command else {
        return None;
    };
    match intent.quote_level_tag.as_deref().unwrap_or_default() {
        tag if tag.starts_with("mm-paired-bid") => Some("paired_ladder"),
        tag if tag.starts_with("mm-convex-accum") => Some("convex_accum"),
        tag if tag.starts_with("mm-capital-recycle") => Some("capital_recycle"),
        tag if tag.starts_with("mm-hedge-rescue") => Some("hedge_rescue"),
        tag if tag.starts_with("mm-reduce") => Some("reduce_cleanup"),
        "" => Some("untagged_submit"),
        _ => Some("other_submit"),
    }
}
