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
use crate::types::{ClientOrderId, InstrumentId, MarketId, OrderIntent, TradeSide};

const NS_PER_MS: i64 = 1_000_000;
const DECISION_CADENCE_MS: u64 = 1_000;

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
    submitted_notional_by_market: HashMap<String, f64>,
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
            submitted_notional_by_market: HashMap::new(),
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

        BteTickResult {
            orders,
            submit_intents,
        }
    }

    fn apply_notional_caps(
        &mut self,
        mut intent: OrderIntent,
        market_id: &MarketId,
    ) -> Option<OrderIntent> {
        let price = intent.limit_price;
        if price <= 0.0 {
            return None;
        }
        let already = *self
            .submitted_notional_by_market
            .get(market_id.as_str())
            .unwrap_or(&0.0);
        let market_headroom = (self.max_market_notional_usd - already).max(0.0);
        if market_headroom <= 0.0 {
            warn!(
                target: "bte_shadow",
                market = %market_id,
                cumulative_usd = already,
                cap_usd = self.max_market_notional_usd,
                "BTE-LIVE notional-cap: per-market cap hit"
            );
            return None;
        }
        let allowed_notional = self.max_order_notional_usd.min(market_headroom);
        if intent.notional_usd() > allowed_notional {
            let new_qty = ((allowed_notional / price).max(0.0) * 100.0).floor() / 100.0;
            if new_qty <= 0.0 {
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
        *self
            .submitted_notional_by_market
            .entry(market_id.as_str().to_string())
            .or_insert(0.0) += intent.notional_usd();
        Some(intent)
    }
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
    let yes_limit = order.limit_price.map(|p| p as f64);
    let mut intent = match order.side {
        Side::BuyYes => OrderIntent::new_buy(
            ClientOrderId::new(format!("bte-{}-{now_ms}-buy-yes", order.market_id)),
            market_id.clone(),
            yes_token.clone(),
            yes_limit.unwrap_or(yes_book.best_ask),
            order.shares,
            "bte taker buy yes",
            now_ms,
        ),
        Side::SellYes => OrderIntent::new_sell(
            ClientOrderId::new(format!("bte-{}-{now_ms}-sell-yes", order.market_id)),
            market_id.clone(),
            yes_token.clone(),
            yes_limit.unwrap_or(yes_book.best_bid),
            order.shares,
            "bte taker sell yes",
            now_ms,
        ),
        Side::BuyNo => {
            let no_token = no_token?.clone();
            let no_px = yes_limit.map(|p| 1.0 - p).or(no_book.map(|b| b.best_ask))?;
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
            let no_px = yes_limit.map(|p| 1.0 - p).or(no_book.map(|b| b.best_bid))?;
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
        intent.quote_level_tag = Some("bte-taker".to_string());
    }
    if intent.limit_price <= 0.0 || intent.quantity <= 0.0 {
        return None;
    }
    if matches!(intent.side, TradeSide::Buy) {
        intent.reduce_only = false;
    }
    Some(intent)
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
