use crate::runtime::types::RuntimeOutcome;
use crate::runtime::types::{ManagedOrder, ManagedOrderStatus};
use crate::runtime::Runtime;
use crate::types::EpochMillis;

pub fn reconcile_open_orders<S: crate::strategy::Strategy>(
    runtime: &mut Runtime<S>,
    now_ms: EpochMillis,
    stale_after_ms: u64,
) -> RuntimeOutcome {
    let mut outcome = runtime.sync_open_orders_from_store(now_ms);

    let stale_client_orders: Vec<_> = runtime
        .open_order_snapshots()
        .into_iter()
        .filter_map(|managed| {
            stale_submit_reconcile_reason(&managed, now_ms, stale_after_ms)
                .map(|reason| (managed.intent.client_order_id, reason))
        })
        .collect();

    for (client_order_id, reason) in stale_client_orders {
        outcome.extend(runtime.mark_order_needs_reconcile(&client_order_id, now_ms, reason));
    }

    let stale_cancel_orders: Vec<_> = runtime
        .open_order_snapshots()
        .into_iter()
        .filter_map(|managed| {
            stale_cancel_reconcile_reason(&managed, now_ms, stale_after_ms)
                .map(|reason| (managed.intent.client_order_id, reason))
        })
        .collect();

    for (client_order_id, reason) in stale_cancel_orders {
        outcome.extend(runtime.mark_order_needs_reconcile(&client_order_id, now_ms, reason));
    }

    outcome
}

fn stale_submit_reconcile_reason(
    managed: &ManagedOrder,
    now_ms: EpochMillis,
    stale_after_ms: u64,
) -> Option<String> {
    if !matches!(
        managed.status,
        ManagedOrderStatus::PendingSubmit | ManagedOrderStatus::Submitted
    ) {
        return None;
    }
    let age_ms = now_ms.saturating_sub(managed.last_update_ms);
    if age_ms <= stale_after_ms {
        return None;
    }
    Some(format!(
        "fail-closed stale submit state {:?}: age_ms={} stale_after_ms={}",
        managed.status, age_ms, stale_after_ms
    ))
}

fn stale_cancel_reconcile_reason(
    managed: &ManagedOrder,
    now_ms: EpochMillis,
    stale_after_ms: u64,
) -> Option<String> {
    if managed.status != ManagedOrderStatus::CancelRequested {
        return None;
    }
    let age_ms = now_ms.saturating_sub(managed.last_update_ms);
    if age_ms <= stale_after_ms {
        return None;
    }
    Some(format!(
        "fail-closed stale cancel state CancelRequested: age_ms={} stale_after_ms={}",
        age_ms, stale_after_ms
    ))
}
