//! Live execution consumer for the shared `pm-shadow` engine.
//!
//! Decisions come ONLY from `pm_shadow::ExecIntent` (the validated engine).
//! This module handles arming, caps, kill-switch, adapter submit, and redemption.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result};
use pm_shadow::{EntryCommit, ExecIntent};
use tracing::{info, warn};

use crate::shadow_parity::{now_unix_s, SharedParityGate};
use crate::types::{ClientOrderId, InstrumentId, MarketId, TradeSide};
use crate::wire::execution_adapter::{
    ClobProtocolVersion, ExecutionAdapter, PolymarketConfig, PolymarketCredentials,
    PolymarketExecutionAdapter, PolymarketL1Credentials, PolymarketSignatureType,
    RedeemPositionsRequest, SubmitOrderAck, SubmitOrderRequest, TimeInForce,
};

const REDEEM_MARGIN_S: i64 = 60;
const FILL_POLL_ATTEMPTS: u32 = 5;
const FILL_POLL_BASE_MS: u64 = 120;

fn now_unix_ms() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

fn env_truthy(names: &[&str]) -> bool {
    names.iter().any(|name| {
        std::env::var(name)
            .ok()
            .map(|v| matches!(v.trim().to_ascii_lowercase().as_str(), "true" | "1" | "yes"))
            .unwrap_or(false)
    })
}

fn env_positive_f64(names: &[&str]) -> Option<f64> {
    names.iter().find_map(|name| {
        std::env::var(name)
            .ok()
            .and_then(|v| v.trim().parse::<f64>().ok())
            .filter(|v| v.is_finite() && *v > 0.0)
    })
}

fn env_first(names: &[&str]) -> Option<String> {
    names
        .iter()
        .find_map(|name| std::env::var(name).ok().map(|v| v.trim().to_string()))
        .filter(|v| !v.is_empty())
}

/// Venue cash snapshot for fractional clip sizing. Refreshed from
/// `sync_balances` in the execution loop; stale reads size DOWN (see
/// [`effective_clip_usd`]).
#[derive(Debug, Default, Clone, Copy)]
pub struct BalanceCache {
    pub cash_usd: Option<f64>,
    pub fetched_at_s: i64,
}

const BALANCE_REFRESH_S: i64 = 300;
const BALANCE_STALE_S: i64 = 1800;

/// Clip sizing invariant: automation may only REDUCE size, never increase it.
/// The env clip (PM_SHADOW_CLIP_USD / PM_FADE_CLIP_USD) is a hard ceiling.
/// With PM_SHADOW_CLIP_FRAC set, the clip is frac x venue cash, capped by the
/// ceiling; if the balance is unknown or stale past 30min, fall back to the
/// SMALLER of the ceiling and $10 rather than trading blind at full size.
fn effective_clip_usd(cache: &BalanceCache, now_s: i64) -> f64 {
    let ceiling = env_positive_f64(&["PM_SHADOW_CLIP_USD", "PM_FADE_CLIP_USD"]).unwrap_or(15.0);
    let frac = env_positive_f64(&["PM_SHADOW_CLIP_FRAC"]);
    clip_from_parts(ceiling, frac, cache, now_s)
}

fn clip_from_parts(ceiling: f64, frac: Option<f64>, cache: &BalanceCache, now_s: i64) -> f64 {
    let Some(frac) = frac else {
        return ceiling;
    };
    match cache.cash_usd {
        Some(cash) if now_s - cache.fetched_at_s <= BALANCE_STALE_S => (frac * cash).min(ceiling),
        _ => ceiling.min(10.0),
    }
}

async fn poll_venue_fill(
    adapter: &PolymarketExecutionAdapter,
    ack: &SubmitOrderAck,
    token_id: &str,
    submit_ms: i64,
) -> (Option<f64>, Option<f64>) {
    let Some(venue_order_id) = ack.venue_order_id.as_ref() else {
        return (None, None);
    };
    let after_ms = (submit_ms.saturating_sub(5_000)).max(0) as u64;
    for attempt in 0..FILL_POLL_ATTEMPTS {
        if attempt > 0 {
            tokio::time::sleep(Duration::from_millis(
                FILL_POLL_BASE_MS * u64::from(attempt),
            ))
            .await;
        }
        let Ok(fills) = adapter.sync_recent_fills(after_ms).await else {
            continue;
        };
        let mut total_qty = 0.0;
        let mut notional = 0.0;
        for fill in fills {
            if fill.venue_order_id.as_str() != venue_order_id.as_str()
                || fill.instrument_id.as_str() != token_id
            {
                continue;
            }
            total_qty += fill.quantity;
            notional += fill.price * fill.quantity;
        }
        if total_qty > 0.0 && notional > 0.0 {
            let price = notional / total_qty;
            if price.is_finite() && price > 0.0 {
                return (Some(total_qty), Some(price));
            }
        }
    }
    (None, None)
}

async fn resolve_fill_stats(
    adapter: &PolymarketExecutionAdapter,
    ack: &SubmitOrderAck,
    token_id: &str,
    submit_ms: i64,
) -> (Option<f64>, Option<f64>) {
    if let (Some(price), Some(qty)) = (ack.avg_fill_price, ack.filled_qty) {
        if price.is_finite() && price > 0.0 && qty > 0.0 {
            return (Some(price), Some(qty));
        }
    }
    poll_venue_fill(adapter, ack, token_id, submit_ms).await
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct PendingRedeem {
    condition_id: String,
    slug: String,
    close_ts_s: i64,
    index_sets: Vec<u64>,
}

struct RedeemLedger {
    path: PathBuf,
    pending: Vec<PendingRedeem>,
    redeemed: HashSet<String>,
}

impl RedeemLedger {
    fn load() -> Self {
        let path = PathBuf::from(
            env_first(&["PM_SHADOW_REDEEM_LEDGER_PATH", "PM_FADE_REDEEM_LEDGER_PATH"])
                .unwrap_or_else(|| "shadow_redeem_ledger.json".to_string()),
        );
        let pending = std::fs::read_to_string(&path)
            .ok()
            .and_then(|raw| serde_json::from_str(&raw).ok())
            .unwrap_or_default();
        Self {
            path,
            pending,
            redeemed: HashSet::new(),
        }
    }

    fn persist(&self) {
        if let Ok(json) = serde_json::to_string(&self.pending) {
            let _ = std::fs::write(&self.path, json);
        }
    }

    fn record_fill(&mut self, intent: &ExecIntent) {
        let Some(condition_id) = intent.condition_id.clone() else {
            return;
        };
        let index_set = if intent.side == "up" {
            intent.up_index_set
        } else {
            intent.down_index_set
        };
        if let Some(pr) = self
            .pending
            .iter_mut()
            .find(|pr| pr.condition_id == condition_id)
        {
            if !pr.index_sets.contains(&index_set) {
                pr.index_sets.push(index_set);
            }
        } else {
            self.pending.push(PendingRedeem {
                condition_id,
                slug: intent.slug.clone(),
                close_ts_s: intent.close_ts_s,
                index_sets: vec![index_set],
            });
        }
        self.persist();
    }

    fn due(&self, now_s: i64) -> Vec<PendingRedeem> {
        self.pending
            .iter()
            .filter(|pr| now_s >= pr.close_ts_s + REDEEM_MARGIN_S)
            .filter(|pr| !self.redeemed.contains(&pr.condition_id))
            .cloned()
            .collect()
    }

    fn mark_redeemed(&mut self, condition_id: &str) {
        self.redeemed.insert(condition_id.to_string());
        self.pending.retain(|pr| pr.condition_id != condition_id);
        self.persist();
    }
}

/// Safe-by-default arming (shadow / paper / live). Accepts `PM_SHADOW_*` with
/// `PM_FADE_*` fallbacks for existing deploy env.
pub struct LiveArm {
    pub live_trade_armed: bool,
    pub paper_trade_armed: bool,
    max_order_notional_usd: f64,
    max_market_notional_usd: f64,
    kill_switch_path: Option<PathBuf>,
    submitted_by_market: HashMap<String, f64>,
}

impl LiveArm {
    pub fn from_env() -> Self {
        let live_trade_requested = env_truthy(&["PM_SHADOW_LIVE_TRADE", "PM_FADE_LIVE_TRADE"]);
        let paper_mode = env_first(&["PM_SHADOW_PAPER_MODE", "PM_FADE_PAPER_MODE"])
            .map(|v| !matches!(v.to_ascii_lowercase().as_str(), "false" | "0" | "no"))
            .unwrap_or(true);
        let kill_switch_path = env_first(&["PM_SHADOW_LIVE_KILL_SWITCH_PATH", "PM_FADE_LIVE_KILL_SWITCH_PATH"])
            .map(PathBuf::from);
        let max_order_notional_usd =
            env_positive_f64(&["PM_SHADOW_MAX_ORDER_NOTIONAL_USD", "PM_FADE_MAX_ORDER_NOTIONAL_USD"]);
        let max_market_notional_usd =
            env_positive_f64(&["PM_SHADOW_MAX_MARKET_NOTIONAL_USD", "PM_FADE_MAX_MARKET_NOTIONAL_USD"]);

        let preconditions_ok = !paper_mode
            && kill_switch_path.is_some()
            && max_order_notional_usd.is_some()
            && max_market_notional_usd.is_some();
        let live_trade_armed = live_trade_requested && preconditions_ok;
        let paper_trade_armed =
            env_truthy(&["PM_SHADOW_PAPER_TRADE", "PM_FADE_PAPER_TRADE"]) && paper_mode;

        if live_trade_requested && !live_trade_armed {
            warn!(
                "PM_SHADOW_LIVE_TRADE set but preconditions missing; staying shadow-only"
            );
        }
        if live_trade_armed {
            warn!("SHADOW REAL-MONEY submission ARMED");
        }

        Self {
            live_trade_armed,
            paper_trade_armed,
            max_order_notional_usd: max_order_notional_usd.unwrap_or(0.0),
            max_market_notional_usd: max_market_notional_usd.unwrap_or(0.0),
            kill_switch_path,
            submitted_by_market: HashMap::new(),
        }
    }

    pub fn wants_execution(&self) -> bool {
        self.live_trade_armed || self.paper_trade_armed
    }

    fn kill_switch_tripped(&self) -> bool {
        self.kill_switch_path
            .as_ref()
            .map(|p| p.exists())
            .unwrap_or(false)
    }

    fn cap_notional(&self, slug: &str, candidate: f64) -> f64 {
        // Paper mode may run without live cap env vars; only enforce when configured.
        if self.max_order_notional_usd <= 0.0 && self.max_market_notional_usd <= 0.0 {
            return candidate;
        }
        let already = *self.submitted_by_market.get(slug).unwrap_or(&0.0);
        let headroom = (self.max_market_notional_usd - already).max(0.0);
        let order_cap = if self.max_order_notional_usd > 0.0 {
            self.max_order_notional_usd
        } else {
            candidate
        };
        candidate.min(order_cap).min(headroom)
    }

    fn record_submitted(&mut self, slug: &str, notional: f64) {
        *self.submitted_by_market.entry(slug.to_string()).or_insert(0.0) += notional;
    }
}

/// V2 FAK/IOC buys spend `limit_price * quantity` USDC on the market-order path.
/// Size shares off the limit (not touch) so venue spend matches the capped clip.
fn market_buy_qty(capped_usd: f64, limit_price: f64) -> f64 {
    if capped_usd > 0.0 && limit_price > 0.0 {
        ((capped_usd / limit_price) * 100.0).floor() / 100.0
    } else {
        0.0
    }
}

/// USDC the venue will debit for a shadow FAK buy at `limit_price`.
fn market_buy_usdc(limit_price: f64, quantity: f64) -> f64 {
    limit_price * quantity
}

pub async fn connect_live_adapter() -> Result<PolymarketExecutionAdapter> {
    let private_key = std::env::var("POLYMARKET_PRIVATE_KEY")
        .or_else(|_| std::env::var("METAMASK_PRIVATE_KEY"))
        .context("POLYMARKET_PRIVATE_KEY required for live arm")?;
    let signature_type = PolymarketSignatureType::parse(
        &std::env::var("POLYMARKET_SIGNATURE_TYPE").unwrap_or_else(|_| "eoa".to_string()),
    )
    .map_err(|e| anyhow::anyhow!("invalid POLYMARKET_SIGNATURE_TYPE: {e:?}"))?;
    let funder_address = std::env::var("POLYMARKET_FUNDER_ADDRESS")
        .ok()
        .or_else(|| std::env::var("POLYMARKET_PROXY_WALLET_ADDRESS").ok());

    let credentials = PolymarketL1Credentials {
        private_key,
        signature_type,
        funder_address: funder_address.clone(),
    };

    let env_some = |a: &str, b: &str| std::env::var(a).ok().or_else(|| std::env::var(b).ok());
    let mut config = PolymarketConfig::default();
    if let Ok(v) = std::env::var("POLYMARKET_CLOB_API_URL") {
        config.api_url = v;
    }
    if let Ok(v) = std::env::var("POLYMARKET_DATA_API_URL") {
        config.data_api_url = v;
    }
    if let Ok(v) = std::env::var("POLYMARKET_RELAYER_URL") {
        config.relayer_url = v;
    }
    config.relayer_api_key = env_some("RELAYER_API_KEY", "POLYMARKET_RELAYER_API_KEY");
    config.relayer_api_key_address =
        env_some("RELAYER_API_KEY_ADDRESS", "POLYMARKET_RELAYER_API_KEY_ADDRESS");
    config.proxy_wallet_address = funder_address;
    config.polygon_rpc_url = std::env::var("POLYGON_RPC_URL").ok();
    config.protocol = ClobProtocolVersion::parse(
        &std::env::var("POLYMARKET_CLOB_VERSION").unwrap_or_else(|_| "v2".to_string()),
    )
    .map_err(|e| anyhow::anyhow!("invalid POLYMARKET_CLOB_VERSION: {e:?}"))?;

    if let (Ok(api_key), Ok(api_secret), Ok(api_passphrase)) = (
        std::env::var("POLYMARKET_API_KEY"),
        std::env::var("POLYMARKET_API_SECRET"),
        std::env::var("POLYMARKET_API_PASSPHRASE"),
    ) {
        config.credentials = Some(PolymarketCredentials {
            api_key,
            api_secret,
            api_passphrase,
            private_key: credentials.private_key.clone(),
            signature_type: credentials.signature_type,
            funder_address: credentials.funder_address.clone(),
        });
    }

    PolymarketExecutionAdapter::connect_with_l1_config(config, credentials)
        .await
        .map_err(|e| anyhow::anyhow!("connect live adapter: {e}"))
}

pub async fn run_execution_loop(
    mut intent_rx: tokio::sync::mpsc::UnboundedReceiver<ExecIntent>,
    commit_tx: tokio::sync::mpsc::UnboundedSender<EntryCommit>,
    arm: Arc<Mutex<LiveArm>>,
    adapter: Option<Arc<PolymarketExecutionAdapter>>,
    parity: Option<SharedParityGate>,
    paper_mode: bool,
) {
    let ledger = Arc::new(Mutex::new(RedeemLedger::load()));
    let mut redeem_tick = tokio::time::interval(Duration::from_secs(1));
    let mut parity_tick = tokio::time::interval(Duration::from_secs(1));
    let mut balance = BalanceCache::default();
    let mut balance_tick = tokio::time::interval(Duration::from_secs(5));

    loop {
        tokio::select! {
            intent = intent_rx.recv() => {
                let Some(intent) = intent else { break };
                handle_intent(
                    intent,
                    &arm,
                    adapter.as_deref(),
                    &commit_tx,
                    &ledger,
                    parity.as_ref(),
                    paper_mode,
                    &balance,
                )
                .await;
            }
            _ = redeem_tick.tick() => {
                redeem_sweep(&arm, adapter.as_deref(), &ledger, now_unix_ms() / 1000).await;
            }
            _ = balance_tick.tick() => {
                let now_s = now_unix_ms() / 1000;
                if now_s - balance.fetched_at_s >= BALANCE_REFRESH_S {
                    if let Some(a) = adapter.as_deref() {
                        match a.sync_balances().await {
                            Ok(b) => {
                                balance = BalanceCache { cash_usd: Some(b.cash_usd), fetched_at_s: now_s };
                                info!(target: "shadow_live", cash_usd = b.cash_usd, "balance refresh");
                            }
                            Err(e) => {
                                warn!(target: "shadow_live", error = %e, "balance refresh failed");
                                balance.fetched_at_s = now_s - BALANCE_REFRESH_S + 60;
                            }
                        }
                    }
                }
            }
            _ = parity_tick.tick() => {
                if let Some(gate) = parity.as_ref() {
                    let now_s = now_unix_s();
                    let mut g = gate.lock().expect("parity gate poisoned");
                    g.tick_expired(now_s);
                    g.maybe_log_stats(now_s);
                }
            }
        }
    }
}

async fn handle_intent(
    intent: ExecIntent,
    arm: &Arc<Mutex<LiveArm>>,
    adapter: Option<&PolymarketExecutionAdapter>,
    commit_tx: &tokio::sync::mpsc::UnboundedSender<EntryCommit>,
    ledger: &Arc<Mutex<RedeemLedger>>,
    parity: Option<&SharedParityGate>,
    paper_mode: bool,
    balance: &BalanceCache,
) {
    info!(
        target: "shadow_live",
        kind = "would_enter",
        slug = %intent.slug,
        side = %intent.side,
        p_exo = intent.p_exo,
        p_side = intent.p_side,
        edge = intent.edge,
        touch = intent.touch_price,
        limit = intent.marketable_limit_price,
        clip = intent.clip,
        "shadow WOULD_ENTER"
    );

    let clip_usd = effective_clip_usd(balance, now_unix_ms() / 1000);
    let capped = {
        let mut a = arm.lock().expect("arm poisoned");
        // Paper parity audits must run with fade.kill in place; only block live money.
        if a.kill_switch_tripped() && !paper_mode {
            a.live_trade_armed = false;
            a.paper_trade_armed = false;
            warn!(slug = %intent.slug, "kill-switch: disarmed");
            let _ = commit_tx.send(EntryCommit {
                slug: intent.slug.clone(),
                filled: false,
            });
            return;
        }
        if a.live_trade_armed || a.paper_trade_armed || paper_mode {
            a.cap_notional(&intent.slug, clip_usd.min(intent.target_notional))
        } else {
            let _ = commit_tx.send(EntryCommit {
                slug: intent.slug.clone(),
                filled: true,
            });
            return;
        }
    };

    if capped <= 0.0 {
        // Deterministic rejection (caps exhausted): report filled=true so the
        // deferred entry state COMMITS and the engine stops re-emitting this
        // intent every decide tick. filled=false is reserved for transient
        // venue misses where a retry can succeed.
        warn!(slug = %intent.slug, "notional cap exhausted; consuming intent");
        let _ = commit_tx.send(EntryCommit {
            slug: intent.slug.clone(),
            filled: true,
        });
        return;
    }

    let limit = intent.marketable_limit_price.clamp(0.0, 1.0);
    let qty = market_buy_qty(capped, limit);

    let (live, paper) = {
        let a = arm.lock().expect("arm poisoned");
        (a.live_trade_armed && !paper_mode, a.paper_trade_armed && !paper_mode)
    };
    let mut filled = false;

    if qty > 0.0 {
        if let Some(gate) = parity {
            if !gate
                .lock()
                .expect("parity gate poisoned")
                .authorize_submit(&intent, now_unix_s())
            {
                let _ = commit_tx.send(EntryCommit {
                    slug: intent.slug.clone(),
                    filled: false,
                });
                return;
            }
        }
    }

    if paper_mode && qty > 0.0 {
        filled = true;
        arm.lock()
            .expect("arm poisoned")
            .record_submitted(&intent.slug, capped);
        info!(
            target: "shadow_live",
            "LIVE ENTER {} {} p_up={:.3} p_side={:.3} touch={:.2} edge={:.3} clip={}",
            intent.side.to_uppercase(),
            intent.slug,
            intent.p_exo,
            intent.p_side,
            intent.touch_price,
            intent.edge,
            intent.clip,
        );
        info!(
            target: "shadow_live",
            slug = %intent.slug,
            limit = limit,
            qty,
            notional = capped,
            "shadow SUBMITTED (paper)"
        );
    } else if live && qty > 0.0 {
        if let Some(adapter) = adapter {
            let req = SubmitOrderRequest {
                client_order_id: ClientOrderId::from(format!(
                    "shadow-live:{}:{}:{}",
                    intent.slug, intent.side, now_unix_ms()
                )),
                market_id: MarketId::from(intent.slug.as_str()),
                instrument_id: InstrumentId::from(intent.token_id.as_str()),
                side: TradeSide::Buy,
                limit_price: limit,
                quantity: qty,
                post_only: false,
                time_in_force: TimeInForce::Ioc,
                expires_at_ms: None,
                strategy_tag: "exo-fade".to_string(),
                quote_level_tag: Some("exo-fade-taker".to_string()),
                submitted_at_ms: now_unix_ms() as u64,
            };
            let submit_ms = now_unix_ms();
            match adapter.submit(req).await {
                Ok(ack) => {
                    filled = true;
                    arm.lock()
                        .expect("arm poisoned")
                        .record_submitted(&intent.slug, capped);
                    ledger.lock().expect("ledger poisoned").record_fill(&intent);
                    info!(
                        target: "shadow_live",
                        "LIVE ENTER {} {} p_up={:.3} p_side={:.3} touch={:.2} edge={:.3} clip={}",
                        intent.side.to_uppercase(),
                        intent.slug,
                        intent.p_exo,
                        intent.p_side,
                        intent.touch_price,
                        intent.edge,
                        intent.clip,
                    );
                    let (avg_fill_price, filled_qty) = resolve_fill_stats(
                        adapter,
                        &ack,
                        intent.token_id.as_str(),
                        submit_ms,
                    )
                    .await;
                    match (avg_fill_price, filled_qty) {
                        (Some(price), Some(qty)) => info!(
                            target: "shadow_live",
                            slug = %intent.slug,
                            accepted = ack.accepted,
                            "shadow SUBMITTED avg_fill_price={price:.4} filled_qty={qty:.2}"
                        ),
                        (Some(price), None) => info!(
                            target: "shadow_live",
                            slug = %intent.slug,
                            accepted = ack.accepted,
                            "shadow SUBMITTED avg_fill_price={price:.4}"
                        ),
                        _ => info!(
                            target: "shadow_live",
                            slug = %intent.slug,
                            accepted = ack.accepted,
                            "shadow SUBMITTED"
                        ),
                    }
                }
                Err(error) => {
                    warn!(target: "shadow_live", slug = %intent.slug, error = %error, "submit miss");
                }
            }
        }
    } else if paper && qty > 0.0 && !paper_mode {
        filled = true;
        arm.lock()
            .expect("arm poisoned")
            .record_submitted(&intent.slug, capped);
        info!(
            target: "shadow_live",
            "LIVE ENTER {} {} p_up={:.3} p_side={:.3} touch={:.2} edge={:.3} clip={} (paper)",
            intent.side.to_uppercase(),
            intent.slug,
            intent.p_exo,
            intent.p_side,
            intent.touch_price,
            intent.edge,
            intent.clip,
        );
        info!(target: "shadow_live", slug = %intent.slug, "shadow PAPER_FILL");
    }

    let _ = commit_tx.send(EntryCommit {
        slug: intent.slug,
        filled,
    });
}

async fn redeem_sweep(
    arm: &Arc<Mutex<LiveArm>>,
    adapter: Option<&PolymarketExecutionAdapter>,
    ledger: &Arc<Mutex<RedeemLedger>>,
    now_s: i64,
) {
    let live = {
        let mut a = arm.lock().expect("arm poisoned");
        if !a.live_trade_armed {
            return;
        }
        if a.kill_switch_tripped() {
            a.live_trade_armed = false;
            return;
        }
        true
    };
    if !live {
        return;
    }
    let Some(adapter) = adapter else {
        return;
    };

    let candidates = ledger.lock().expect("ledger poisoned").due(now_s);
    for pending in candidates {
        let index_sets = if pending.index_sets.is_empty() {
            vec![1, 2]
        } else {
            pending.index_sets.clone()
        };
        let req = RedeemPositionsRequest {
            command_id: ClientOrderId::from(format!(
                "shadow-redeem:{}:{}",
                pending.slug, now_unix_ms()
            )),
            market_id: MarketId::from(pending.slug.as_str()),
            condition_id: pending.condition_id.clone(),
            collateral_token_address: None,
            index_sets: index_sets.clone(),
            submitted_at_ms: now_unix_ms() as u64,
        };
        match adapter.redeem_positions(req).await {
            Ok(ack) if ack.accepted => {
                ledger
                    .lock()
                    .expect("ledger poisoned")
                    .mark_redeemed(&pending.condition_id);
                info!(
                    target: "shadow_live",
                    slug = %pending.slug,
                    "shadow redeem OK"
                );
            }
            Ok(ack) => {
                warn!(
                    target: "shadow_live",
                    slug = %pending.slug,
                    condition_id = %pending.condition_id,
                    accepted = ack.accepted,
                    "shadow redeem rejected by venue"
                );
            }
            Err(error) => {
                warn!(
                    target: "shadow_live",
                    slug = %pending.slug,
                    condition_id = %pending.condition_id,
                    error = %error,
                    "shadow redeem error"
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use pm_alpha::frozen_fade_decide_config;
    use pm_shadow::frozen_shadow_final_args;

    #[test]
    fn market_buy_qty_caps_usdc_at_limit_not_touch() {
        // 8:40 ET tail: touch 7c, limit 15.2c, $25 clip.
        let qty = super::market_buy_qty(25.0, 0.152);
        assert!((super::market_buy_usdc(0.152, qty) - 25.0).abs() < 0.01);
        // Old touch-based sizing would have sent ~$54.
        let bad_qty = ((25.0_f64 / 0.07) * 100.0).floor() / 100.0;
        assert!(super::market_buy_usdc(0.152, bad_qty) > 50.0);
    }

    #[test]
    fn market_buy_qty_down_clip_stays_within_cap() {
        let qty = super::market_buy_qty(25.0, 0.584);
        assert!((super::market_buy_usdc(0.584, qty) - 25.0).abs() < 0.01);
    }

    /// Agent must use the same frozen config as shadow-final / backtest SSOT.
    #[test]
    fn frozen_shadow_final_args_matches_decide_ssot() {
        let args = frozen_shadow_final_args(PathBuf::from("shadow-agent"));
        let decide = frozen_fade_decide_config(50.0);

        assert_eq!(args.edge_threshold, decide.edge_threshold);
        assert_eq!(args.min_entry_sigma_bps, decide.min_entry_sigma_bps);
        assert_eq!(args.rearm_edge, decide.rearm_edge);
        assert_eq!(args.max_clips, 2);
        assert_eq!(args.exit_after_s, decide.exit_after_s);
        assert_eq!(args.stop_before_close_s, decide.stop_before_close_s);
        assert!(args.skip_saturday);
        assert!((args.perp_price_weight - 0.75).abs() < f64::EPSILON);
        assert_eq!(args.vol_lookback_s, 3600);
        assert_eq!(args.vol_estimator, "realized");
        assert!(!args.lane_late_fav);
        assert_eq!(args.enter_within_close_s, 0);
        assert_eq!(args.latency_probe_ms, 150);
    }

    #[test]
    fn clip_sizing_only_reduces_never_increases() {
        let fresh = super::BalanceCache { cash_usd: Some(1000.0), fetched_at_s: 1000 };
        // No fraction configured: ceiling passes through.
        assert_eq!(super::clip_from_parts(15.0, None, &fresh, 1000), 15.0);
        // Fractional sizing below the ceiling.
        assert_eq!(super::clip_from_parts(50.0, Some(0.01), &fresh, 1000), 10.0);
        // Fraction can never exceed the ceiling even with a big balance.
        let rich = super::BalanceCache { cash_usd: Some(1_000_000.0), fetched_at_s: 1000 };
        assert_eq!(super::clip_from_parts(15.0, Some(0.01), &rich, 1000), 15.0);
        // Unknown balance with fraction configured: conservative floor, not the ceiling.
        let unknown = super::BalanceCache::default();
        assert_eq!(super::clip_from_parts(50.0, Some(0.01), &unknown, 1000), 10.0);
        // Stale balance (>30min) also sizes down.
        let stale = super::BalanceCache { cash_usd: Some(1000.0), fetched_at_s: 0 };
        assert_eq!(super::clip_from_parts(50.0, Some(0.01), &stale, 2000), 10.0);
        // Ceiling below the $10 fallback stays authoritative.
        assert_eq!(super::clip_from_parts(5.0, Some(0.01), &unknown, 1000), 5.0);
    }
}
