//! Strategy-local prefilter for paired-MM running caps.
//!
//! This is not the account-level `RiskEngine`; it is a deterministic guard
//! that prevents the paired-MM algorithm from proposing obviously unsafe entry
//! ladders. Runtime must still pass every intent through the hard risk engine
//! before submission.

use crate::market_making::paired_mm::types::{PairedInventorySnapshot, RunningInventoryCaps};
use crate::types::{IntentKind, OrderIntent};

#[derive(Clone, Debug, PartialEq)]
pub enum PairedMmRiskReject {
    GrossCostCap { gross_cost_usd: f64, cap_usd: f64 },
    EntryNotionalCap { notional_usd: f64, cap_usd: f64 },
}

#[derive(Clone, Debug, PartialEq)]
pub struct PairedMmRiskDecision {
    pub accepted: bool,
    pub reject: Option<PairedMmRiskReject>,
    pub note: String,
}

impl PairedMmRiskDecision {
    pub fn accept() -> Self {
        Self {
            accepted: true,
            reject: None,
            note: "paired-mm risk prefilter accepted".to_string(),
        }
    }

    pub fn reject(reject: PairedMmRiskReject, note: impl Into<String>) -> Self {
        Self {
            accepted: false,
            reject: Some(reject),
            note: note.into(),
        }
    }
}

pub fn evaluate_entry_intent(
    inventory: &PairedInventorySnapshot,
    intent: &OrderIntent,
    caps: &RunningInventoryCaps,
) -> PairedMmRiskDecision {
    if intent.kind != IntentKind::Entry {
        return PairedMmRiskDecision::accept();
    }

    let gross_cost_usd = inventory.gross_cost_usd();
    if gross_cost_usd >= caps.max_gross_cost_usd {
        return PairedMmRiskDecision::reject(
            PairedMmRiskReject::GrossCostCap {
                gross_cost_usd,
                cap_usd: caps.max_gross_cost_usd,
            },
            format!(
                "paired-mm gross cost cap reached gross={gross_cost_usd:.4} cap={:.4}",
                caps.max_gross_cost_usd
            ),
        );
    }

    let notional_usd = intent.notional_usd();
    if notional_usd > caps.max_entry_notional_usd {
        return PairedMmRiskDecision::reject(
            PairedMmRiskReject::EntryNotionalCap {
                notional_usd,
                cap_usd: caps.max_entry_notional_usd,
            },
            format!(
                "paired-mm entry notional cap exceeded notional={notional_usd:.4} cap={:.4}",
                caps.max_entry_notional_usd
            ),
        );
    }

    PairedMmRiskDecision::accept()
}

pub fn filter_entry_intents(
    inventory: &PairedInventorySnapshot,
    intents: Vec<OrderIntent>,
    caps: &RunningInventoryCaps,
) -> (Vec<OrderIntent>, Vec<PairedMmRiskDecision>) {
    let mut accepted = Vec::with_capacity(intents.len());
    let mut decisions = Vec::new();
    for intent in intents {
        let decision = evaluate_entry_intent(inventory, &intent, caps);
        if decision.accepted {
            accepted.push(intent);
        } else {
            decisions.push(decision);
        }
    }
    (accepted, decisions)
}
