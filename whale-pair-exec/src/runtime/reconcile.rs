use crate::runtime::Runtime;
use crate::runtime::types::ManagedOrderStatus;
use crate::runtime::types::RuntimeOutcome;
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
        .filter(|managed| {
            let age_ms = now_ms.saturating_sub(managed.last_update_ms);
            age_ms > stale_after_ms
                && matches!(
                    managed.status,
                    ManagedOrderStatus::PendingSubmit | ManagedOrderStatus::Submitted
                )
        })
        .map(|managed| managed.intent.client_order_id)
        .collect();

    for client_order_id in stale_client_orders {
        outcome.extend(runtime.mark_order_needs_reconcile(
            &client_order_id,
            now_ms,
            "order submit state exceeded reconciliation window",
        ));
    }

    let stale_cancel_orders: Vec<_> = runtime
        .open_order_snapshots()
        .into_iter()
        .filter(|managed| {
            let age_ms = now_ms.saturating_sub(managed.last_update_ms);
            age_ms > stale_after_ms && managed.status == ManagedOrderStatus::CancelRequested
        })
        .map(|managed| managed.intent.client_order_id)
        .collect();

    for client_order_id in stale_cancel_orders {
        outcome.extend(runtime.mark_order_needs_reconcile(
            &client_order_id,
            now_ms,
            "order cancel state exceeded reconciliation window",
        ));
    }

    outcome
}
