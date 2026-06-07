//! Live glue for the proven BTC-5m br2 strategy.
//!
//! This module is ADDITIVE. It drives the read-only [`Br2ShadowAdapter`] from
//! the runtime's live feeds (Binance spot tape + Polymarket YES top-of-book)
//! and LOGS the orders br2 WOULD place.
//!
//! Three layered behaviors, each behind its own flag:
//!   - `PM_BTC_5M_BR2_SHADOW=true` enables the SHADOW path: br2 decides and
//!     LOGS. It submits nothing. This is the existing, proven behavior.
//!   - `PM_BTC_5M_BR2_PAPER_TRADE=true` ARMS paper submission. When armed AND
//!     the runtime is in paper mode, the ShadowOrders br2 produces each tick are
//!     converted to [`OrderIntent`]s and handed to the runner, which feeds them
//!     through the SAME paper-fill simulator + safety path as on_book_state's
//!     `Submit` commands. The shadow LOGGING is unchanged.
//!   - `PM_BTC_5M_BR2_LIVE_TRADE=true` ARMS REAL-MONEY submission. This is the
//!     DORMANT real arm: it only takes effect when the runtime is NOT in paper
//!     mode AND every precondition below holds. When armed, the same converted
//!     [`OrderIntent`]s flow through the SAME live execution + six safety layers
//!     as every other live order. It is impossible to arm by accident.
//!
//! REAL-MONEY PRECONDITIONS (ALL required, enforced in [`Br2LiveShadow::from_env`]):
//!   - `PM_BTC_5M_BR2_LIVE_TRADE` truthy, AND
//!   - runtime is NOT in paper mode (`paper_mode == false`), AND
//!   - a kill-switch path is configured (`PM_BTC_5M_LIVE_KILL_SWITCH_PATH`
//!     non-empty), AND
//!   - a per-order notional cap `PM_BTC_5M_BR2_MAX_ORDER_NOTIONAL_USD` is set
//!     and > 0, AND
//!   - a per-market cumulative notional cap
//!     `PM_BTC_5M_BR2_MAX_MARKET_NOTIONAL_USD` is set and > 0.
//! If `PM_BTC_5M_BR2_LIVE_TRADE` is set but ANY precondition is missing (or the
//! runtime is in paper mode), we REFUSE: warn loudly and leave
//! `live_trade_armed=false` (shadow logging still runs, no real orders).
//!
//! SAFETY INVARIANT: br2 orders may ONLY be submitted when EITHER paper-trade is
//! armed (which itself requires paper_mode) OR live-trade is armed (which itself
//! requires !paper_mode AND all preconditions). The two arms are mutually
//! exclusive (one needs paper_mode, the other needs !paper_mode), so they can
//! never both be true. [`Br2LiveShadow::decide_tick`] returns intents only when
//! one of the two arms is set; otherwise it returns zero intents and the runner
//! submits nothing.
//!
//! It is OFF by default. With all flags unset, [`Br2LiveShadow::from_env`]
//! returns `None` and the runtime does no br2 work at all, leaving the existing
//! live strategy path untouched.
//!
//! Wiring (see runtime/runner.rs):
//!   - the Binance spot mpsc consumer taps each `SpotTradeEvent` into
//!     [`Br2LiveShadow::on_spot_trade`] WITHOUT disturbing the existing
//!     `on_btc_trade` regime consumer.
//!   - the runtime book tick drives [`Br2LiveShadow::decide_tick`] for the
//!     active BTC-5m market, which handles open/close lifecycle and runs one
//!     position-aware shadow decision per YES book update (1s cadence). When
//!     paper-trade is armed, the runner converts the returned ShadowOrders to
//!     OrderIntents and enqueues them into the book outcome before execution.

use std::collections::hash_map::DefaultHasher;
use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::path::Path;

use tracing::{info, warn};

use crate::book::BookState;
use crate::market_context::MarketContextRecord;
use crate::runtime::br2_shadow::{
    Br2ShadowAdapter, DecisionPosition, ShadowOrder, SpotTrade, YesTopOfBook,
};
use crate::types::{ClientOrderId, InstrumentId, MarketId, OrderIntent, TradeSide};
use pm_strategy::Side;
use pm_types::TradeHistory;

const NS_PER_MS: i64 = 1_000_000;
/// Minimum spacing between shadow decisions for one market. The 062901 backtest
/// sampled the book on a 1s cadence; mirror that to keep decisions comparable
/// and avoid spamming the log on every book delta.
const DECISION_CADENCE_MS: u64 = 1_000;
/// Minimum spacing between "br2 produced no order" diagnostics for one market.
/// The decision cadence is 1s; across many live markets an unthrottled diagnostic
/// would flood the log, so emit at most once per this window per market.
const NO_ORDER_DIAG_THROTTLE_MS: u64 = 15_000;

/// Tracks the runtime-active BTC-5m market the adapter is currently driving.
struct ActiveMarket {
    /// The runtime's string market id (gamma market id).
    market_id: MarketId,
    /// Stable u32 the adapter labels decisions with (hash of `market_id`).
    market_u32: u32,
    /// YES instrument id (`instrument_ids[0]`); its book IS the YES top-of-book.
    yes_asset_id: String,
    /// NO instrument id (`instrument_ids[1]`), if the market exposes one.
    no_asset_id: Option<String>,
    /// This market's resolution time (unix ms). We expire by OUR active
    /// market's close, not the ticked record's, since the runtime keeps
    /// ticking already-resolved windows.
    close_ms: u64,
    /// Last decision timestamp (unix ms) for the 1s cadence throttle.
    last_decision_ms: u64,
    /// Last no-order diagnostic timestamp (unix ms). Throttles the "br2 fired
    /// nothing" diagnostic to at most once per `NO_ORDER_DIAG_THROTTLE_MS` per
    /// market so the 1s cadence across many markets does not flood the log.
    last_no_order_diag_ms: u64,
    /// Monotonic count of decisions br2 has made in this market. Fed into
    /// `Ctx.events_seen` so position-aware/lane gates see a real progression
    /// (the backtest feeds a monotonic events count, not 0).
    events_seen: u64,
}

/// Per-tick result of driving the br2 decision engine.
///
/// `orders` are the ShadowOrders br2 decided this tick (already logged by
/// [`Br2LiveShadow::decide_tick`]). `submit_intents`, when present, are the
/// SAME orders converted to runtime [`OrderIntent`]s for the paper-fill path.
/// It is `Some` only when paper-trade is armed AND the runtime is in paper
/// mode; otherwise it is `None` and the runner must not submit anything.
#[derive(Debug, Default)]
pub struct Br2TickResult {
    pub orders: Vec<ShadowOrder>,
    pub submit_intents: Vec<OrderIntent>,
}

/// Runtime-side driver that feeds the br2 shadow adapter from live feeds.
pub struct Br2LiveShadow {
    adapter: Br2ShadowAdapter,
    active: Option<ActiveMarket>,
    /// True when `PM_BTC_5M_BR2_PAPER_TRADE` is armed AND the runtime is in
    /// paper mode. Lets `decide_tick` emit submit intents into the paper-fill
    /// path. Can never be true outside paper mode (enforced in `from_env`).
    paper_trade_armed: bool,
    /// True when `PM_BTC_5M_BR2_LIVE_TRADE` is armed AND the runtime is NOT in
    /// paper mode AND every real-money precondition holds (kill-switch path +
    /// both notional caps). This is the ONLY switch that lets `decide_tick` emit
    /// submit intents into the REAL execution path. Can never be true in paper
    /// mode, and is mutually exclusive with `paper_trade_armed` (enforced in
    /// `from_env`).
    live_trade_armed: bool,
    /// Per-order notional cap in USD (price*shares). Belt-and-suspenders bound
    /// enforced INSIDE this module before emitting, independent of the runtime's
    /// own caps. Only populated when `live_trade_armed`.
    max_order_notional_usd: f64,
    /// Per-market cumulative submitted-notional cap in USD. Once a market's
    /// running submitted notional reaches this, further br2 orders for that
    /// market are refused. Only populated when `live_trade_armed`.
    max_market_notional_usd: f64,
    /// Running submitted notional per market id, for the cumulative cap above.
    /// Keyed on the runtime market id string. Only used when `live_trade_armed`.
    submitted_notional_by_market: HashMap<String, f64>,
    /// One-time guard so the cold-start warmup skip ("spot tape does not cover
    /// open") logs once rather than every throttled tick until the tape fills.
    warmup_skip_warned: bool,
}

impl Br2LiveShadow {
    /// Construct the live driver IFF `PM_BTC_5M_BR2_SHADOW` is truthy. Returns
    /// `None` (no-op) otherwise so the runtime path stays unchanged.
    ///
    /// `paper_mode` is the runtime's effective paper-mode flag (read from
    /// AppConfig by the caller). It HARD-GATES both arms in opposite directions:
    ///   - paper submission (`PM_BTC_5M_BR2_PAPER_TRADE`) requires `paper_mode == true`.
    ///   - REAL-MONEY submission (`PM_BTC_5M_BR2_LIVE_TRADE`) requires `paper_mode == false`.
    /// If an arm flag is set in the wrong mode (or, for live, a precondition is
    /// missing), we WARN loudly and leave that arm disabled (shadow logging
    /// still runs).
    ///
    /// `live_kill_switch_path` is the runtime's configured kill-switch path
    /// (`config.live_kill_switch_path`, sourced from
    /// `PM_BTC_5M_LIVE_KILL_SWITCH_PATH`). A non-empty path is a REQUIRED
    /// precondition for arming real-money submission: with no kill switch
    /// configured, the live arm refuses regardless of the other flags.
    ///
    /// When enabled, loads the frozen meta-calibrator snapshot from
    /// `BR2_SNAPSHOT_PATH` if set; otherwise runs with a fresh calibrator (the
    /// gates are more conservative but the code path is identical).
    pub fn from_env(paper_mode: bool, live_kill_switch_path: Option<&Path>) -> Option<Self> {
        let enabled = env_truthy("PM_BTC_5M_BR2_SHADOW");
        if !enabled {
            return None;
        }

        let paper_trade_requested = env_truthy("PM_BTC_5M_BR2_PAPER_TRADE");
        let paper_trade_armed = paper_trade_requested && paper_mode;
        if paper_trade_requested && !paper_mode {
            warn!(
                target: "br2_shadow",
                "BR2-PAPER-TRADE arm flag PM_BTC_5M_BR2_PAPER_TRADE is set but the runtime is \
                 NOT in paper mode; refusing paper submission. br2 stays SHADOW-ONLY for the \
                 paper arm. (Real-money submission is governed separately by \
                 PM_BTC_5M_BR2_LIVE_TRADE.)"
            );
        }
        if paper_trade_armed {
            info!(
                target: "br2_shadow",
                "BR2-PAPER-TRADE armed: br2 orders WILL be submitted through the paper-fill \
                 simulator (paper_mode=true). No real-money submission occurs."
            );
        }

        // REAL-MONEY arm. Dormant by default. Arming requires PM_BTC_5M_BR2_LIVE_TRADE
        // truthy AND !paper_mode AND every precondition below. Any failure REFUSES
        // (warns loudly, leaves the arm off). The two notional caps are also the
        // belt-and-suspenders bounds enforced in decide_tick.
        let live_trade_requested = env_truthy("PM_BTC_5M_BR2_LIVE_TRADE");
        let kill_switch_configured = live_kill_switch_path
            .map(|p| !p.as_os_str().is_empty())
            .unwrap_or(false);
        let max_order_notional_usd = env_positive_f64("PM_BTC_5M_BR2_MAX_ORDER_NOTIONAL_USD");
        let max_market_notional_usd = env_positive_f64("PM_BTC_5M_BR2_MAX_MARKET_NOTIONAL_USD");

        let live_preconditions_ok = !paper_mode
            && kill_switch_configured
            && max_order_notional_usd.is_some()
            && max_market_notional_usd.is_some();
        let live_trade_armed = live_trade_requested && live_preconditions_ok;

        if live_trade_requested && !live_trade_armed {
            // Spell out exactly which precondition failed so a refusal is never silent.
            warn!(
                target: "br2_shadow",
                paper_mode,
                kill_switch_configured,
                max_order_notional_usd = ?max_order_notional_usd,
                max_market_notional_usd = ?max_market_notional_usd,
                "BR2-LIVE-TRADE arm flag PM_BTC_5M_BR2_LIVE_TRADE is set but a precondition is \
                 missing; REFUSING real-money submission. Required: !paper_mode AND \
                 PM_BTC_5M_LIVE_KILL_SWITCH_PATH non-empty AND \
                 PM_BTC_5M_BR2_MAX_ORDER_NOTIONAL_USD>0 AND \
                 PM_BTC_5M_BR2_MAX_MARKET_NOTIONAL_USD>0. br2 stays SHADOW-ONLY (log only). \
                 No real orders will be placed."
            );
        }
        if live_trade_armed {
            warn!(
                target: "br2_shadow",
                max_order_notional_usd = max_order_notional_usd.unwrap_or(0.0),
                max_market_notional_usd = max_market_notional_usd.unwrap_or(0.0),
                kill_switch = ?live_kill_switch_path,
                "BR2 REAL-MONEY submission ARMED: max_order=${} max_market=${} kill_switch={:?}. \
                 Real orders WILL be placed.",
                max_order_notional_usd.unwrap_or(0.0),
                max_market_notional_usd.unwrap_or(0.0),
                live_kill_switch_path,
            );
        }

        let snapshot = match std::env::var("BR2_SNAPSHOT_PATH")
            .ok()
            .filter(|p| !p.trim().is_empty())
        {
            Some(path) => {
                match Br2ShadowAdapter::load_snapshot_from_path(std::path::Path::new(&path)) {
                    Ok(snap) => {
                        info!(target: "br2_shadow", path = %path, "BR2-SHADOW loaded frozen meta-calibrator snapshot");
                        Some(snap)
                    }
                    Err(error) => {
                        warn!(
                            target: "br2_shadow",
                            path = %path,
                            error = %error,
                            "BR2-SHADOW failed to load snapshot; running with a fresh calibrator"
                        );
                        None
                    }
                }
            }
            None => {
                info!(target: "br2_shadow", "BR2-SHADOW enabled with a fresh meta-calibrator (no BR2_SNAPSHOT_PATH)");
                None
            }
        };

        let mut adapter = Br2ShadowAdapter::new(snapshot);

        // Optional override for the late-favourite model-edge gate, letting us run
        // the validated edge06 config without hardcoding divergence. Unset =>
        // byte-identical to the champion (base 0.09). When set, also clamp the
        // high-cert edge to <= the new base so the high-cert path is never
        // stricter than the base.
        if let Some(edge) = env_unit_interval_f64("PM_BTC_5M_BR2_MIN_MODEL_EDGE") {
            let prev = adapter.strategy_config().late_favourite_min_model_edge;
            adapter.set_late_favourite_min_model_edge(edge as f32);
            info!(
                target: "br2_shadow",
                new_edge = edge,
                prev_edge = prev,
                "br2 min_model_edge override = {} (was {})",
                edge,
                prev,
            );
        }

        // Optional override for the directional-lane realized-vol floor (180s,
        // bps), applied to all three lanes at once. Unset => byte-identical to
        // the champion (1.25 on every lane). Accepts 0 to fully disable the
        // floor.
        if let Some(v) = env_nonneg_f64("PM_BTC_5M_BR2_MIN_REALIZED_VOL_BPS") {
            adapter.set_min_realized_vol_180s_bps(v as f32);
            info!(
                target: "br2_shadow",
                "br2 min_realized_vol_180s_bps override = {} (was 1.25)",
                v,
            );
        }

        // Optional override for the late-favourite minimum ask gate ("askwide"),
        // a price in (0,1). Unset => byte-identical to the champion (0.70).
        if let Some(v) = env_unit_interval_f64("PM_BTC_5M_BR2_MIN_ASK") {
            adapter.set_late_favourite_min_ask(v as f32);
            info!(
                target: "br2_shadow",
                "br2 late_favourite_min_ask override = {} (was 0.70)",
                v,
            );
        }

        Some(Self {
            adapter,
            active: None,
            paper_trade_armed,
            live_trade_armed,
            max_order_notional_usd: max_order_notional_usd.unwrap_or(0.0),
            max_market_notional_usd: max_market_notional_usd.unwrap_or(0.0),
            submitted_notional_by_market: HashMap::new(),
            warmup_skip_warned: false,
        })
    }

    /// True when this driver is armed to submit through the paper-fill path.
    pub fn paper_trade_armed(&self) -> bool {
        self.paper_trade_armed
    }

    /// True when this driver is armed to submit through the REAL execution path.
    /// Can only be true when the runtime is NOT in paper mode and every
    /// precondition held at construction (see `from_env`).
    pub fn live_trade_armed(&self) -> bool {
        self.live_trade_armed
    }

    /// Tap one live Binance aggTrade print into the adapter's trailing tape.
    /// Maps `is_buyer_maker: None -> false` (Coinbase / unknown aggressor side)
    /// and converts the ms event timestamp to ns.
    pub fn on_spot_trade(
        &mut self,
        price: f64,
        quantity: f64,
        observed_at_ms: u64,
        is_buyer_maker: Option<bool>,
    ) {
        self.adapter.on_spot_trade(SpotTrade {
            ts_ns: (observed_at_ms as i64).saturating_mul(NS_PER_MS),
            price,
            quantity: quantity as f32,
            is_buyer_maker: is_buyer_maker.unwrap_or(false),
        });
    }

    /// Drive the BTC-5m lifecycle + one shadow decision from a YES book update.
    ///
    /// `market_id` is the runtime market for `yes_book`; `record` is its market
    /// context (carries open/close times and the YES/NO instruments). `yes_book`
    /// is the just-updated book; we only act when it is the YES asset
    /// (`instrument_ids[0]`). `pos` is the real runtime paper position for this
    /// market (yes_shares, no_shares, cash), threaded into br2's position-aware
    /// lanes. `no_book` is the current NO-token book snapshot, used only for
    /// marketable-price fallback when a ShadowOrder has no explicit limit.
    ///
    /// Returns a [`Br2TickResult`]: always the decided ShadowOrders (also
    /// logged here), plus `submit_intents` IFF paper-trade is armed. The caller
    /// MUST NOT submit anything when `submit_intents` is empty.
    pub fn decide_tick(
        &mut self,
        market_id: &MarketId,
        record: &MarketContextRecord,
        yes_book: &BookState,
        pos: DecisionPosition,
        no_book: Option<&BookState>,
        now_ms: u64,
    ) -> Br2TickResult {
        let (Some(open_ms), Some(close_ms)) =
            (record.event_start_time_ms, record.event_end_time_ms)
        else {
            return Br2TickResult::default();
        };
        let Some(yes_asset_id) = record.instrument_ids.first() else {
            return Br2TickResult::default();
        };
        let no_asset_id = record.instrument_ids.get(1).cloned();

        // Expire OUR active market once we are past ITS resolution time. We key
        // on the active market's own close_ms (not the ticked record's) because
        // the runtime keeps ticking already-resolved windows.
        if let Some(active) = &self.active {
            if now_ms > active.close_ms {
                info!(
                    target: "br2_shadow",
                    market = %active.market_id,
                    market_u32 = active.market_u32,
                    "BR2-SHADOW closed BTC-5m market"
                );
                self.adapter.on_market_close();
                self.active = None;
            }
        }

        // Ignore ticks for a market that is not currently live (already resolved
        // or not yet open). Never (re)open a resolved window — that was the
        // open/close thrash bug that reset per-market state every tick.
        if now_ms > close_ms || now_ms < open_ms {
            return Br2TickResult::default();
        }

        // Roll to a new LIVE BTC-5m window if the runtime moved on.
        let is_new_market = match &self.active {
            Some(active) => &active.market_id != market_id,
            None => true,
        };
        if is_new_market {
            if self.active.is_some() {
                self.adapter.on_market_close();
            }
            let market_u32 = stable_market_u32(market_id);
            let open_ns = (open_ms as i64).saturating_mul(NS_PER_MS);
            let close_ns = (close_ms as i64).saturating_mul(NS_PER_MS);
            // Cold-start warmup guard: refuse to open (and decide) a market
            // until the spot tape actually covers its open_ns with a valid
            // (>0) price. Without this the strike proxy is spot_at_open=0.0
            // and the model runs on a garbage strike. Once the tape spans the
            // model window (~300s) the open succeeds and markets open normally.
            if !self.adapter.on_market_open(market_u32, open_ns, close_ns) {
                if !self.warmup_skip_warned {
                    self.warmup_skip_warned = true;
                    warn!(
                        target: "br2_shadow",
                        market = %market_id,
                        market_u32,
                        open_ms,
                        close_ms,
                        "br2 skipping market: spot tape does not cover open (warmup)"
                    );
                }
                self.active = None;
                return Br2TickResult::default();
            }
            self.warmup_skip_warned = false;
            info!(
                target: "br2_shadow",
                market = %market_id,
                market_u32,
                open_ms,
                close_ms,
                yes_asset = %yes_asset_id,
                "BR2-SHADOW opened BTC-5m market"
            );
            self.active = Some(ActiveMarket {
                market_id: market_id.clone(),
                market_u32,
                yes_asset_id: yes_asset_id.clone(),
                no_asset_id: no_asset_id.clone(),
                close_ms,
                last_decision_ms: 0,
                last_no_order_diag_ms: 0,
                events_seen: 0,
            });
        }

        // Only the YES book drives decisions. Throttle to the 1s cadence.
        let should_decide = match &self.active {
            Some(active) => {
                yes_book.asset_id == active.yes_asset_id
                    && now_ms.saturating_sub(active.last_decision_ms) >= DECISION_CADENCE_MS
            }
            None => false,
        };
        if !should_decide {
            return Br2TickResult::default();
        }
        // Need a two-sided YES top-of-book to build the decision input.
        if yes_book.best_bid <= 0.0 || yes_book.best_ask <= 0.0 {
            return Br2TickResult::default();
        }

        let market_u32 = self.active.as_ref().map(|a| a.market_u32).unwrap_or(0);
        let tob = YesTopOfBook {
            ts_ns: (now_ms as i64).saturating_mul(NS_PER_MS),
            yes_bid: yes_book.best_bid as f32,
            yes_bid_size: yes_book.best_bid_size as f32,
            yes_ask: yes_book.best_ask as f32,
            yes_ask_size: yes_book.best_ask_size as f32,
        };

        // Position-aware decision: feed the REAL runtime paper position so br2's
        // side-lock and tail-coverage lanes behave as in the backtest. Build the
        // canonical ReplayEvent from the same spot/strike logic the adapter uses.
        // Feed br2 a monotonic per-market events count (the backtest does not
        // feed 0 every event). Position fields stay the real runtime inventory.
        let mut pos = pos;
        if let Some(active) = self.active.as_ref() {
            pos.events_seen = active.events_seen;
        }
        let orders = match self.adapter.build_live_event(&tob) {
            Some(event) => {
                let trades = TradeHistory::default();
                self.adapter.on_decision_event(&event, pos, &trades)
            }
            None => Vec::new(),
        };
        if let Some(active) = self.active.as_mut() {
            active.last_decision_ms = now_ms;
            active.events_seen = active.events_seen.saturating_add(1);
        }
        for order in &orders {
            info!(
                target: "br2_shadow",
                market = %market_id,
                market_u32,
                "BR2-SHADOW {}",
                order.log_line()
            );
        }

        // Diagnostic: when br2 produced NO order in a decidable tick (in-window,
        // two-sided YES book present, cadence elapsed) emit a THROTTLED snapshot
        // of WHY nothing fired. Lets a reader tell correct abstention (no
        // qualifying favourite / model gate unmet) from an integration bug
        // (garbage model output, or gates met but still no order). Read-only: no
        // decision logic runs here.
        if orders.is_empty() {
            let due = self
                .active
                .as_ref()
                .map(|a| {
                    now_ms.saturating_sub(a.last_no_order_diag_ms) >= NO_ORDER_DIAG_THROTTLE_MS
                })
                .unwrap_or(false);
            if due {
                let secs_to_close = (close_ms as i64 - now_ms as i64) as f64 / 1000.0;
                let yes_mid = 0.5 * (yes_book.best_bid + yes_book.best_ask);
                let model = self.adapter.last_model_output();
                let confidence_score = model.map(|m| m.confidence_score);
                let risk_score = model.map(|m| m.risk_score);
                let direction_score = model.map(|m| m.direction_score);
                let calibrated_p = model.map(|m| m.calibrated_p);
                // YES-side edge vs mid, the value the external model gate checks
                // for a YES-adding order (BuyYes). Clamped like the gate does.
                let side_edge_vs_mid = model
                    .map(|m| pm_model::side_edge_vs_mid(&m, yes_mid as f32, true).clamp(0.0, 1.0));
                let gate = self.adapter.model_gate();
                let stats = self.adapter.gate_stats();
                info!(
                    target: "br2_shadow",
                    market = %market_id,
                    market_u32,
                    secs_to_close,
                    yes_bid = yes_book.best_bid,
                    yes_ask = yes_book.best_ask,
                    yes_mid,
                    confidence_score = ?confidence_score,
                    risk_score = ?risk_score,
                    direction_score = ?direction_score,
                    calibrated_p = ?calibrated_p,
                    side_edge_vs_mid = ?side_edge_vs_mid,
                    spot_at_open = ?self.adapter.spot_at_open(),
                    gate_min_confidence = gate.min_confidence,
                    gate_max_risk = gate.max_risk,
                    gate_min_edge = gate.min_edge,
                    gate_stats = ?stats,
                    "BR2-SHADOW no order this tick (decidable): abstain-or-gate diagnostic"
                );
                if let Some(active) = self.active.as_mut() {
                    active.last_no_order_diag_ms = now_ms;
                }
            }
        }

        // Convert to submit intents ONLY when an arm is set. Both arms are
        // hard-gated in `from_env`: `paper_trade_armed` requires paper_mode,
        // `live_trade_armed` requires !paper_mode + all preconditions, and they
        // are mutually exclusive. When neither is armed we return zero intents
        // and the runner submits nothing (byte-identical to shadow-only).
        let submit_intents = if self.paper_trade_armed || self.live_trade_armed {
            let yes_token = InstrumentId::from(yes_asset_id.as_str());
            let no_token = no_asset_id
                .as_ref()
                .map(|id| InstrumentId::from(id.as_str()));
            // Tag intents as IOC takers ONLY on the real-money arm. The paper
            // arm leaves the tag None (byte-identical attribution to before this
            // increment); the paper-fill sim crosses immediately regardless.
            let tag_taker = self.live_trade_armed;
            let mut intents = Vec::new();
            for order in &orders {
                let Some(intent) = shadow_order_to_intent(
                    order,
                    market_id,
                    &yes_token,
                    no_token.as_ref(),
                    yes_book,
                    no_book,
                    now_ms,
                    tag_taker,
                ) else {
                    continue;
                };
                // Belt-and-suspenders notional caps, enforced HERE before
                // emitting, independent of the runtime's own caps. Only active
                // for the live arm (caps are only populated when live_trade_armed;
                // the paper arm keeps its existing, unbounded-here behavior so it
                // stays byte-identical to before this increment).
                let intent = if self.live_trade_armed {
                    match self.apply_notional_caps(intent, market_id) {
                        Some(capped) => capped,
                        None => continue,
                    }
                } else {
                    intent
                };
                intents.push(intent);
            }
            intents
        } else {
            Vec::new()
        };

        Br2TickResult {
            orders,
            submit_intents,
        }
    }

    /// Belt-and-suspenders notional caps for the REAL-MONEY arm, applied before
    /// emitting and independent of the runtime's own caps.
    ///
    /// Per-order cap: if the order's notional (limit_price*quantity) exceeds
    /// `max_order_notional_usd`, CLIP the quantity down to the largest size that
    /// fits the cap (drop entirely if even a zero-ish size can't fit / price is
    /// non-positive). Per-market cap: track cumulative submitted notional per
    /// market id; once `max_market_notional_usd` is reached for a market, REFUSE
    /// further br2 orders for that market. Every cap action is logged.
    fn apply_notional_caps(
        &mut self,
        mut intent: OrderIntent,
        market_id: &MarketId,
    ) -> Option<OrderIntent> {
        let price = intent.limit_price;
        if !(price > 0.0) {
            warn!(
                target: "br2_shadow",
                market = %market_id,
                price,
                "BR2-LIVE notional-cap: dropping order with non-positive price"
            );
            return None;
        }

        // Per-market cumulative headroom first: if this market is already at the
        // cap, refuse outright.
        let already = *self
            .submitted_notional_by_market
            .get(market_id.as_str())
            .unwrap_or(&0.0);
        let market_headroom = (self.max_market_notional_usd - already).max(0.0);
        if market_headroom <= 0.0 {
            warn!(
                target: "br2_shadow",
                market = %market_id,
                cumulative_usd = already,
                cap_usd = self.max_market_notional_usd,
                "BR2-LIVE notional-cap: per-market cumulative cap hit; refusing further br2 orders \
                 for this market"
            );
            return None;
        }

        // Per-order cap, then bound by remaining per-market headroom.
        let allowed_notional = self.max_order_notional_usd.min(market_headroom);
        let order_notional = intent.notional_usd();
        if order_notional > allowed_notional {
            let new_qty = (allowed_notional / price).max(0.0);
            // Round down to 2dp (the wire boundary rounds the same way; rounding
            // here keeps our notional accounting consistent with what is sent).
            let new_qty = (new_qty * 100.0).floor() / 100.0;
            if new_qty <= 0.0 {
                warn!(
                    target: "br2_shadow",
                    market = %market_id,
                    order_notional_usd = order_notional,
                    allowed_notional_usd = allowed_notional,
                    "BR2-LIVE notional-cap: order does not fit remaining headroom; dropping"
                );
                return None;
            }
            warn!(
                target: "br2_shadow",
                market = %market_id,
                from_qty = intent.quantity,
                to_qty = new_qty,
                order_notional_usd = order_notional,
                allowed_notional_usd = allowed_notional,
                per_order_cap_usd = self.max_order_notional_usd,
                market_headroom_usd = market_headroom,
                "BR2-LIVE notional-cap: clipping order quantity to fit cap"
            );
            intent.quantity = new_qty;
        }

        let final_notional = intent.notional_usd();
        *self
            .submitted_notional_by_market
            .entry(market_id.as_str().to_string())
            .or_insert(0.0) += final_notional;
        Some(intent)
    }
}

/// Map one br2 [`ShadowOrder`] to a runtime [`OrderIntent`].
///
/// Side/instrument/price mapping (limit prices are in 0..1 probability terms):
///   - BuyYes  -> Buy  YES token, price = shadow.limit_price (YES terms) or yes_ask
///   - SellYes -> Sell YES token, price = shadow.limit_price or yes_bid
///   - BuyNo   -> Buy  NO token,  price = 1 - shadow.limit_price (YES->NO) or no_ask
///   - SellNo  -> Sell NO token,  price = 1 - shadow.limit_price or no_bid
///
/// TAKER semantics: br2 is a taker that sweeps to a limit. When `tag_taker` is
/// true (the REAL-MONEY arm), the intent is tagged
/// `quote_level_tag = "br2-taker"`, which the live submit path
/// (`submit_request_from_intent` in runner.rs) reads to force
/// `TimeInForce::Ioc` and `post_only = false`. The tag matches none of the MM
/// gate prefixes / `MmQuoteKind` substrings, so it changes nothing for other
/// strategies; it only routes br2 to the IOC taker branch. On the paper arm
/// `tag_taker` is false and the tag stays `None` (byte-identical attribution to
/// before this increment); the paper-fill sim crosses immediately regardless of
/// TIF.
///
/// `new_buy`/`new_sell` produce kind=Entry/Close and reduce_only=false/true,
/// matching "Entry for Buy loads, Close for reduce-only Sell".
///
/// Returns `None` (skips the order) when a NO-side order is requested but the
/// market exposes no NO token, or when no usable price can be derived from the
/// ShadowOrder limit or the book fallback.
fn shadow_order_to_intent(
    order: &ShadowOrder,
    market_id: &MarketId,
    yes_token: &InstrumentId,
    no_token: Option<&InstrumentId>,
    yes_book: &BookState,
    no_book: Option<&BookState>,
    now_ms: u64,
    tag_taker: bool,
) -> Option<OrderIntent> {
    let yes_limit = order.limit_price.map(|p| p as f64);
    let no_limit = order.limit_price.map(|p| 1.0 - p as f64);

    let (instrument, side, price) = match order.side {
        Side::BuyYes => (
            yes_token.clone(),
            TradeSide::Buy,
            yes_limit.or(positive(yes_book.best_ask))?,
        ),
        Side::SellYes => (
            yes_token.clone(),
            TradeSide::Sell,
            yes_limit.or(positive(yes_book.best_bid))?,
        ),
        Side::BuyNo => {
            let token = no_token?.clone();
            let fallback = no_book.and_then(|b| positive(b.best_ask));
            (token, TradeSide::Buy, no_limit.or(fallback)?)
        }
        Side::SellNo => {
            let token = no_token?.clone();
            let fallback = no_book.and_then(|b| positive(b.best_bid));
            (token, TradeSide::Sell, no_limit.or(fallback)?)
        }
    };

    let price = price.clamp(0.0, 1.0);
    let reason = format!("br2:{}", order.tag);
    let client_order_id = ClientOrderId::from(format!(
        "br2-paper:{}:{}:{}:{}",
        market_id.as_str(),
        order.tag,
        side_label(order.side),
        now_ms
    ));

    let mut intent = match side {
        TradeSide::Buy => OrderIntent::new_buy(
            client_order_id,
            market_id.clone(),
            instrument,
            price,
            order.shares,
            reason,
            now_ms,
        ),
        TradeSide::Sell => OrderIntent::new_sell(
            client_order_id,
            market_id.clone(),
            instrument,
            price,
            order.shares,
            reason,
            now_ms,
        ),
    };
    if tag_taker {
        intent.quote_level_tag = Some("br2-taker".to_string());
    }
    Some(intent)
}

fn positive(value: f64) -> Option<f64> {
    (value > 0.0).then_some(value)
}

fn side_label(side: Side) -> &'static str {
    match side {
        Side::BuyYes => "buyyes",
        Side::SellYes => "sellyes",
        Side::BuyNo => "buyno",
        Side::SellNo => "sellno",
    }
}

fn env_truthy(name: &str) -> bool {
    std::env::var(name)
        .ok()
        .map(|v| matches!(v.trim().to_ascii_lowercase().as_str(), "true" | "1" | "yes"))
        .unwrap_or(false)
}

/// Parse an env var as a strictly-positive, finite f64. Returns `None` if the
/// var is unset, unparseable, NaN/inf, or <= 0. Used for the notional caps,
/// where "set and > 0" is a hard precondition for the real-money arm.
fn env_positive_f64(name: &str) -> Option<f64> {
    std::env::var(name)
        .ok()
        .and_then(|v| v.trim().parse::<f64>().ok())
        .filter(|v| v.is_finite() && *v > 0.0)
}

/// Parse an env var as a finite f64 strictly inside the open interval (0, 1).
/// Returns `None` if unset, unparseable, NaN/inf, or outside (0, 1). Used for the
/// model-edge override, where an edge is a probability gap in (0, 1).
fn env_unit_interval_f64(name: &str) -> Option<f64> {
    std::env::var(name)
        .ok()
        .and_then(|v| v.trim().parse::<f64>().ok())
        .filter(|v| v.is_finite() && *v > 0.0 && *v < 1.0)
}

/// Parse an env var as a finite, non-negative f64. Returns `None` if unset,
/// unparseable, NaN/inf, or negative. Used for the realized-vol floor override,
/// where 0 is a valid value that fully disables the floor.
fn env_nonneg_f64(name: &str) -> Option<f64> {
    std::env::var(name)
        .ok()
        .and_then(|v| v.trim().parse::<f64>().ok())
        .filter(|v| v.is_finite() && *v >= 0.0)
}

/// Derive a stable u32 the adapter labels decisions with from the runtime's
/// string market id. The adapter only uses this as an opaque label.
fn stable_market_u32(market_id: &MarketId) -> u32 {
    let mut hasher = DefaultHasher::new();
    market_id.as_str().hash(&mut hasher);
    hasher.finish() as u32
}
