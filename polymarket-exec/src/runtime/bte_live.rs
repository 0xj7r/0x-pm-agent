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

struct ActiveMarket {
    market_id: MarketId,
    market_u32: u32,
    yes_asset_id: String,
    close_ms: u64,
    last_decision_ms: u64,
    events_seen: u64,
}

#[derive(Debug, Default)]
pub struct BteTickResult {
    pub orders: Vec<BteShadowOrder>,
    pub submit_intents: Vec<OrderIntent>,
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
                    match self.apply_notional_caps(intent, market_id, order.side, pos) {
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

    fn apply_notional_caps(
        &mut self,
        mut intent: OrderIntent,
        market_id: &MarketId,
        order_side: Side,
        pos: BteDecisionPosition,
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
        if risk_increasing {
            *self
                .risk_increasing_notional_by_market
                .entry(market_id.as_str().to_string())
                .or_insert(0.0) += intent.notional_usd();
        }
        Some(intent)
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

fn env_positive_f64(key: &str) -> Option<f64> {
    std::env::var(key)
        .ok()
        .and_then(|value| value.trim().parse::<f64>().ok())
        .filter(|value| value.is_finite() && *value > 0.0)
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
            live.apply_notional_caps(add_yes, &market, Side::BuyYes, long_yes_at_cap)
                .is_none(),
            "same-side add should be blocked once current residual exceeds the market cap"
        );

        let capped = live
            .apply_notional_caps(repair_no, &market, Side::BuyNo, long_yes_at_cap)
            .expect("opposite-side repair should not be blocked by the market cap");
        assert!((capped.notional_usd() - 4.0).abs() < 1e-9);
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
}
