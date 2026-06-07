use crate::config::AppConfig;
use crate::metrics::AppMetrics;
use crate::risk::RiskLimits;
use crate::runtime::execution_policy::LiveSafetyState;
use crate::runtime::{Runtime, RuntimeOutcome};
use crate::strategy::StrategyMode;
use crate::types::RuntimeStatus;
use std::path::Path;

const LIVE_HEALTH_STARTUP_GRACE_MS: u64 = 15_000;

pub(super) fn needs_reconcile_order_count(runtime: &Runtime<StrategyMode>) -> usize {
    runtime
        .open_order_snapshots()
        .into_iter()
        .filter(|managed| {
            managed.status == crate::runtime::types::ManagedOrderStatus::NeedsReconcile
        })
        .count()
}

pub(super) fn enforce_live_health(
    runtime: &mut Runtime<StrategyMode>,
    metrics: &AppMetrics,
    config: &AppConfig,
    live_safety: &LiveSafetyState,
    now_ms: u64,
    started_at_ms: u64,
) -> RuntimeOutcome {
    if config.paper_mode {
        return RuntimeOutcome::default();
    }
    // Operator kill-switch is a HARD stop in EVERY runtime state. It must be
    // evaluated before the status==Running gate below, otherwise a runtime
    // parked in Starting/Degraded/RiskOff (for example behind the initial
    // reconcile gate) would never observe the kill file and never log it.
    if let Some(reason) = live_kill_switch_reason(config.live_kill_switch_path.as_deref()) {
        tracing::warn!(
            target: "polymarket_exec::runtime::live_health",
            mode = "live",
            status = ?runtime.status(),
            reason = %reason,
            "operator kill switch active: forcing degrade_and_cancel_all in every runtime state"
        );
        metrics.observe_riskoff_transition();
        return runtime.degrade_and_cancel_all(now_ms, format!("live health failure: {reason}"));
    }
    if runtime.status() != RuntimeStatus::Running {
        return RuntimeOutcome::default();
    }
    if now_ms.saturating_sub(started_at_ms) < LIVE_HEALTH_STARTUP_GRACE_MS {
        return RuntimeOutcome::default();
    }
    let (health_failures, risk_failures) =
        live_health_failures(runtime, metrics, config, live_safety);

    if health_failures.is_empty() && risk_failures.is_empty() {
        RuntimeOutcome::default()
    } else if health_failures.is_empty() {
        metrics.observe_riskoff_transition();
        runtime.riskoff_and_cancel_entry_orders(
            now_ms,
            format!("live risk limit failure: {}", risk_failures.join("; ")),
        )
    } else {
        metrics.observe_riskoff_transition();
        let mut failures = health_failures;
        failures.extend(risk_failures);
        runtime.degrade_and_cancel_all(
            now_ms,
            format!("live health failure: {}", failures.join("; ")),
        )
    }
}

pub(super) fn auto_recover_live_riskoff(
    runtime: &mut Runtime<StrategyMode>,
    metrics: &AppMetrics,
    config: &AppConfig,
    live_safety: &LiveSafetyState,
    now_ms: u64,
    started_at_ms: u64,
) -> RuntimeOutcome {
    if config.paper_mode
        || !matches!(
            runtime.status(),
            RuntimeStatus::RiskOff | RuntimeStatus::Degraded
        )
        || config.live_risk_off_auto_recover.is_zero()
    {
        return RuntimeOutcome::default();
    }
    let recover_after_ms = config
        .live_risk_off_auto_recover
        .as_millis()
        .max(LIVE_HEALTH_STARTUP_GRACE_MS as u128) as u64;
    if now_ms.saturating_sub(started_at_ms) < recover_after_ms {
        return RuntimeOutcome::default();
    }
    let (health_failures, risk_failures) =
        live_health_failures(runtime, metrics, config, live_safety);
    if health_failures.is_empty() && risk_failures.is_empty() {
        runtime.recover_live_blocked_status(
            now_ms,
            format!("live health checks passed for {recover_after_ms}ms"),
        )
    } else {
        RuntimeOutcome::default()
    }
}

fn live_health_failures(
    runtime: &Runtime<StrategyMode>,
    metrics: &AppMetrics,
    config: &AppConfig,
    live_safety: &LiveSafetyState,
) -> (Vec<String>, Vec<String>) {
    let snapshot = metrics.snapshot();
    let market_stale_ms = config
        .strategy_profile
        .as_ref()
        .and_then(|profile| profile.health.market_ws_stale_ms)
        .unwrap_or(config.book_stale_after.as_millis() as u64 * 3);
    let user_stale_ms = config
        .strategy_profile
        .as_ref()
        .and_then(|profile| profile.health.user_ws_stale_ms)
        .unwrap_or(30_000);
    let mut health_failures = Vec::new();
    let mut risk_failures = Vec::new();
    if !snapshot.market_ws_connected {
        health_failures.push("market websocket disconnected".to_string());
    }
    if snapshot.market_last_message_age_ms >= 0.0
        && snapshot.market_last_message_age_ms > market_stale_ms as f64
    {
        health_failures.push(format!(
            "market websocket stale age_ms={:.0} max_ms={market_stale_ms}",
            snapshot.market_last_message_age_ms
        ));
    }
    if !snapshot.user_ws_connected && snapshot.user_last_message_age_ms >= 0.0 {
        health_failures.push("user websocket disconnected".to_string());
    }
    // User WS is an authenticated event stream, not a heartbeat stream. It can
    // be legitimately idle for long periods when we have no fills/cancels. Do
    // not treat a connected-but-quiet User WS as unhealthy, otherwise one
    // fail-closed reconcile incident can leave the live runtime permanently
    // degraded and flat even after REST reconcile is clean. Disconnection is
    // still a hard health failure above.
    let _ = user_stale_ms;
    if !snapshot.execution_adapter_connected {
        health_failures.push("execution adapter disconnected".to_string());
    }
    let session_anchor_usd = live_risk_anchor_usd(config, live_safety);
    let free_cash_floor_usd = config.risk_limits.free_cash_floor_usd(session_anchor_usd);
    match live_safety.last_venue_cash_usd {
        Some(cash_usd) if cash_usd < free_cash_floor_usd => {
            risk_failures.push(format!(
                "venue cash below floor cash={cash_usd:.4} floor={:.4}",
                free_cash_floor_usd
            ));
        }
        Some(_) => {}
        None => health_failures.push("venue balance has not synced".to_string()),
    }
    let needs_reconcile = needs_reconcile_order_count(runtime);
    if needs_reconcile > 0 {
        health_failures.push(format!(
            "orders need reconciliation count={needs_reconcile}"
        ));
    }
    if runtime.inventory().gross_exposure_usd() > config.risk_limits.max_gross_notional_usd {
        risk_failures.push(format!(
            "gross exposure exceeded cap exposure={:.4} cap={:.4}",
            runtime.inventory().gross_exposure_usd(),
            config.risk_limits.max_gross_notional_usd
        ));
    }
    if let Some(equity_floor_usd) =
        portfolio_equity_floor_usd(&config.risk_limits, session_anchor_usd)
    {
        let local_equity_usd =
            runtime.inventory().total_cash_usd() + runtime.inventory().gross_exposure_usd();
        if local_equity_usd < equity_floor_usd {
            risk_failures.push(format!(
                "local portfolio equity below floor equity={local_equity_usd:.4} floor={equity_floor_usd:.4}"
            ));
        }
        if let Some(venue_cash_usd) = live_safety.last_venue_cash_usd {
            let venue_marked_equity_usd = venue_cash_usd + runtime.inventory().gross_exposure_usd();
            if venue_marked_equity_usd < equity_floor_usd {
                risk_failures.push(format!(
                    "venue marked equity below floor equity={venue_marked_equity_usd:.4} floor={equity_floor_usd:.4}"
                ));
            }
        }
    }
    if let Some(reason) = live_kill_switch_reason(config.live_kill_switch_path.as_deref()) {
        health_failures.push(reason);
    }

    (health_failures, risk_failures)
}

pub(super) fn live_kill_switch_reason(path: Option<&Path>) -> Option<String> {
    path.filter(|path| path.exists())
        .map(|path| format!("operator kill switch active path={}", path.display()))
}

pub(super) fn enforce_capital_guard(
    runtime: &mut Runtime<StrategyMode>,
    metrics: &AppMetrics,
    risk_limits: &RiskLimits,
    session_anchor_usd: f64,
    now_ms: u64,
    mode: &str,
) -> RuntimeOutcome {
    if runtime.status() != RuntimeStatus::Running {
        return RuntimeOutcome::default();
    }
    let Some(equity_floor_usd) = portfolio_equity_floor_usd(risk_limits, session_anchor_usd) else {
        return RuntimeOutcome::default();
    };

    let local_equity_usd =
        runtime.inventory().total_cash_usd() + runtime.inventory().gross_exposure_usd();
    if local_equity_usd >= equity_floor_usd {
        return RuntimeOutcome::default();
    }

    metrics.observe_riskoff_transition();
    runtime.riskoff_and_cancel_entry_orders(
        now_ms,
        format!(
            "{mode} capital guard: portfolio equity below floor equity={local_equity_usd:.4} floor={equity_floor_usd:.4}"
        ),
    )
}

pub(super) fn portfolio_equity_floor_usd(
    risk_limits: &RiskLimits,
    starting_cash_usd: f64,
) -> Option<f64> {
    risk_limits.portfolio_equity_floor_usd(starting_cash_usd)
}

pub(super) fn live_risk_anchor_usd(config: &AppConfig, live_safety: &LiveSafetyState) -> f64 {
    live_safety
        .session_equity_anchor_usd
        .filter(|value| value.is_finite() && *value > 0.0)
        .unwrap_or(config.starting_cash_usd)
}
