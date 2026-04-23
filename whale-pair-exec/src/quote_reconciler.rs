use std::collections::{HashMap, VecDeque};

use crate::runtime::types::{ManagedOrder, ManagedOrderStatus};
use crate::types::{ClientOrderId, EpochMillis, OrderIntent};

#[derive(Clone, Copy, Debug)]
pub struct ReconcilerConfig {
    pub min_order_age_ms: u64,
    pub max_churn_per_window: usize,
    pub churn_window_ms: u64,
    pub hard_pull_ms: u64,
    pub max_submit_per_window: usize,
    pub max_replace_per_window: usize,
    pub max_cancel_per_window: usize,
}

impl Default for ReconcilerConfig {
    fn default() -> Self {
        Self {
            min_order_age_ms: 750,
            max_churn_per_window: 12,
            churn_window_ms: 20_000,
            hard_pull_ms: 5_000,
            max_submit_per_window: 6,
            max_replace_per_window: 4,
            max_cancel_per_window: 12,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum QuoteAction {
    Keep(OrderIntent),
    Cancel {
        client_order_id: ClientOrderId,
        reason: String,
    },
    Replace {
        existing_client_order_id: ClientOrderId,
        replacement: OrderIntent,
        cancel_reason: String,
    },
    Submit(OrderIntent),
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct QuotePlan {
    pub actions: Vec<QuoteAction>,
    pub notes: Vec<String>,
}

#[derive(Clone, Copy, Debug)]
enum ChurnGate {
    Allowed,
    HardPull,
}

#[derive(Clone, Debug)]
struct QuoteMatchKey {
    market_id: crate::types::MarketId,
    instrument_id: crate::types::InstrumentId,
    side: crate::types::TradeSide,
    reduce_only: bool,
    level_tag: String,
}

impl PartialEq for QuoteMatchKey {
    fn eq(&self, other: &Self) -> bool {
        self.market_id == other.market_id
            && self.instrument_id == other.instrument_id
            && self.side == other.side
            && self.reduce_only == other.reduce_only
            && self.level_tag == other.level_tag
    }
}
impl Eq for QuoteMatchKey {}
impl std::hash::Hash for QuoteMatchKey {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.market_id.hash(state);
        self.instrument_id.hash(state);
        self.side.hash(state);
        self.reduce_only.hash(state);
        self.level_tag.hash(state);
    }
}

impl QuoteMatchKey {
    fn from_intent(intent: &OrderIntent) -> Self {
        let level_tag = intent
            .quote_level_tag
            .clone()
            .unwrap_or_else(|| "default".to_string());
        Self {
            market_id: intent.market_id.clone(),
            instrument_id: intent.instrument_id.clone(),
            side: intent.side,
            reduce_only: intent.reduce_only,
            level_tag,
        }
    }
}

#[derive(Clone, Debug)]
pub struct QuoteReconciler {
    config: ReconcilerConfig,
    churn_events: VecDeque<EpochMillis>,
    submit_events: VecDeque<EpochMillis>,
    replace_events: VecDeque<EpochMillis>,
    cancel_events: VecDeque<EpochMillis>,
    hard_pull_until_ms: Option<EpochMillis>,
}

impl Default for QuoteReconciler {
    fn default() -> Self {
        Self {
            config: ReconcilerConfig::default(),
            churn_events: VecDeque::new(),
            submit_events: VecDeque::new(),
            replace_events: VecDeque::new(),
            cancel_events: VecDeque::new(),
            hard_pull_until_ms: None,
        }
    }
}
impl QuoteReconciler {
    pub fn new(config: ReconcilerConfig) -> Self {
        Self {
            config,
            churn_events: VecDeque::new(),
            submit_events: VecDeque::new(),
            replace_events: VecDeque::new(),
            cancel_events: VecDeque::new(),
            hard_pull_until_ms: None,
        }
    }

    fn order_age_ms(order: &ManagedOrder, now_ms: EpochMillis) -> u64 {
        now_ms.saturating_sub(order.last_update_ms)
    }

    fn can_change(&self, order: &ManagedOrder, now_ms: EpochMillis) -> bool {
        if matches!(order.status, ManagedOrderStatus::Filled | ManagedOrderStatus::Cancelled) {
            return false;
        }
        Self::order_age_ms(order, now_ms) >= self.config.min_order_age_ms
    }

    fn prune_window(events: &mut VecDeque<EpochMillis>, now_ms: EpochMillis, window_ms: u64) {
        let window_ms = window_ms.max(1);
        while let Some(front) = events.front() {
            if now_ms.saturating_sub(*front) <= window_ms {
                break;
            }
            let _ = events.pop_front();
        }
    }

    fn within_rate_cap(
        events: &VecDeque<EpochMillis>,
        now_ms: EpochMillis,
        window_ms: u64,
        cap: usize,
        upcoming: usize,
    ) -> bool {
        if cap == 0 {
            return upcoming == 0;
        }
        let window_ms = window_ms.max(1);
        let count = events
            .iter()
            .filter(|event_ms| now_ms.saturating_sub(**event_ms) <= window_ms)
            .count();
        count + upcoming <= cap
    }

    fn same_quote(current: &OrderIntent, desired: &OrderIntent) -> bool {
        (current.quantity - desired.quantity).abs() <= 1e-9
            && (current.limit_price - desired.limit_price).abs() <= 1e-9
            && current.side == desired.side
            && current.reduce_only == desired.reduce_only
            && current.quote_level_tag == desired.quote_level_tag
    }

    fn record_churn(&mut self, now_ms: EpochMillis, churn: usize) {
        let window_ms = self.config.churn_window_ms.max(1);
        Self::prune_window(&mut self.churn_events, now_ms, window_ms);
        (0..churn).for_each(|_| self.churn_events.push_back(now_ms));
    }

    fn record_submit(&mut self, now_ms: EpochMillis) {
        Self::prune_window(&mut self.submit_events, now_ms, self.config.churn_window_ms);
        self.submit_events.push_back(now_ms);
    }

    fn record_replace(&mut self, now_ms: EpochMillis) {
        Self::prune_window(&mut self.replace_events, now_ms, self.config.churn_window_ms);
        self.replace_events.push_back(now_ms);
    }

    fn record_cancel(&mut self, now_ms: EpochMillis) {
        Self::prune_window(&mut self.cancel_events, now_ms, self.config.churn_window_ms);
        self.cancel_events.push_back(now_ms);
    }

    fn can_submit(&self, now_ms: EpochMillis, upcoming: usize) -> bool {
        Self::within_rate_cap(
            &self.submit_events,
            now_ms,
            self.config.churn_window_ms,
            self.config.max_submit_per_window,
            upcoming,
        )
    }

    fn can_replace(&self, now_ms: EpochMillis, upcoming: usize) -> bool {
        Self::within_rate_cap(
            &self.replace_events,
            now_ms,
            self.config.churn_window_ms,
            self.config.max_replace_per_window,
            upcoming,
        )
    }

    fn can_cancel(&self, now_ms: EpochMillis, upcoming: usize) -> bool {
        Self::within_rate_cap(
            &self.cancel_events,
            now_ms,
            self.config.churn_window_ms,
            self.config.max_cancel_per_window,
            upcoming,
        )
    }

    fn evaluate_churn(&self, now_ms: EpochMillis, upcoming_churn: usize) -> ChurnGate {
        let mut events = self.churn_events.clone();
        let window_ms = self.config.churn_window_ms.max(1);
        while let Some(front) = events.front() {
            if now_ms.saturating_sub(*front) <= window_ms {
                break;
            }
            let _ = events.pop_front();
        }
        if events.len() + upcoming_churn > self.config.max_churn_per_window {
            ChurnGate::HardPull
        } else {
            ChurnGate::Allowed
        }
    }

    fn should_hard_pull(&self, now_ms: EpochMillis) -> bool {
        self.hard_pull_until_ms
            .is_some_and(|until| now_ms <= until && until != 0)
    }

    fn build_cancel_all(
        &self,
        open_orders: &HashMap<ClientOrderId, ManagedOrder>,
        now_ms: EpochMillis,
    ) -> Vec<QuoteAction> {
        let mut actions = Vec::new();
        let mut keys = open_orders.keys().collect::<Vec<_>>();
        keys.sort_unstable();
        for client_order_id in keys {
            if let Some(order) = open_orders.get(client_order_id) {
                if self.can_change(order, now_ms) {
                    actions.push(QuoteAction::Cancel {
                        client_order_id: client_order_id.clone(),
                        reason: "hard pull refresh".to_string(),
                    });
                }
            }
        }
        actions
    }

    pub fn plan(
        &mut self,
        desired: crate::quote_engine::DesiredQuoteSet,
        open_orders: &HashMap<ClientOrderId, ManagedOrder>,
        now_ms: EpochMillis,
    ) -> QuotePlan {
        let mut plan = QuotePlan::default();
        if self.should_hard_pull(now_ms) {
            plan.actions.extend(self.build_cancel_all(open_orders, now_ms));
            plan.notes.push("hard pull active".to_string());
            return plan;
        }

        let mut by_key: HashMap<QuoteMatchKey, Vec<(ClientOrderId, &ManagedOrder)>> =
            HashMap::new();
        for (id, managed) in open_orders.iter() {
            if managed.remaining_qty() <= 1e-9 {
                continue;
            }
            by_key
                .entry(QuoteMatchKey::from_intent(&managed.intent))
                .or_default()
                .push((id.clone(), managed));
        }

        for value in by_key.values_mut() {
            value.sort_by(|left, right| left.0.as_str().cmp(right.0.as_str()));
        }

        let mut pending_replacements = 0usize;
        let mut planned_churn = 0usize;
        let mut planned_submits = 0usize;
        let mut planned_replaces = 0usize;
        let mut planned_cancels = 0usize;

        for desired_quote in desired.quotes {
            let desired_intent = desired_quote.intent;
            let key = QuoteMatchKey::from_intent(&desired_intent);
            let mut matches = by_key.remove(&key).unwrap_or_default();
            if matches.is_empty() {
                if self.can_submit(now_ms, planned_submits + 1) {
                    plan.actions.push(QuoteAction::Submit(desired_intent));
                    planned_submits += 1;
                    planned_churn += 1;
                } else {
                    plan.notes.push("submit rate cap reached".to_string());
                }
                continue;
            }

            let (existing_id, existing_order) = matches.remove(0);
            if Self::same_quote(&existing_order.intent, &desired_intent) {
                plan.actions.push(QuoteAction::Keep(existing_order.intent.clone()));
            } else if self.can_change(existing_order, now_ms) {
                if self.can_replace(now_ms, planned_replaces + 1) {
                    plan.actions.push(QuoteAction::Replace {
                        existing_client_order_id: existing_id,
                        replacement: desired_intent,
                        cancel_reason: "desired changed".to_string(),
                    });
                    pending_replacements += 1;
                    planned_replaces += 1;
                    planned_churn += 2;
                } else {
                    plan.actions.push(QuoteAction::Keep(existing_order.intent.clone()));
                    plan.notes.push("replace rate cap reached".to_string());
                }
            } else {
                plan.actions.push(QuoteAction::Keep(existing_order.intent.clone()));
            }

            for (extra_id, extra_order) in matches {
                if self.can_change(extra_order, now_ms) {
                    if self.can_cancel(now_ms, planned_cancels + 1) {
                        plan.actions.push(QuoteAction::Cancel {
                            client_order_id: extra_id,
                            reason: "replace cleanup".to_string(),
                        });
                        planned_cancels += 1;
                        planned_churn += 1;
                    } else {
                        plan.actions.push(QuoteAction::Keep(extra_order.intent.clone()));
                        plan.notes.push("cancel rate cap reached".to_string());
                    }
                } else {
                    plan.actions.push(QuoteAction::Keep(extra_order.intent.clone()));
                }
            }
        }

        let mut unmatched_ids = by_key
            .into_values()
            .flat_map(|values| values.into_iter())
            .collect::<Vec<_>>();
        unmatched_ids.sort_by(|left, right| left.0.as_str().cmp(right.0.as_str()));

        for (client_order_id, order) in unmatched_ids {
            if self.can_change(order, now_ms) {
                if self.can_cancel(now_ms, planned_cancels + 1) {
                    plan.actions.push(QuoteAction::Cancel {
                        client_order_id,
                        reason: "no longer desired".to_string(),
                    });
                    planned_cancels += 1;
                    planned_churn += 1;
                } else {
                    plan.actions.push(QuoteAction::Keep(order.intent.clone()));
                    plan.notes.push("cancel rate cap reached".to_string());
                }
            } else {
                plan.actions.push(QuoteAction::Keep(order.intent.clone()));
            }
        }

        if let ChurnGate::HardPull = self.evaluate_churn(now_ms, planned_churn) {
            self.hard_pull_until_ms = Some(now_ms.saturating_add(self.config.hard_pull_ms));
            plan.actions = self.build_cancel_all(open_orders, now_ms);
            plan.notes.push("hard pull triggered".to_string());
            return plan;
        }

        self.record_churn(now_ms, planned_churn);
        (0..planned_submits).for_each(|_| self.record_submit(now_ms));
        (0..planned_replaces).for_each(|_| self.record_replace(now_ms));
        (0..planned_cancels).for_each(|_| self.record_cancel(now_ms));
        if pending_replacements > 0 {
            plan.notes.push(format!(
                "reconciler prepared {} replace(s)",
                pending_replacements
            ));
        }

        plan.actions.sort_by(|left, right| match (left, right) {
            (
                QuoteAction::Cancel { client_order_id: left_id, .. },
                QuoteAction::Cancel { client_order_id: right_id, .. },
            ) => left_id.as_str().cmp(right_id.as_str()),
            (
                QuoteAction::Replace {
                    existing_client_order_id: left_id,
                    ..
                },
                QuoteAction::Replace {
                    existing_client_order_id: right_id,
                    ..
                },
            ) => left_id.as_str().cmp(right_id.as_str()),
            (
                QuoteAction::Submit(_) | QuoteAction::Keep(_),
                QuoteAction::Submit(_) | QuoteAction::Keep(_),
            ) => std::cmp::Ordering::Equal,
            (QuoteAction::Cancel { .. }, _) => std::cmp::Ordering::Less,
            (_, QuoteAction::Cancel { .. }) => std::cmp::Ordering::Greater,
            (QuoteAction::Replace { .. }, QuoteAction::Submit(_) | QuoteAction::Keep(_)) => {
                std::cmp::Ordering::Less
            }
            (QuoteAction::Submit(_) | QuoteAction::Keep(_), QuoteAction::Replace { .. }) => {
                std::cmp::Ordering::Greater
            }
            (QuoteAction::Keep(_), QuoteAction::Submit(_)) => std::cmp::Ordering::Less,
            (QuoteAction::Submit(_), QuoteAction::Keep(_)) => std::cmp::Ordering::Greater,
        });
        plan
    }
}

#[cfg(test)]
mod tests {
    use super::{QuoteAction, QuotePlan, QuoteReconciler, ReconcilerConfig};
    use crate::runtime::ManagedOrder;
    use crate::runtime::ManagedOrderStatus;
    use crate::types::{ClientOrderId, EpochMillis, InstrumentId, MarketId, OrderIntent, TradeSide};
    use std::collections::HashMap;

    fn managed(id: &str, intent: &OrderIntent, updated_ms: EpochMillis) -> ManagedOrder {
        ManagedOrder {
            intent: intent.clone(),
            status: ManagedOrderStatus::Working,
            cumulative_filled_qty: 0.0,
            reserved_cash_usd: 0.0,
            last_update_ms: updated_ms,
        }
    }

    fn intent(id: &str, price: f64) -> OrderIntent {
        OrderIntent {
            client_order_id: ClientOrderId::from(format!("{id}")),
            market_id: MarketId::from("market-1"),
            instrument_id: InstrumentId::from("instrument-1"),
            side: TradeSide::Buy,
            limit_price: price,
            quantity: 1.0,
            reduce_only: false,
            reason: "test".to_string(),
            quote_level_tag: Some("lvl-1".to_string()),
            created_at_ms: 1,
        }
    }

    #[test]
    fn quote_reconciler_keeps_matching_quote_set() {
        let intent = intent("existing", 0.2);
        let mut open_orders = HashMap::new();
        open_orders.insert(ClientOrderId::from("existing"), managed("existing", &intent, 1));

        let mut reconciler = QuoteReconciler::new(ReconcilerConfig {
            min_order_age_ms: 0,
            max_churn_per_window: 16,
            churn_window_ms: 10_000,
            hard_pull_ms: 5_000,
            max_submit_per_window: 6,
            max_replace_per_window: 4,
            max_cancel_per_window: 12,
        });
        let desired = crate::quote_engine::DesiredQuoteSet {
            quotes: vec![crate::quote_engine::DesiredQuote {
                intent,
                level: 0,
                is_cleanup: false,
                suppress_if_stale: false,
                expires_at_ms: None,
            }],
            stale_quote_max_age_ms: None,
            quote_expiry_ms: None,
        };
        let plan = reconciler.plan(desired, &open_orders, 2);
        assert_eq!(plan.actions.len(), 1);
        assert!(matches!(plan.actions[0], QuoteAction::Keep(_)));
    }

    #[test]
    fn quote_reconciler_replaces_when_price_changes() {
        let intent = intent("existing", 0.2);
        let mut open_orders = HashMap::new();
        open_orders.insert(ClientOrderId::from("existing"), managed("existing", &intent, 1));

        let mut reconciler = QuoteReconciler::new(ReconcilerConfig {
            min_order_age_ms: 0,
            max_churn_per_window: 16,
            churn_window_ms: 10_000,
            hard_pull_ms: 5_000,
            max_submit_per_window: 6,
            max_replace_per_window: 4,
            max_cancel_per_window: 12,
        });
        let mut replacement = intent("replacement", 0.21);
        replacement.quote_level_tag = Some("lvl-1".to_string());
        let desired = crate::quote_engine::DesiredQuoteSet {
            quotes: vec![crate::quote_engine::DesiredQuote {
                intent: replacement,
                level: 0,
                is_cleanup: false,
                suppress_if_stale: false,
                expires_at_ms: None,
            }],
            stale_quote_max_age_ms: None,
            quote_expiry_ms: None,
        };
        let plan = reconciler.plan(desired, &open_orders, 2);
        assert!(matches!(plan.actions[0], QuoteAction::Replace { .. }));
    }

    #[test]
    fn quote_reconciler_keeps_when_min_age_blocks_reprice() {
        let intent = intent("existing", 0.2);
        let mut open_orders = HashMap::new();
        open_orders.insert(ClientOrderId::from("existing"), managed("existing", &intent, 9));

        let mut reconciler = QuoteReconciler::new(ReconcilerConfig {
            min_order_age_ms: 50,
            max_churn_per_window: 16,
            churn_window_ms: 10_000,
            hard_pull_ms: 5_000,
            max_submit_per_window: 6,
            max_replace_per_window: 4,
            max_cancel_per_window: 12,
        });
        let mut replacement = intent("replacement", 0.21);
        replacement.quote_level_tag = Some("lvl-1".to_string());
        let desired = crate::quote_engine::DesiredQuoteSet {
            quotes: vec![crate::quote_engine::DesiredQuote {
                intent: replacement,
                level: 0,
                is_cleanup: false,
                suppress_if_stale: false,
                expires_at_ms: None,
            }],
            stale_quote_max_age_ms: None,
            quote_expiry_ms: None,
        };
        let plan = reconciler.plan(desired, &open_orders, 10);
        assert!(matches!(plan.actions[0], QuoteAction::Keep(_)));
    }

    #[test]
    fn quote_reconciler_suppresses_submit_over_cap() {
        let mut reconciler = QuoteReconciler::new(ReconcilerConfig {
            min_order_age_ms: 0,
            max_churn_per_window: 16,
            churn_window_ms: 10_000,
            hard_pull_ms: 5_000,
            max_submit_per_window: 0,
            max_replace_per_window: 4,
            max_cancel_per_window: 12,
        });
        let desired = crate::quote_engine::DesiredQuoteSet {
            quotes: vec![crate::quote_engine::DesiredQuote {
                intent: intent("new", 0.22),
                level: 0,
                is_cleanup: false,
                suppress_if_stale: false,
                expires_at_ms: None,
            }],
            stale_quote_max_age_ms: None,
            quote_expiry_ms: None,
        };
        let plan = reconciler.plan(desired, &HashMap::new(), 2);
        assert!(plan.actions.is_empty());
        assert!(plan.notes.iter().any(|note| note.contains("submit rate cap reached")));
    }
}
