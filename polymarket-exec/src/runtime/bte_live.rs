//! Live glue for the BackToExplore taker overlay.
//!
//! OFF by default. When `PM_BTC_5M_BTE_SHADOW=true`, the driver feeds live
//! books/spot into the shared BTE strategy and logs decisions. Paper/live
//! submission require separate arm flags and hard preconditions.

use std::collections::hash_map::DefaultHasher;
use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::path::Path;

use pm_strategy::Side;
use pm_types::TradeHistory;
use tracing::{info, warn};

use crate::book::BookState;
use crate::market_context::MarketContextRecord;
use crate::runtime::bte_shadow::{
    BteDecisionPosition, BteRegimeInputs, BteShadowAdapter, BteShadowOrder,
};
use crate::strategy_profile::StrategyProfile;
use crate::types::{ClientOrderId, InstrumentId, IntentKind, MarketId, OrderIntent, TradeSide};

const NS_PER_MS: i64 = 1_000_000;
const DECISION_CADENCE_MS: u64 = 1_000;
const DEFAULT_MIN_ORDER_SHARES: f64 = 5.0;
const DEFAULT_MIN_ORDER_NOTIONAL_USD: f64 = 1.8;
const LIVE_REVERSAL_REPAIR_MIN_RESIDUAL_SHARES: f64 = 15.0;
const LIVE_REVERSAL_REPAIR_MIN_PRESSURE: f32 = 0.20;
const LIVE_REVERSAL_REPAIR_MIN_WHIPSAW: f32 = 0.52;
const LIVE_REVERSAL_REPAIR_MAX_PATH_EFFICIENCY: f32 = 0.30;
const LIVE_REVERSAL_REPAIR_MIN_SIGN_FLIP_RATE: f32 = 0.30;
const DEFAULT_EV_MAX_GUARANTEED_LOSS_USD: f64 = 1.00;
const DEFAULT_EV_MIN_WORST_IMPROVEMENT_USD: f64 = 0.10;
const DEFAULT_EV_MIN_MARGINAL_EV_USD: f64 = 0.0;
const DEFAULT_WHIPSAW_ENTRY_DELAY_SECS: f64 = 60.0;
const ENTRY_DELAY_LOG_INTERVAL_MS: u64 = 10_000;

struct ActiveMarket {
    market_id: MarketId,
    market_u32: u32,
    yes_asset_id: String,
    close_ms: u64,
    last_decision_ms: u64,
    last_entry_delay_log_ms: u64,
    events_seen: u64,
}

#[derive(Debug, Default)]
pub struct BteTickResult {
    pub orders: Vec<BteShadowOrder>,
    pub submit_intents: Vec<OrderIntent>,
}

struct BteEvContext<'a> {
    yes_book: &'a BookState,
    no_book: Option<&'a BookState>,
    forced_repair: bool,
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct BteTerminalSnapshot {
    yes_qty: f64,
    no_qty: f64,
    total_cost_usd: f64,
    pnl_if_yes: f64,
    pnl_if_no: f64,
    worst_terminal_pnl_usd: f64,
    best_terminal_pnl_usd: f64,
}

impl BteTerminalSnapshot {
    fn from_position(pos: BteDecisionPosition) -> Self {
        let yes_qty = pos.yes_shares.max(0.0);
        let no_qty = pos.no_shares.max(0.0);
        let yes_avg = finite_nonnegative(pos.yes_avg_price);
        let no_avg = finite_nonnegative(pos.no_avg_price);
        Self::new(yes_qty, no_qty, yes_qty * yes_avg + no_qty * no_avg)
    }

    fn after_buy(self, side: Side, quantity: f64, price: f64) -> Self {
        let quantity = finite_nonnegative(quantity);
        let price = finite_nonnegative(price);
        match side {
            Side::BuyYes => Self::new(
                self.yes_qty + quantity,
                self.no_qty,
                self.total_cost_usd + quantity * price,
            ),
            Side::BuyNo => Self::new(
                self.yes_qty,
                self.no_qty + quantity,
                self.total_cost_usd + quantity * price,
            ),
            Side::SellYes | Side::SellNo => self,
        }
    }

    fn guaranteed_loss_usd(self) -> f64 {
        (-self.best_terminal_pnl_usd).max(0.0)
    }

    fn new(yes_qty: f64, no_qty: f64, total_cost_usd: f64) -> Self {
        let pnl_if_yes = yes_qty - total_cost_usd;
        let pnl_if_no = no_qty - total_cost_usd;
        Self {
            yes_qty,
            no_qty,
            total_cost_usd,
            pnl_if_yes,
            pnl_if_no,
            worst_terminal_pnl_usd: pnl_if_yes.min(pnl_if_no),
            best_terminal_pnl_usd: pnl_if_yes.max(pnl_if_no),
        }
    }
}

pub struct BteLiveShadow {
    adapter: BteShadowAdapter,
    active: Option<ActiveMarket>,
    paper_trade_armed: bool,
    live_trade_armed: bool,
    max_order_notional_usd: f64,
    max_market_notional_usd: f64,
    min_order_shares: f64,
    min_order_notional_usd: f64,
    ev_guard_enabled: bool,
    ev_max_guaranteed_loss_usd: f64,
    ev_min_worst_improvement_usd: f64,
    ev_min_marginal_ev_usd: f64,
    whipsaw_entry_delay_secs: f64,
    risk_increasing_notional_by_market: HashMap<String, f64>,
    warmup_skip_warned: bool,
}

impl BteLiveShadow {
    pub fn from_env(
        paper_mode: bool,
        live_kill_switch_path: Option<&Path>,
        profile: Option<&StrategyProfile>,
    ) -> Option<Self> {
        if !env_truthy("PM_BTC_5M_BTE_SHADOW") {
            return None;
        }

        let Some(profile) = profile else {
            warn!(
                target: "bte_shadow",
                "PM_BTC_5M_BTE_SHADOW is set but no strategy profile was loaded; refusing BTE"
            );
            return None;
        };

        let paper_trade_requested = env_truthy("PM_BTC_5M_BTE_PAPER_TRADE");
        let paper_trade_armed = paper_trade_requested && paper_mode;
        if paper_trade_requested && !paper_mode {
            warn!(
                target: "bte_shadow",
                "BTE paper arm requested outside paper mode; refusing paper submission"
            );
        }
        if paper_trade_armed {
            info!(
                target: "bte_shadow",
                "BTE paper-trade armed: BTE orders will enter the paper-fill path"
            );
        }

        let live_trade_requested = env_truthy("PM_BTC_5M_BTE_LIVE_TRADE");
        let kill_switch_configured = live_kill_switch_path
            .map(|p| !p.as_os_str().is_empty())
            .unwrap_or(false);
        let max_order_notional_usd = env_positive_f64("PM_BTC_5M_BTE_MAX_ORDER_NOTIONAL_USD");
        let max_market_notional_usd = env_positive_f64("PM_BTC_5M_BTE_MAX_MARKET_NOTIONAL_USD");
        let min_order_shares =
            env_positive_f64("PM_BTC_5M_BTE_MIN_ORDER_SHARES").unwrap_or(DEFAULT_MIN_ORDER_SHARES);
        let min_order_notional_usd = env_positive_f64("PM_BTC_5M_BTE_MIN_ORDER_NOTIONAL_USD")
            .unwrap_or(DEFAULT_MIN_ORDER_NOTIONAL_USD);
        let ev_guard_enabled = env_bool_default("PM_BTC_5M_BTE_EV_GUARD", true);
        let ev_max_guaranteed_loss_usd =
            env_nonnegative_f64("PM_BTC_5M_BTE_EV_MAX_GUARANTEED_LOSS_USD")
                .unwrap_or(DEFAULT_EV_MAX_GUARANTEED_LOSS_USD);
        let ev_min_worst_improvement_usd =
            env_nonnegative_f64("PM_BTC_5M_BTE_EV_MIN_WORST_IMPROVEMENT_USD")
                .unwrap_or(DEFAULT_EV_MIN_WORST_IMPROVEMENT_USD);
        let ev_min_marginal_ev_usd = env_f64("PM_BTC_5M_BTE_EV_MIN_MARGINAL_EV_USD")
            .unwrap_or(DEFAULT_EV_MIN_MARGINAL_EV_USD);
        let whipsaw_entry_delay_secs =
            env_nonnegative_f64("PM_BTC_5M_BTE_WHIPSAW_ENTRY_DELAY_SECS")
                .unwrap_or(DEFAULT_WHIPSAW_ENTRY_DELAY_SECS);
        let live_preconditions_ok = !paper_mode
            && kill_switch_configured
            && max_order_notional_usd.is_some()
            && max_market_notional_usd.is_some();
        let live_trade_armed = live_trade_requested && live_preconditions_ok;

        if live_trade_requested && !live_trade_armed {
            warn!(
                target: "bte_shadow",
                paper_mode,
                kill_switch_configured,
                max_order_notional_usd = ?max_order_notional_usd,
                max_market_notional_usd = ?max_market_notional_usd,
                "BTE live arm requested but a precondition is missing; refusing real-money submission"
            );
        }
        if live_trade_armed {
            warn!(
                target: "bte_shadow",
                max_order_notional_usd = max_order_notional_usd.unwrap_or(0.0),
                max_market_notional_usd = max_market_notional_usd.unwrap_or(0.0),
                min_order_shares,
                min_order_notional_usd,
                ev_guard_enabled,
                ev_max_guaranteed_loss_usd,
                ev_min_worst_improvement_usd,
                ev_min_marginal_ev_usd,
                whipsaw_entry_delay_secs,
                kill_switch = ?live_kill_switch_path,
                "BTE REAL-MONEY submission ARMED"
            );
        }

        Some(Self {
            adapter: BteShadowAdapter::new(profile.back_to_explore_config()),
            active: None,
            paper_trade_armed,
            live_trade_armed,
            max_order_notional_usd: max_order_notional_usd.unwrap_or(0.0),
            max_market_notional_usd: max_market_notional_usd.unwrap_or(0.0),
            min_order_shares,
            min_order_notional_usd,
            ev_guard_enabled,
            ev_max_guaranteed_loss_usd,
            ev_min_worst_improvement_usd,
            ev_min_marginal_ev_usd,
            whipsaw_entry_delay_secs,
            risk_increasing_notional_by_market: HashMap::new(),
            warmup_skip_warned: false,
        })
    }

    pub fn paper_trade_armed(&self) -> bool {
        self.paper_trade_armed
    }

    pub fn live_trade_armed(&self) -> bool {
        self.live_trade_armed
    }

    pub fn on_spot_trade(
        &mut self,
        price: f64,
        quantity: f64,
        observed_at_ms: u64,
        is_buyer_maker: Option<bool>,
    ) {
        self.adapter
            .on_spot_trade(crate::runtime::br2_shadow::SpotTrade {
                ts_ns: (observed_at_ms as i64).saturating_mul(NS_PER_MS),
                price,
                quantity: quantity as f32,
                is_buyer_maker: is_buyer_maker.unwrap_or(false),
            });
    }

    pub fn decide_tick(
        &mut self,
        market_id: &MarketId,
        record: &MarketContextRecord,
        yes_book: &BookState,
        pos: BteDecisionPosition,
        regime: BteRegimeInputs,
        no_book: Option<&BookState>,
        now_ms: u64,
    ) -> BteTickResult {
        let (Some(open_ms), Some(close_ms)) =
            (record.event_start_time_ms, record.event_end_time_ms)
        else {
            return BteTickResult::default();
        };
        let Some(yes_asset_id) = record.instrument_ids.first() else {
            return BteTickResult::default();
        };
        let no_asset_id = record.instrument_ids.get(1).cloned();

        if let Some(active) = &self.active {
            if now_ms > active.close_ms {
                info!(
                    target: "bte_shadow",
                    market = %active.market_id,
                    market_u32 = active.market_u32,
                    "BTE-SHADOW closed BTC-5m market"
                );
                self.adapter.on_market_close();
                self.active = None;
            }
        }
        if now_ms > close_ms || now_ms < open_ms {
            return BteTickResult::default();
        }

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
            if !self.adapter.on_market_open(market_u32, open_ns, close_ns) {
                if !self.warmup_skip_warned {
                    self.warmup_skip_warned = true;
                    warn!(
                        target: "bte_shadow",
                        market = %market_id,
                        market_u32,
                        open_ms,
                        close_ms,
                        "BTE skipping market: spot tape does not cover open (warmup)"
                    );
                }
                self.active = None;
                return BteTickResult::default();
            }
            self.warmup_skip_warned = false;
            info!(
                target: "bte_shadow",
                market = %market_id,
                market_u32,
                open_ms,
                close_ms,
                yes_asset = %yes_asset_id,
                "BTE-SHADOW opened BTC-5m market"
            );
            self.active = Some(ActiveMarket {
                market_id: market_id.clone(),
                market_u32,
                yes_asset_id: yes_asset_id.clone(),
                close_ms,
                last_decision_ms: 0,
                last_entry_delay_log_ms: 0,
                events_seen: 0,
            });
        }

        let should_decide = match &self.active {
            Some(active) => {
                yes_book.asset_id == active.yes_asset_id
                    && now_ms.saturating_sub(active.last_decision_ms) >= DECISION_CADENCE_MS
            }
            None => false,
        };
        if !should_decide || yes_book.best_bid <= 0.0 || yes_book.best_ask <= 0.0 {
            return BteTickResult::default();
        }

        let market_u32 = self.active.as_ref().map(|a| a.market_u32).unwrap_or(0);
        let tob = crate::runtime::br2_shadow::YesTopOfBook {
            ts_ns: (now_ms as i64).saturating_mul(NS_PER_MS),
            yes_bid: yes_book.best_bid as f32,
            yes_bid_size: yes_book.best_bid_size as f32,
            yes_ask: yes_book.best_ask as f32,
            yes_ask_size: yes_book.best_ask_size as f32,
        };
        let mut pos = pos;
        if let Some(active) = self.active.as_ref() {
            pos.events_seen = active.events_seen;
        }
        let seconds_after_open = now_ms.saturating_sub(open_ms) as f64 / 1_000.0;
        if should_delay_whipsaw_entry(
            pos,
            regime,
            seconds_after_open,
            self.whipsaw_entry_delay_secs,
        ) {
            if let Some(active) = self.active.as_mut() {
                active.last_decision_ms = now_ms;
                if now_ms.saturating_sub(active.last_entry_delay_log_ms)
                    >= ENTRY_DELAY_LOG_INTERVAL_MS
                {
                    active.last_entry_delay_log_ms = now_ms;
                    warn!(
                        target: "bte_shadow",
                        market = %market_id,
                        market_u32,
                        seconds_after_open,
                        delay_secs = self.whipsaw_entry_delay_secs,
                        whipsaw_score = regime.whipsaw_score,
                        path_efficiency = regime.path_efficiency,
                        reversal_pressure = regime.reversal_pressure,
                        sign_flip_rate = regime.sign_flip_rate,
                        realized_vol_180s_bps = regime.realized_vol_180s_bps,
                        "BTE-LIVE delaying flat entry in whipsaw regime"
                    );
                }
            }
            return BteTickResult::default();
        }
        let orders = match self.adapter.build_live_event(&tob) {
            Some(event) => {
                self.adapter
                    .on_decision_event(&event, pos, regime, &TradeHistory::default())
            }
            None => Vec::new(),
        };
        if let Some(active) = self.active.as_mut() {
            active.last_decision_ms = now_ms;
            active.events_seen = active.events_seen.saturating_add(1);
        }
        for order in &orders {
            info!(
                target: "bte_shadow",
                market = %market_id,
                market_u32,
                "BTE-SHADOW {}",
                order.log_line()
            );
        }

        let submit_intents = if self.paper_trade_armed || self.live_trade_armed {
            let yes_token = InstrumentId::from(yes_asset_id.as_str());
            let no_token = no_asset_id
                .as_ref()
                .map(|id| InstrumentId::from(id.as_str()));
            let tag_taker = self.live_trade_armed;
            let mut intents = Vec::new();
            for order in &orders {
                let (order, forced_repair) = live_reversal_repair_order(order, pos, regime);
                if forced_repair {
                    warn!(
                        target: "bte_shadow",
                        market = %market_id,
                        market_u32 = order.market_id,
                        side = ?order.side,
                        residual_shares = pos.current_market_net_exposure_shares,
                        yes_shares = pos.yes_shares,
                        no_shares = pos.no_shares,
                        whipsaw_score = regime.whipsaw_score,
                        path_efficiency = regime.path_efficiency,
                        reversal_pressure = regime.reversal_pressure,
                        sign_flip_rate = regime.sign_flip_rate,
                        realized_vol_180s_bps = regime.realized_vol_180s_bps,
                        "BTE-LIVE forcing opposite-side repair in reversal regime"
                    );
                }
                let Some(intent) = shadow_order_to_intent(
                    &order,
                    market_id,
                    &yes_token,
                    no_token.as_ref(),
                    yes_book,
                    no_book,
                    now_ms,
                    tag_taker,
                ) else {
                    warn!(
                        target: "bte_shadow",
                        market = %market_id,
                        market_u32 = order.market_id,
                        side = ?order.side,
                        has_no_token = no_token.is_some(),
                        has_no_book = no_book.is_some(),
                        "BTE-LIVE dropped shadow order during live-intent conversion"
                    );
                    continue;
                };
                let intent = mark_forced_repair_intent(intent, forced_repair);
                let intent = if self.live_trade_armed {
                    match self.apply_notional_caps(
                        intent,
                        market_id,
                        order.side,
                        pos,
                        Some(BteEvContext {
                            yes_book,
                            no_book,
                            forced_repair,
                        }),
                    ) {
                        Some(capped) => capped,
                        None => continue,
                    }
                } else {
                    intent
                };
                info!(
                    target: "bte_shadow",
                    market = %market_id,
                    client_order_id = %intent.client_order_id,
                    side = ?intent.side,
                    price = intent.limit_price,
                    quantity = intent.quantity,
                    notional_usd = intent.notional_usd(),
                    "BTE-LIVE accepted live intent"
                );
                intents.push(intent);
            }
            intents
        } else {
            Vec::new()
        };

        BteTickResult {
            orders,
            submit_intents,
        }
    }

    pub fn clear_pending_market_cap_reservation(&mut self, market_id: &MarketId) {
        self.risk_increasing_notional_by_market
            .remove(market_id.as_str());
    }

    fn apply_notional_caps(
        &mut self,
        mut intent: OrderIntent,
        market_id: &MarketId,
        order_side: Side,
        pos: BteDecisionPosition,
        ev_context: Option<BteEvContext<'_>>,
    ) -> Option<OrderIntent> {
        let price = intent.limit_price;
        if price <= 0.0 {
            return None;
        }
        let risk_increasing = order_increases_current_market_residual(order_side, pos);
        let already = *self
            .risk_increasing_notional_by_market
            .get(market_id.as_str())
            .unwrap_or(&0.0);
        let current_residual_notional =
            pos.current_market_net_exposure_shares.abs().max(0.0) * price;
        let counted_risk_notional = already.max(current_residual_notional);
        let market_headroom = if risk_increasing {
            (self.max_market_notional_usd - counted_risk_notional).max(0.0)
        } else {
            self.max_market_notional_usd
                .max(self.max_order_notional_usd)
        };
        if market_headroom <= 0.0 {
            warn!(
                target: "bte_shadow",
                market = %market_id,
                risk_increasing_usd = counted_risk_notional,
                cap_usd = self.max_market_notional_usd,
                "BTE-LIVE notional-cap: per-market directional cap hit"
            );
            return None;
        }
        let allowed_notional = self.max_order_notional_usd.min(market_headroom);
        let min_qty_for_notional = self.min_order_notional_usd / price;
        let min_qty = round_qty_up(self.min_order_shares.max(min_qty_for_notional));
        if min_qty <= 0.0 || !min_qty.is_finite() {
            return None;
        }
        let min_notional = min_qty * price;
        if min_notional > allowed_notional {
            warn!(
                target: "bte_shadow",
                market = %market_id,
                min_qty,
                min_notional_usd = min_notional,
                allowed_notional_usd = allowed_notional,
                "BTE-LIVE notional-cap: configured floor does not fit remaining cap; dropping"
            );
            return None;
        }

        if intent.quantity < min_qty {
            warn!(
                target: "bte_shadow",
                market = %market_id,
                from_qty = intent.quantity,
                to_qty = min_qty,
                min_order_shares = self.min_order_shares,
                min_order_notional_usd = self.min_order_notional_usd,
                "BTE-LIVE notional-floor: raising order quantity"
            );
            intent.quantity = min_qty;
        }

        if intent.notional_usd() > allowed_notional {
            let new_qty = round_qty_down((allowed_notional / price).max(0.0));
            if new_qty <= 0.0 {
                return None;
            }
            if new_qty < min_qty {
                warn!(
                    target: "bte_shadow",
                    market = %market_id,
                    clipped_qty = new_qty,
                    min_qty,
                    allowed_notional_usd = allowed_notional,
                    "BTE-LIVE notional-cap: clipped quantity would violate floor; dropping"
                );
                return None;
            }
            warn!(
                target: "bte_shadow",
                market = %market_id,
                from_qty = intent.quantity,
                to_qty = new_qty,
                "BTE-LIVE notional-cap: clipping order quantity"
            );
            intent.quantity = new_qty;
        }
        if let Some(ev_context) = ev_context {
            self.log_and_check_ev_guard(&intent, market_id, order_side, pos, ev_context)?;
        }
        if risk_increasing {
            *self
                .risk_increasing_notional_by_market
                .entry(market_id.as_str().to_string())
                .or_insert(0.0) += intent.notional_usd();
        }
        Some(intent)
    }

    fn log_and_check_ev_guard(
        &self,
        intent: &OrderIntent,
        market_id: &MarketId,
        order_side: Side,
        pos: BteDecisionPosition,
        ev_context: BteEvContext<'_>,
    ) -> Option<()> {
        let before = BteTerminalSnapshot::from_position(pos);
        let after = before.after_buy(order_side, intent.quantity, intent.limit_price);
        let p_yes = book_probability_yes(ev_context.yes_book, ev_context.no_book);
        let marginal_ev_usd = p_yes.and_then(|prob| {
            marginal_buy_ev_usd(order_side, intent.quantity, intent.limit_price, prob)
        });
        let worst_improvement_usd = after.worst_terminal_pnl_usd - before.worst_terminal_pnl_usd;
        let best_change_usd = after.best_terminal_pnl_usd - before.best_terminal_pnl_usd;
        let risk_increasing = order_increases_current_market_residual(order_side, pos);
        let repair_like = !risk_increasing && pos.current_market_net_exposure_shares.abs() > 1e-9;
        let guaranteed_loss_after_usd = after.guaranteed_loss_usd();

        info!(
            target: "bte_ev",
            market = %market_id,
            client_order_id = %intent.client_order_id,
            side = ?order_side,
            price = intent.limit_price,
            quantity = intent.quantity,
            yes_qty_before = before.yes_qty,
            no_qty_before = before.no_qty,
            total_cost_before_usd = before.total_cost_usd,
            pnl_if_yes_before_usd = before.pnl_if_yes,
            pnl_if_no_before_usd = before.pnl_if_no,
            yes_qty_after = after.yes_qty,
            no_qty_after = after.no_qty,
            total_cost_after_usd = after.total_cost_usd,
            pnl_if_yes_after_usd = after.pnl_if_yes,
            pnl_if_no_after_usd = after.pnl_if_no,
            worst_terminal_pnl_after_usd = after.worst_terminal_pnl_usd,
            best_terminal_pnl_after_usd = after.best_terminal_pnl_usd,
            guaranteed_loss_after_usd,
            p_yes = p_yes,
            marginal_ev_usd = marginal_ev_usd,
            risk_increasing,
            repair_like,
            forced_repair = ev_context.forced_repair,
            worst_improvement_usd,
            best_change_usd,
            real_pair_taker_cost = real_pair_taker_cost(ev_context.yes_book, ev_context.no_book),
            "BTE-EV candidate terminal accounting"
        );

        if !self.ev_guard_enabled {
            return Some(());
        }

        let guaranteed_loss_too_large = guaranteed_loss_after_usd > self.ev_max_guaranteed_loss_usd;
        let improves_worst_enough = worst_improvement_usd >= self.ev_min_worst_improvement_usd;
        let marginal_ev_ok = marginal_ev_usd
            .map(|ev| ev >= self.ev_min_marginal_ev_usd)
            .unwrap_or(false);

        let allowed = if !guaranteed_loss_too_large {
            true
        } else if repair_like || ev_context.forced_repair {
            improves_worst_enough || marginal_ev_ok
        } else {
            marginal_ev_ok && worst_improvement_usd >= -self.ev_min_worst_improvement_usd
        };

        if allowed {
            return Some(());
        }

        warn!(
            target: "bte_ev",
            market = %market_id,
            client_order_id = %intent.client_order_id,
            side = ?order_side,
            price = intent.limit_price,
            quantity = intent.quantity,
            guaranteed_loss_after_usd,
            max_guaranteed_loss_usd = self.ev_max_guaranteed_loss_usd,
            marginal_ev_usd = marginal_ev_usd,
            min_marginal_ev_usd = self.ev_min_marginal_ev_usd,
            worst_improvement_usd,
            min_worst_improvement_usd = self.ev_min_worst_improvement_usd,
            risk_increasing,
            repair_like,
            forced_repair = ev_context.forced_repair,
            pnl_if_yes_after_usd = after.pnl_if_yes,
            pnl_if_no_after_usd = after.pnl_if_no,
            "BTE-EV guard suppressed terminal-negative candidate"
        );
        None
    }
}

fn book_probability_yes(yes_book: &BookState, no_book: Option<&BookState>) -> Option<f64> {
    let yes_mid = valid_mid(yes_book.best_bid, yes_book.best_ask)?;
    let Some(no_book) = no_book else {
        return Some(yes_mid);
    };
    let no_mid = valid_mid(no_book.best_bid, no_book.best_ask)?;
    let denom = yes_mid + no_mid;
    (denom > 0.0 && denom.is_finite()).then_some((yes_mid / denom).clamp(0.0, 1.0))
}

fn marginal_buy_ev_usd(side: Side, quantity: f64, price: f64, p_yes: f64) -> Option<f64> {
    let quantity = finite_nonnegative(quantity);
    let price = finite_nonnegative(price);
    let p_yes = p_yes.clamp(0.0, 1.0);
    if quantity <= 0.0 || price <= 0.0 {
        return None;
    }
    match side {
        Side::BuyYes => Some(quantity * (p_yes - price)),
        Side::BuyNo => Some(quantity * ((1.0 - p_yes) - price)),
        Side::SellYes | Side::SellNo => None,
    }
}

fn real_pair_taker_cost(yes_book: &BookState, no_book: Option<&BookState>) -> Option<f64> {
    let no_book = no_book?;
    (yes_book.best_ask.is_finite()
        && no_book.best_ask.is_finite()
        && yes_book.best_ask > 0.0
        && no_book.best_ask > 0.0)
        .then_some(yes_book.best_ask + no_book.best_ask)
}

fn valid_mid(bid: f64, ask: f64) -> Option<f64> {
    (bid.is_finite() && ask.is_finite() && bid > 0.0 && ask > 0.0 && bid <= ask)
        .then_some(0.5 * (bid + ask))
}

fn finite_nonnegative(value: f64) -> f64 {
    if value.is_finite() && value > 0.0 {
        value
    } else {
        0.0
    }
}

fn order_increases_current_market_residual(side: Side, pos: BteDecisionPosition) -> bool {
    let residual = pos.current_market_net_exposure_shares;
    match side {
        Side::BuyYes => residual >= 0.0,
        Side::BuyNo => residual <= 0.0,
        Side::SellYes | Side::SellNo => false,
    }
}

fn live_reversal_repair_order(
    order: &BteShadowOrder,
    pos: BteDecisionPosition,
    regime: BteRegimeInputs,
) -> (BteShadowOrder, bool) {
    if !matches!(order.side, Side::BuyYes | Side::BuyNo) {
        return (order.clone(), false);
    }
    let residual = pos.current_market_net_exposure_shares;
    if residual.abs() < LIVE_REVERSAL_REPAIR_MIN_RESIDUAL_SHARES {
        return (order.clone(), false);
    }
    if !order_increases_current_market_residual(order.side, pos) {
        return (order.clone(), false);
    }
    if !live_reversal_repair_regime(regime) {
        return (order.clone(), false);
    }

    let mut repair = order.clone();
    repair.side = if residual > 0.0 {
        Side::BuyNo
    } else {
        Side::BuyYes
    };
    repair.tag = "back_to_explore_live_reversal_repair";
    (repair, true)
}

fn live_reversal_repair_regime(regime: BteRegimeInputs) -> bool {
    if regime.reversal_pressure >= LIVE_REVERSAL_REPAIR_MIN_PRESSURE
        && regime.path_efficiency <= LIVE_REVERSAL_REPAIR_MAX_PATH_EFFICIENCY
    {
        return true;
    }
    regime.whipsaw_score >= LIVE_REVERSAL_REPAIR_MIN_WHIPSAW
        && regime.sign_flip_rate >= LIVE_REVERSAL_REPAIR_MIN_SIGN_FLIP_RATE
        && regime.path_efficiency <= LIVE_REVERSAL_REPAIR_MAX_PATH_EFFICIENCY
}

fn should_delay_whipsaw_entry(
    pos: BteDecisionPosition,
    regime: BteRegimeInputs,
    seconds_after_open: f64,
    delay_secs: f64,
) -> bool {
    if delay_secs <= 0.0 || seconds_after_open >= delay_secs {
        return false;
    }
    if pos.yes_shares.max(0.0) + pos.no_shares.max(0.0) > 1e-9 {
        return false;
    }
    bte_whipsaw_entry_delay_regime(regime)
}

fn bte_whipsaw_entry_delay_regime(regime: BteRegimeInputs) -> bool {
    regime.whipsaw_score >= 0.48
        || (regime.path_efficiency <= 0.30 && regime.sign_flip_rate >= 0.30)
        || (regime.reversal_pressure >= 0.20 && regime.path_efficiency <= 0.40)
}

fn mark_forced_repair_intent(mut intent: OrderIntent, forced_repair: bool) -> OrderIntent {
    if forced_repair {
        intent.kind = IntentKind::Close;
    }
    intent
}

fn shadow_order_to_intent(
    order: &BteShadowOrder,
    market_id: &MarketId,
    yes_token: &InstrumentId,
    no_token: Option<&InstrumentId>,
    yes_book: &BookState,
    no_book: Option<&BookState>,
    now_ms: u64,
    tag_taker: bool,
) -> Option<OrderIntent> {
    let mut intent = match order.side {
        Side::BuyYes => OrderIntent::new_buy(
            ClientOrderId::new(format!("bte-{}-{now_ms}-buy-yes", order.market_id)),
            market_id.clone(),
            yes_token.clone(),
            live_price(yes_book.best_ask)?,
            order.shares,
            "bte taker buy yes",
            now_ms,
        ),
        Side::SellYes => OrderIntent::new_sell(
            ClientOrderId::new(format!("bte-{}-{now_ms}-sell-yes", order.market_id)),
            market_id.clone(),
            yes_token.clone(),
            live_price(yes_book.best_bid)?,
            order.shares,
            "bte taker sell yes",
            now_ms,
        ),
        Side::BuyNo => {
            let no_token = no_token?.clone();
            let no_px = live_price(no_book?.best_ask)?;
            OrderIntent::new_buy(
                ClientOrderId::new(format!("bte-{}-{now_ms}-buy-no", order.market_id)),
                market_id.clone(),
                no_token,
                no_px,
                order.shares,
                "bte taker buy no",
                now_ms,
            )
        }
        Side::SellNo => {
            let no_token = no_token?.clone();
            let no_px = live_price(no_book?.best_bid)?;
            OrderIntent::new_sell(
                ClientOrderId::new(format!("bte-{}-{now_ms}-sell-no", order.market_id)),
                market_id.clone(),
                no_token,
                no_px,
                order.shares,
                "bte taker sell no",
                now_ms,
            )
        }
    };
    if tag_taker {
        intent.quote_level_tag = Some(format!("bte-taker:{}", order.tag));
    }
    if intent.limit_price <= 0.0 || intent.quantity <= 0.0 {
        return None;
    }
    if matches!(intent.side, TradeSide::Buy) {
        intent.reduce_only = false;
    }
    Some(intent)
}

fn live_price(price: f64) -> Option<f64> {
    (price.is_finite() && price > 0.0 && price <= 1.0).then_some(price)
}

fn round_qty_down(quantity: f64) -> f64 {
    (quantity * 100.0).floor() / 100.0
}

fn round_qty_up(quantity: f64) -> f64 {
    ((quantity * 100.0) - 1e-9).ceil() / 100.0
}

fn stable_market_u32(market_id: &MarketId) -> u32 {
    let mut hasher = DefaultHasher::new();
    market_id.as_str().hash(&mut hasher);
    (hasher.finish() & 0xffff_ffff) as u32
}

fn env_truthy(key: &str) -> bool {
    std::env::var(key)
        .ok()
        .map(|value| {
            matches!(
                value.trim().to_ascii_lowercase().as_str(),
                "1" | "true" | "yes" | "on"
            )
        })
        .unwrap_or(false)
}

fn env_bool_default(key: &str, default: bool) -> bool {
    std::env::var(key)
        .ok()
        .map(|value| match value.trim().to_ascii_lowercase().as_str() {
            "1" | "true" | "yes" | "on" => true,
            "0" | "false" | "no" | "off" => false,
            _ => default,
        })
        .unwrap_or(default)
}

fn env_f64(key: &str) -> Option<f64> {
    std::env::var(key)
        .ok()
        .and_then(|value| value.trim().parse::<f64>().ok())
        .filter(|value| value.is_finite())
}

fn env_positive_f64(key: &str) -> Option<f64> {
    env_f64(key).filter(|value| *value > 0.0)
}

fn env_nonnegative_f64(key: &str) -> Option<f64> {
    env_f64(key).filter(|value| *value >= 0.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn book(asset_id: &str, bid: f64, ask: f64) -> BookState {
        BookState::from_top_of_book(asset_id, bid, 100.0, ask, 100.0, 0.5 * (bid + ask), 1_000)
    }

    fn order(side: Side, shares: f64) -> BteShadowOrder {
        BteShadowOrder {
            market_id: 42,
            side,
            shares,
            max_depth: 1,
            limit_price: Some(0.982),
            tag: "test",
        }
    }

    fn live_fixture() -> BteLiveShadow {
        BteLiveShadow {
            adapter: BteShadowAdapter::new(pm_strategy::BackToExploreConfig::default()),
            active: None,
            paper_trade_armed: false,
            live_trade_armed: true,
            max_order_notional_usd: 50.0,
            max_market_notional_usd: 500.0,
            min_order_shares: 5.0,
            min_order_notional_usd: 1.8,
            ev_guard_enabled: true,
            ev_max_guaranteed_loss_usd: DEFAULT_EV_MAX_GUARANTEED_LOSS_USD,
            ev_min_worst_improvement_usd: DEFAULT_EV_MIN_WORST_IMPROVEMENT_USD,
            ev_min_marginal_ev_usd: DEFAULT_EV_MIN_MARGINAL_EV_USD,
            whipsaw_entry_delay_secs: DEFAULT_WHIPSAW_ENTRY_DELAY_SECS,
            risk_increasing_notional_by_market: HashMap::new(),
            warmup_skip_warned: false,
        }
    }

    fn ev_context<'a>(yes_book: &'a BookState, no_book: &'a BookState) -> BteEvContext<'a> {
        BteEvContext {
            yes_book,
            no_book: Some(no_book),
            forced_repair: false,
        }
    }

    #[test]
    fn bte_buy_no_uses_live_no_ask_not_inverted_shadow_limit() {
        let market = MarketId::from("m");
        let yes = InstrumentId::from("yes");
        let no = InstrumentId::from("no");
        let yes_book = book("yes", 0.35, 0.37);
        let no_book = book("no", 0.63, 0.65);

        let intent = shadow_order_to_intent(
            &order(Side::BuyNo, 5.0),
            &market,
            &yes,
            Some(&no),
            &yes_book,
            Some(&no_book),
            123,
            true,
        )
        .expect("valid NO book should produce intent");

        assert_eq!(intent.instrument_id.as_str(), "no");
        assert_eq!(intent.side, TradeSide::Buy);
        assert!((intent.limit_price - 0.65).abs() < 1e-9);
        assert_eq!(intent.quote_level_tag.as_deref(), Some("bte-taker:test"));
    }

    #[test]
    fn bte_terminal_snapshot_matches_june_8_negative_shape() {
        let snapshot = BteTerminalSnapshot::new(61.2, 71.831, 76.6675);

        assert!((snapshot.pnl_if_yes + 15.4675).abs() < 1e-9);
        assert!((snapshot.pnl_if_no + 4.8365).abs() < 1e-9);
        assert!((snapshot.worst_terminal_pnl_usd + 15.4675).abs() < 1e-9);
        assert!((snapshot.best_terminal_pnl_usd + 4.8365).abs() < 1e-9);
        assert!((snapshot.guaranteed_loss_usd() - 4.8365).abs() < 1e-9);
    }

    #[test]
    fn bte_ev_guard_blocks_risk_add_that_deepens_guaranteed_loss() {
        let mut live = live_fixture();
        let market = MarketId::from("m");
        let yes_book = book("yes", 0.10, 0.12);
        let no_book = book("no", 0.88, 0.90);
        let pos = BteDecisionPosition {
            yes_shares: 20.0,
            no_shares: 25.0,
            yes_avg_price: 0.65,
            no_avg_price: 0.75,
            current_market_net_exposure_shares: -5.0,
            ..BteDecisionPosition::default()
        };
        let add_no = OrderIntent::new_buy(
            ClientOrderId::from("add-no"),
            market.clone(),
            InstrumentId::from("no"),
            0.90,
            5.0,
            "test",
            1_000,
        );

        assert!(
            live.apply_notional_caps(
                add_no,
                &market,
                Side::BuyNo,
                pos,
                Some(ev_context(&yes_book, &no_book)),
            )
            .is_none(),
            "risk-increasing NO buy should be blocked once it deepens a guaranteed terminal loss"
        );
    }

    #[test]
    fn bte_ev_guard_allows_repair_that_improves_worst_terminal_outcome() {
        let mut live = live_fixture();
        live.ev_max_guaranteed_loss_usd = 0.10;
        let market = MarketId::from("m");
        let yes_book = book("yes", 0.38, 0.40);
        let no_book = book("no", 0.58, 0.60);
        let pos = BteDecisionPosition {
            yes_shares: 40.0,
            no_shares: 0.0,
            yes_avg_price: 0.95,
            no_avg_price: 0.0,
            current_market_net_exposure_shares: 40.0,
            ..BteDecisionPosition::default()
        };
        let repair_no = OrderIntent::new_buy(
            ClientOrderId::from("repair-no"),
            market.clone(),
            InstrumentId::from("no"),
            0.60,
            5.0,
            "test",
            1_000,
        );

        assert!(
            live.apply_notional_caps(
                repair_no,
                &market,
                Side::BuyNo,
                pos,
                Some(ev_context(&yes_book, &no_book)),
            )
            .is_some(),
            "opposite-side repair should remain available when it improves worst terminal PnL"
        );
    }

    #[test]
    fn bte_live_notional_floor_raises_tiny_order_within_cap() {
        let mut live = BteLiveShadow {
            adapter: BteShadowAdapter::new(pm_strategy::BackToExploreConfig::default()),
            active: None,
            paper_trade_armed: false,
            live_trade_armed: true,
            max_order_notional_usd: 5.0,
            max_market_notional_usd: 50.0,
            min_order_shares: 5.0,
            min_order_notional_usd: 1.8,
            ev_guard_enabled: true,
            ev_max_guaranteed_loss_usd: DEFAULT_EV_MAX_GUARANTEED_LOSS_USD,
            ev_min_worst_improvement_usd: DEFAULT_EV_MIN_WORST_IMPROVEMENT_USD,
            ev_min_marginal_ev_usd: DEFAULT_EV_MIN_MARGINAL_EV_USD,
            whipsaw_entry_delay_secs: DEFAULT_WHIPSAW_ENTRY_DELAY_SECS,
            risk_increasing_notional_by_market: HashMap::new(),
            warmup_skip_warned: false,
        };
        let market = MarketId::from("m");
        let intent = OrderIntent::new_buy(
            ClientOrderId::from("c"),
            market.clone(),
            InstrumentId::from("no"),
            0.12,
            3.0,
            "test",
            1_000,
        );

        let capped = live
            .apply_notional_caps(
                intent,
                &market,
                Side::BuyNo,
                BteDecisionPosition {
                    current_market_net_exposure_shares: 0.0,
                    ..BteDecisionPosition::default()
                },
                None,
            )
            .expect("floor fits within $5 cap");

        assert!((capped.quantity - 15.0).abs() < 1e-9);
        assert!((capped.notional_usd() - 1.8).abs() < 1e-9);
    }

    #[test]
    fn bte_live_notional_floor_drops_when_floor_exceeds_cap() {
        let mut live = BteLiveShadow {
            adapter: BteShadowAdapter::new(pm_strategy::BackToExploreConfig::default()),
            active: None,
            paper_trade_armed: false,
            live_trade_armed: true,
            max_order_notional_usd: 3.0,
            max_market_notional_usd: 50.0,
            min_order_shares: 5.0,
            min_order_notional_usd: 1.8,
            ev_guard_enabled: true,
            ev_max_guaranteed_loss_usd: DEFAULT_EV_MAX_GUARANTEED_LOSS_USD,
            ev_min_worst_improvement_usd: DEFAULT_EV_MIN_WORST_IMPROVEMENT_USD,
            ev_min_marginal_ev_usd: DEFAULT_EV_MIN_MARGINAL_EV_USD,
            whipsaw_entry_delay_secs: DEFAULT_WHIPSAW_ENTRY_DELAY_SECS,
            risk_increasing_notional_by_market: HashMap::new(),
            warmup_skip_warned: false,
        };
        let market = MarketId::from("m");
        let intent = OrderIntent::new_buy(
            ClientOrderId::from("c"),
            market.clone(),
            InstrumentId::from("yes"),
            0.98,
            1.0,
            "test",
            1_000,
        );

        assert!(live
            .apply_notional_caps(
                intent,
                &market,
                Side::BuyYes,
                BteDecisionPosition {
                    current_market_net_exposure_shares: 0.0,
                    ..BteDecisionPosition::default()
                },
                None,
            )
            .is_none());
    }

    #[test]
    fn bte_live_market_cap_blocks_same_side_risk_but_allows_repair() {
        let mut live = BteLiveShadow {
            adapter: BteShadowAdapter::new(pm_strategy::BackToExploreConfig::default()),
            active: None,
            paper_trade_armed: false,
            live_trade_armed: true,
            max_order_notional_usd: 15.0,
            max_market_notional_usd: 50.0,
            min_order_shares: 5.0,
            min_order_notional_usd: 1.8,
            ev_guard_enabled: true,
            ev_max_guaranteed_loss_usd: DEFAULT_EV_MAX_GUARANTEED_LOSS_USD,
            ev_min_worst_improvement_usd: DEFAULT_EV_MIN_WORST_IMPROVEMENT_USD,
            ev_min_marginal_ev_usd: DEFAULT_EV_MIN_MARGINAL_EV_USD,
            whipsaw_entry_delay_secs: DEFAULT_WHIPSAW_ENTRY_DELAY_SECS,
            risk_increasing_notional_by_market: HashMap::new(),
            warmup_skip_warned: false,
        };
        let market = MarketId::from("m");
        let add_yes = OrderIntent::new_buy(
            ClientOrderId::from("add-yes"),
            market.clone(),
            InstrumentId::from("yes"),
            0.60,
            10.0,
            "test",
            1_000,
        );
        let repair_no = OrderIntent::new_buy(
            ClientOrderId::from("repair-no"),
            market.clone(),
            InstrumentId::from("no"),
            0.40,
            10.0,
            "test",
            1_000,
        );
        let long_yes_at_cap = BteDecisionPosition {
            current_market_net_exposure_shares: 90.0,
            ..BteDecisionPosition::default()
        };

        assert!(
            live.apply_notional_caps(add_yes, &market, Side::BuyYes, long_yes_at_cap, None)
                .is_none(),
            "same-side add should be blocked once current residual exceeds the market cap"
        );

        let capped = live
            .apply_notional_caps(repair_no, &market, Side::BuyNo, long_yes_at_cap, None)
            .expect("opposite-side repair should not be blocked by the market cap");
        assert!((capped.notional_usd() - 4.0).abs() < 1e-9);
    }

    #[test]
    fn bte_live_can_release_router_suppressed_cap_reservation() {
        let mut live = BteLiveShadow {
            adapter: BteShadowAdapter::new(pm_strategy::BackToExploreConfig::default()),
            active: None,
            paper_trade_armed: false,
            live_trade_armed: true,
            max_order_notional_usd: 5.0,
            max_market_notional_usd: 6.0,
            min_order_shares: 5.0,
            min_order_notional_usd: 1.8,
            ev_guard_enabled: true,
            ev_max_guaranteed_loss_usd: DEFAULT_EV_MAX_GUARANTEED_LOSS_USD,
            ev_min_worst_improvement_usd: DEFAULT_EV_MIN_WORST_IMPROVEMENT_USD,
            ev_min_marginal_ev_usd: DEFAULT_EV_MIN_MARGINAL_EV_USD,
            whipsaw_entry_delay_secs: DEFAULT_WHIPSAW_ENTRY_DELAY_SECS,
            risk_increasing_notional_by_market: HashMap::new(),
            warmup_skip_warned: false,
        };
        let market = MarketId::from("m");
        let pos = BteDecisionPosition {
            current_market_net_exposure_shares: 0.0,
            ..BteDecisionPosition::default()
        };
        let first = OrderIntent::new_buy(
            ClientOrderId::from("first"),
            market.clone(),
            InstrumentId::from("yes"),
            0.80,
            5.0,
            "test",
            1_000,
        );
        let second = OrderIntent::new_buy(
            ClientOrderId::from("second"),
            market.clone(),
            InstrumentId::from("yes"),
            0.80,
            5.0,
            "test",
            2_000,
        );

        assert!(live
            .apply_notional_caps(first, &market, Side::BuyYes, pos, None)
            .is_some());
        assert!(
            live.apply_notional_caps(second.clone(), &market, Side::BuyYes, pos, None)
                .is_none(),
            "the first accepted intent reserves live cap until submitted or explicitly released"
        );

        live.clear_pending_market_cap_reservation(&market);
        assert!(
            live.apply_notional_caps(second, &market, Side::BuyYes, pos, None)
                .is_some(),
            "router-suppressed intents should not poison the next real opportunity"
        );
    }

    #[test]
    fn bte_live_reversal_regime_forces_opposite_side_repair() {
        let long_yes = BteDecisionPosition {
            yes_shares: 45.0,
            no_shares: 5.0,
            current_market_net_exposure_shares: 40.0,
            ..BteDecisionPosition::default()
        };
        let regime = BteRegimeInputs {
            whipsaw_score: 0.56,
            path_efficiency: 0.18,
            reversal_pressure: 0.32,
            sign_flip_rate: 0.34,
            realized_vol_180s_bps: 8.8,
        };

        let (repair, forced) =
            live_reversal_repair_order(&order(Side::BuyYes, 5.0), long_yes, regime);

        assert!(forced);
        assert_eq!(repair.side, Side::BuyNo);
        assert_eq!(repair.tag, "back_to_explore_live_reversal_repair");
    }

    #[test]
    fn bte_live_forced_repair_intent_is_close_kind() {
        let intent = OrderIntent::new_buy(
            ClientOrderId::from("repair"),
            MarketId::from("m"),
            InstrumentId::from("no"),
            0.44,
            5.0,
            "forced repair",
            1_000,
        );

        let repaired = mark_forced_repair_intent(intent, true);

        assert_eq!(repaired.kind, IntentKind::Close);
        assert!(!repaired.reduce_only);
    }

    #[test]
    fn bte_live_clean_path_keeps_same_side_order() {
        let long_yes = BteDecisionPosition {
            yes_shares: 45.0,
            no_shares: 5.0,
            current_market_net_exposure_shares: 40.0,
            ..BteDecisionPosition::default()
        };
        let regime = BteRegimeInputs {
            whipsaw_score: 0.20,
            path_efficiency: 0.80,
            reversal_pressure: 0.0,
            sign_flip_rate: 0.12,
            realized_vol_180s_bps: 5.0,
        };

        let (same, forced) =
            live_reversal_repair_order(&order(Side::BuyYes, 5.0), long_yes, regime);

        assert!(!forced);
        assert_eq!(same.side, Side::BuyYes);
        assert_eq!(same.tag, "test");
    }

    #[test]
    fn bte_whipsaw_entry_delay_blocks_flat_early_stress() {
        let regime = BteRegimeInputs {
            whipsaw_score: 0.51,
            path_efficiency: 0.28,
            reversal_pressure: 0.24,
            sign_flip_rate: 0.35,
            realized_vol_180s_bps: 10.0,
        };

        assert!(should_delay_whipsaw_entry(
            BteDecisionPosition::default(),
            regime,
            30.0,
            60.0,
        ));
    }

    #[test]
    fn bte_whipsaw_entry_delay_allows_existing_inventory_repair_window() {
        let regime = BteRegimeInputs {
            whipsaw_score: 0.51,
            path_efficiency: 0.28,
            reversal_pressure: 0.24,
            sign_flip_rate: 0.35,
            realized_vol_180s_bps: 10.0,
        };
        let pos = BteDecisionPosition {
            yes_shares: 10.0,
            current_market_net_exposure_shares: 10.0,
            ..BteDecisionPosition::default()
        };

        assert!(!should_delay_whipsaw_entry(pos, regime, 30.0, 60.0));
    }

    #[test]
    fn bte_whipsaw_entry_delay_expires_after_delay() {
        let regime = BteRegimeInputs {
            whipsaw_score: 0.51,
            path_efficiency: 0.28,
            reversal_pressure: 0.24,
            sign_flip_rate: 0.35,
            realized_vol_180s_bps: 10.0,
        };

        assert!(!should_delay_whipsaw_entry(
            BteDecisionPosition::default(),
            regime,
            60.0,
            60.0,
        ));
    }
}
