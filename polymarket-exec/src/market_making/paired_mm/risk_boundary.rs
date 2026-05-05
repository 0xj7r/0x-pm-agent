//! Strategy-local prefilter for paired-MM running caps.
//!
//! This is not the account-level `RiskEngine`; it is a deterministic guard
//! that prevents the paired-MM algorithm from proposing obviously unsafe entry
//! ladders. Runtime must still pass every intent through the hard risk engine
//! before submission.

use std::collections::HashSet;

use crate::market_making::pairing::types::{PairedInventorySnapshot, RunningInventoryCaps};
use crate::types::{IntentKind, OrderIntent};

const USD_EPSILON: f64 = 1e-6;

#[derive(Clone, Debug, PartialEq)]
pub enum PairedMmRiskReject {
    GrossCostCap { gross_cost_usd: f64, cap_usd: f64 },
    EntryNotionalCap { notional_usd: f64, cap_usd: f64 },
    EntryQuantityCap { quantity: f64, cap_qty: f64 },
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
    if gross_cost_usd + USD_EPSILON >= caps.max_gross_cost_usd {
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
    if notional_usd > caps.max_entry_notional_usd + USD_EPSILON {
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

    if intent.quantity > caps.max_side_imbalance_qty + USD_EPSILON {
        return PairedMmRiskDecision::reject(
            PairedMmRiskReject::EntryQuantityCap {
                quantity: intent.quantity,
                cap_qty: caps.max_side_imbalance_qty,
            },
            format!(
                "paired-mm entry quantity cap exceeded quantity={:.4} cap={:.4}",
                intent.quantity, caps.max_side_imbalance_qty
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
    let mut rejected_pair_ids: HashSet<String> = HashSet::new();
    let mut accepted = Vec::with_capacity(intents.len());
    let mut decisions = Vec::new();
    let mut evaluated = Vec::with_capacity(intents.len());

    for intent in intents {
        let decision = evaluate_entry_intent(inventory, &intent, caps);
        if !decision.accepted {
            if let Some(pair_id) = intent.pair_id.as_ref() {
                rejected_pair_ids.insert(pair_id.clone());
            }
            decisions.push(decision.clone());
        }
        evaluated.push((intent, decision));
    }

    for (intent, decision) in evaluated {
        if !decision.accepted {
            continue;
        }
        if intent
            .pair_id
            .as_ref()
            .is_some_and(|pair_id| rejected_pair_ids.contains(pair_id))
        {
            decisions.push(PairedMmRiskDecision::reject(
                PairedMmRiskReject::EntryNotionalCap {
                    notional_usd: intent.notional_usd(),
                    cap_usd: caps.max_entry_notional_usd,
                },
                format!(
                    "paired-mm paired entry mate rejected; suppressing pair_id={}",
                    intent.pair_id.as_deref().unwrap_or_default()
                ),
            ));
            continue;
        }
        accepted.push(intent);
    }
    (accepted, decisions)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::market_making::pairing::types::RunningInventoryCaps;
    use crate::types::{ClientOrderId, InstrumentId, MarketId, TradeSide};

    fn entry_intent(price: f64, quantity: f64) -> OrderIntent {
        OrderIntent {
            client_order_id: ClientOrderId::from("test"),
            market_id: MarketId::from("m"),
            instrument_id: InstrumentId::from("yes"),
            side: TradeSide::Buy,
            limit_price: price,
            quantity,
            reduce_only: false,
            reason: "test".to_string(),
            quote_level_tag: Some("test".to_string()),
            created_at_ms: 0,
            pair_id: None,
            kind: IntentKind::Entry,
        }
    }

    #[test]
    fn accepts_entry_notional_at_cap_with_float_noise() {
        let caps = RunningInventoryCaps {
            max_entry_notional_usd: 5.0,
            ..RunningInventoryCaps::default()
        };
        let intent = entry_intent(0.1, 50.000000001);

        let decision = evaluate_entry_intent(&PairedInventorySnapshot::default(), &intent, &caps);

        assert!(decision.accepted, "{decision:?}");
    }

    #[test]
    fn rejects_entry_that_can_exceed_side_imbalance_cap_by_itself() {
        let caps = RunningInventoryCaps {
            max_entry_notional_usd: 5.0,
            max_side_imbalance_qty: 20.0,
            ..RunningInventoryCaps::default()
        };
        let intent = entry_intent(0.10, 50.0);

        let decision = evaluate_entry_intent(&PairedInventorySnapshot::default(), &intent, &caps);

        assert!(!decision.accepted, "{decision:?}");
        assert!(matches!(
            decision.reject,
            Some(PairedMmRiskReject::EntryQuantityCap { .. })
        ));
    }
}
