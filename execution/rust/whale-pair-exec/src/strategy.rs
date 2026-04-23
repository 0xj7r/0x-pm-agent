use std::collections::HashMap;
use std::env;

use crate::inventory::{InventorySnapshot, PositionState};
use crate::market_context::MarketContextRecord;
use crate::types::{
    ClientOrderId, EpochMillis, InstrumentId, MarketId, MarketSnapshot, OrderIntent,
    QuoteSnapshot, RuntimeStatus, TradeSide,
};

#[derive(Clone, Copy, Debug)]
pub struct GoatPairConfig {
    pub accumulate_price_max: f64,
    pub aggressive_price_max: f64,
    pub base_clip_usd: f64,
    pub aggressive_clip_usd: f64,
    pub max_gross_cost_usd: f64,
    pub completion_min_pnl_per_share: f64,
    pub max_imbalance_ratio: f64,
    pub taker_fee_coeff: f64,
}

impl GoatPairConfig {
    pub fn from_env() -> Self {
        Self {
            accumulate_price_max: parse_f64("WHALE_PAIR_ACCUMULATE_PRICE_MAX", 0.50),
            aggressive_price_max: parse_f64("WHALE_PAIR_AGGRESSIVE_PRICE_MAX", 0.10),
            base_clip_usd: parse_f64("WHALE_PAIR_BASE_CLIP_USD", 20.0),
            aggressive_clip_usd: parse_f64("WHALE_PAIR_AGGRESSIVE_CLIP_USD", 50.0),
            max_gross_cost_usd: parse_f64("WHALE_PAIR_MAX_GROSS_COST_USD", 200.0),
            completion_min_pnl_per_share: parse_f64(
                "WHALE_PAIR_COMPLETION_MIN_PNL_PER_SHARE",
                0.002,
            ),
            max_imbalance_ratio: parse_f64("WHALE_PAIR_MAX_IMBALANCE_RATIO", 3.0),
            taker_fee_coeff: parse_f64("WHALE_PAIR_TAKER_FEE_COEFF", 0.072),
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub struct UnlawfulShearConfig {
    pub cheap_hedge_price_max: f64,
    pub core_price_min: f64,
    pub core_price_max: f64,
    pub min_price_gap: f64,
    pub probe_clip_usd: f64,
    pub core_clip_usd: f64,
    pub hedge_clip_usd: f64,
    pub rebalance_clip_usd: f64,
    pub trim_clip_fraction: f64,
    pub max_gross_cost_usd: f64,
    pub target_hedge_ratio_min: f64,
    pub target_hedge_ratio_max: f64,
    pub salvage_drawdown_ratio: f64,
    pub salvage_bid_floor: f64,
    pub max_open_orders_total: usize,
    pub taker_fee_coeff: f64,
}

impl UnlawfulShearConfig {
    pub fn from_env() -> Self {
        Self {
            cheap_hedge_price_max: parse_f64(
                "WHALE_PAIR_UNLAWFUL_SHEAR_CHEAP_HEDGE_PRICE_MAX",
                0.38,
            ),
            core_price_min: parse_f64("WHALE_PAIR_UNLAWFUL_SHEAR_CORE_PRICE_MIN", 0.52),
            core_price_max: parse_f64("WHALE_PAIR_UNLAWFUL_SHEAR_CORE_PRICE_MAX", 0.92),
            min_price_gap: parse_f64("WHALE_PAIR_UNLAWFUL_SHEAR_MIN_PRICE_GAP", 0.12),
            probe_clip_usd: parse_f64("WHALE_PAIR_UNLAWFUL_SHEAR_PROBE_CLIP_USD", 3.0),
            core_clip_usd: parse_f64("WHALE_PAIR_UNLAWFUL_SHEAR_CORE_CLIP_USD", 20.0),
            hedge_clip_usd: parse_f64("WHALE_PAIR_UNLAWFUL_SHEAR_HEDGE_CLIP_USD", 6.0),
            rebalance_clip_usd: parse_f64(
                "WHALE_PAIR_UNLAWFUL_SHEAR_REBALANCE_CLIP_USD",
                12.0,
            ),
            trim_clip_fraction: parse_f64(
                "WHALE_PAIR_UNLAWFUL_SHEAR_TRIM_CLIP_FRACTION",
                0.30,
            ),
            max_gross_cost_usd: parse_f64(
                "WHALE_PAIR_UNLAWFUL_SHEAR_MAX_GROSS_COST_USD",
                120.0,
            ),
            target_hedge_ratio_min: parse_f64(
                "WHALE_PAIR_UNLAWFUL_SHEAR_TARGET_HEDGE_RATIO_MIN",
                0.20,
            ),
            target_hedge_ratio_max: parse_f64(
                "WHALE_PAIR_UNLAWFUL_SHEAR_TARGET_HEDGE_RATIO_MAX",
                0.60,
            ),
            salvage_drawdown_ratio: parse_f64(
                "WHALE_PAIR_UNLAWFUL_SHEAR_SALVAGE_DRAWDOWN_RATIO",
                0.18,
            ),
            salvage_bid_floor: parse_f64(
                "WHALE_PAIR_UNLAWFUL_SHEAR_SALVAGE_BID_FLOOR",
                0.05,
            ),
            max_open_orders_total: parse_usize(
                "WHALE_PAIR_UNLAWFUL_SHEAR_MAX_OPEN_ORDERS_TOTAL",
                6,
            ),
            taker_fee_coeff: parse_f64("WHALE_PAIR_TAKER_FEE_COEFF", 0.072),
        }
    }
}

#[derive(Debug, Default)]
struct GoatMarketState {
    asks: HashMap<InstrumentId, f64>,
    last_seq: u64,
    last_fill_ms: Option<EpochMillis>,
}

#[derive(Debug, Default)]
struct UnlawfulMarketState {
    quotes: HashMap<InstrumentId, QuoteSnapshot>,
    last_seq: u64,
    last_action_ms: Option<EpochMillis>,
}

#[derive(Debug, Clone, Copy)]
enum UnlawfulShearPhase {
    Early,
    Mid,
    Late,
    VeryLate,
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SettlementLeg {
    Up,
    Down,
    Unknown,
}

#[derive(Clone, Debug, Default)]
pub struct StrategyDecision {
    pub intents: Vec<OrderIntent>,
    pub notes: Vec<String>,
}

impl StrategyDecision {
    pub fn none() -> Self {
        Self::default()
    }

    pub fn single(intent: OrderIntent) -> Self {
        Self {
            intents: vec![intent],
            notes: Vec::new(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.intents.is_empty() && self.notes.is_empty()
    }
}

pub struct StrategyContext {
    pub now_ms: EpochMillis,
    pub runtime_status: RuntimeStatus,
    pub inventory: InventorySnapshot,
    pub open_orders_total: usize,
    pub market_context: Option<MarketContextRecord>,
}

pub trait Strategy {
    fn name(&self) -> &str;

    fn on_start(&mut self, _context: &StrategyContext) -> StrategyDecision {
        StrategyDecision::none()
    }

    fn on_market_snapshot(
        &mut self,
        _context: &StrategyContext,
        _snapshot: &MarketSnapshot,
    ) -> StrategyDecision {
        StrategyDecision::none()
    }

    fn on_fill(
        &mut self,
        _context: &StrategyContext,
        _fill: &crate::types::FillReport,
    ) -> StrategyDecision {
        StrategyDecision::none()
    }
}

#[derive(Debug)]
pub enum StrategyMode {
    Goat(GoatPairStrategy),
    UnlawfulShear(UnlawfulShearStrategy),
    Noop(NoopStrategy),
}

impl StrategyMode {
    pub fn from_name(name: &str) -> Self {
        match name {
            "goat_pair" => Self::Goat(GoatPairStrategy::with_defaults()),
            "noop" => Self::Noop(NoopStrategy),
            "unlawful_shear" => Self::UnlawfulShear(UnlawfulShearStrategy::with_defaults()),
            _ => Self::UnlawfulShear(UnlawfulShearStrategy::with_defaults()),
        }
    }

    pub fn taker_fee_coeff(&self) -> f64 {
        match self {
            Self::Goat(strategy) => strategy.taker_fee_coeff(),
            Self::UnlawfulShear(strategy) => strategy.taker_fee_coeff(),
            Self::Noop(_) => 0.0,
        }
    }
}

impl Strategy for StrategyMode {
    fn name(&self) -> &str {
        match self {
            Self::Goat(strategy) => strategy.name(),
            Self::UnlawfulShear(strategy) => strategy.name(),
            Self::Noop(strategy) => strategy.name(),
        }
    }

    fn on_start(&mut self, context: &StrategyContext) -> StrategyDecision {
        match self {
            Self::Goat(strategy) => strategy.on_start(context),
            Self::UnlawfulShear(strategy) => strategy.on_start(context),
            Self::Noop(strategy) => strategy.on_start(context),
        }
    }

    fn on_market_snapshot(
        &mut self,
        context: &StrategyContext,
        snapshot: &MarketSnapshot,
    ) -> StrategyDecision {
        match self {
            Self::Goat(strategy) => strategy.on_market_snapshot(context, snapshot),
            Self::UnlawfulShear(strategy) => strategy.on_market_snapshot(context, snapshot),
            Self::Noop(strategy) => strategy.on_market_snapshot(context, snapshot),
        }
    }

    fn on_fill(
        &mut self,
        context: &StrategyContext,
        fill: &crate::types::FillReport,
    ) -> StrategyDecision {
        match self {
            Self::Goat(strategy) => strategy.on_fill(context, fill),
            Self::UnlawfulShear(strategy) => strategy.on_fill(context, fill),
            Self::Noop(strategy) => strategy.on_fill(context, fill),
        }
    }
}

#[derive(Debug)]
pub struct GoatPairStrategy {
    config: GoatPairConfig,
    cooldown_ms: u64,
    market_states: HashMap<MarketId, GoatMarketState>,
}

impl GoatPairStrategy {
    pub fn new(config: GoatPairConfig, cooldown_ms: u64) -> Self {
        Self {
            config,
            cooldown_ms,
            market_states: HashMap::new(),
        }
    }

    pub fn with_defaults() -> Self {
        Self::new(GoatPairConfig::from_env(), 250)
    }

    pub fn taker_fee_coeff(&self) -> f64 {
        self.config.taker_fee_coeff
    }

    pub fn estimate_taker_fee_usd(&self, price: f64, quantity: f64) -> f64 {
        if !(price.is_finite() && quantity.is_finite()) {
            return 0.0;
        }
        let notional_usd = price * quantity;
        if notional_usd <= 0.0 {
            return 0.0;
        }
        notional_usd * self.config.taker_fee_coeff * price * (1.0 - price)
    }

    fn position_for(
        &self,
        inventory: &InventorySnapshot,
        instrument_id: &InstrumentId,
    ) -> (f64, f64) {
        inventory
            .positions
            .iter()
            .find(|position| &position.instrument_id == instrument_id)
            .map_or((0.0, 0.0), |position| (position.quantity, position.avg_price))
    }

    fn gross_cost_usd(&self, inventory: &InventorySnapshot, market_id: &MarketId) -> f64 {
        inventory
            .positions
            .iter()
            .filter(|position| &position.market_id == market_id)
            .map(|position| position.quantity.abs() * position.avg_price)
            .sum()
    }

    fn taker_fee_usd(&self, price: f64, notional: f64) -> f64 {
        notional * self.config.taker_fee_coeff * price * (1.0 - price)
    }

    fn choose_clip_usd(&self, best_ask: f64) -> Option<f64> {
        if best_ask <= self.config.aggressive_price_max {
            Some(self.config.aggressive_clip_usd)
        } else if best_ask <= self.config.accumulate_price_max {
            Some(self.config.base_clip_usd)
        } else {
            None
        }
    }

    fn completion_pnl_per_share(&self, opposite_avg: f64, price: f64) -> f64 {
        1.0 - opposite_avg - price - self.config.taker_fee_coeff * price * (1.0 - price)
    }

    fn build_order(
        &mut self,
        market_id: MarketId,
        instrument_id: InstrumentId,
        price: f64,
        quantity: f64,
        reason: String,
        now_ms: EpochMillis,
    ) -> OrderIntent {
        let market_state = self.market_states.entry(market_id.clone()).or_default();
        market_state.last_seq = market_state.last_seq.saturating_add(1);
        market_state.last_fill_ms = Some(now_ms);
        let client_order_id = ClientOrderId::from(format!(
            "{}-{}-{}-{}",
            market_id.as_str(),
            market_state.last_seq,
            instrument_id.as_str(),
            now_ms
        ));

        OrderIntent {
            client_order_id,
            market_id,
            instrument_id,
            side: TradeSide::Buy,
            limit_price: price,
            quantity,
            reduce_only: false,
            reason,
            created_at_ms: now_ms,
        }
    }

    fn top_sides(&self, snapshot: &MarketSnapshot) -> Option<(InstrumentId, f64)> {
        let book = &snapshot.quote;
        let ask = book
            .best_ask
            .as_ref()
            .map(|level| level.price)
            .filter(|price| *price > 0.0)?;
        Some((snapshot.instrument_id.clone(), ask))
    }
}

impl Strategy for GoatPairStrategy {
    fn name(&self) -> &str {
        "goat_pair"
    }

    fn on_start(&mut self, _context: &StrategyContext) -> StrategyDecision {
        StrategyDecision::none()
    }

    fn on_market_snapshot(
        &mut self,
        context: &StrategyContext,
        snapshot: &MarketSnapshot,
    ) -> StrategyDecision {
        // TODO(2026-04-23): this strategy does not yet model hidden intent signals, whale event
        // clustering, or partial-orderbook liquidity shifts seen in real two-sided execution.
        if context.runtime_status != RuntimeStatus::Running {
            return StrategyDecision::none();
        }

        let (instrument_id, ask) = match self.top_sides(snapshot) {
            Some(next) => next,
            None => return StrategyDecision::none(),
        };

        let market_state = self.market_states.entry(snapshot.market_id.clone()).or_default();
        let last_fill_ms = market_state.last_fill_ms.unwrap_or_default();
        if self.cooldown_ms > 0 && context.now_ms.saturating_sub(last_fill_ms) < self.cooldown_ms {
            market_state.asks.insert(instrument_id.clone(), ask);
            return StrategyDecision::none();
        }

        market_state.asks.insert(instrument_id.clone(), ask);
        if market_state.asks.len() < 2 {
            return StrategyDecision::none();
        }

        let mut sides: Vec<(InstrumentId, f64)> = market_state
            .asks
            .iter()
            .map(|(id, level)| (id.clone(), *level))
            .collect();
        sides.retain(|(_, level)| level.is_finite() && *level > 0.0);
        if sides.len() < 2 {
            return StrategyDecision::none();
        }

        sides.sort_by(|left, right| left.1.total_cmp(&right.1));
        let mut candidate = None;
        for (side_instrument, side_ask) in sides.clone() {
            let (this_qty, _) = self.position_for(&context.inventory, &side_instrument);
            let opposite_instrument = if side_instrument == sides[0].0 {
                sides[1].0.clone()
            } else {
                sides[0].0.clone()
            };
            let (opp_qty, opp_avg) = self.position_for(&context.inventory, &opposite_instrument);
            let opposite_has = opp_qty > 0.0;

            let clip_usd = self.choose_clip_usd(side_ask).or_else(|| {
                if opposite_has && opp_qty > this_qty {
                    let pnl = self.completion_pnl_per_share(opp_avg, side_ask);
                    if pnl >= self.config.completion_min_pnl_per_share {
                        Some(self.config.base_clip_usd)
                    } else {
                        None
                    }
                } else {
                    None
                }
            });

            let Some(clip_usd) = clip_usd else {
                continue;
            };

            if opposite_has {
                let projected_same = this_qty + clip_usd / side_ask;
                let ratio = projected_same / opp_qty.max(1e-9);
                if ratio > self.config.max_imbalance_ratio {
                    continue;
                }
            }

            let remaining_gross = self.config.max_gross_cost_usd
                - self.gross_cost_usd(&context.inventory, &snapshot.market_id);
            let remaining_qty = remaining_gross.max(0.0) / side_ask;
            if remaining_qty <= 0.0 || remaining_qty * side_ask < 5e-4 {
                return StrategyDecision::none();
            }

            let mut qty = clip_usd / side_ask;
            if qty > remaining_qty {
                qty = remaining_qty;
            }
            if qty <= 0.0 {
                return StrategyDecision::none();
            }

            let fee = self.taker_fee_usd(side_ask, qty * side_ask);
            let reason = if clip_usd >= self.config.aggressive_clip_usd {
                format!(
                    "goat-pair accumulate aggressive p={side_ask:.4},fee={fee:.4},qty={qty:.4}"
                )
            } else if this_qty > 0.0 && opp_qty > this_qty {
                format!("goat-pair completion p={side_ask:.4},opp_avg={opp_avg:.4}")
            } else {
                format!("goat-pair accumulate p={side_ask:.4},qty={qty:.4}")
            };
            let order = self.build_order(
                snapshot.market_id.clone(),
                side_instrument,
                side_ask,
                qty,
                reason,
                context.now_ms,
            );
            candidate = Some(order);
            break;
        }

        let Some(order) = candidate else {
            return StrategyDecision::none();
        };
        StrategyDecision::single(order)
    }

    fn on_fill(
        &mut self,
        _context: &StrategyContext,
        fill: &crate::types::FillReport,
    ) -> StrategyDecision {
        let note = format!(
            "fill {} {} qty={}@{} fee={:.4}",
            fill.instrument_id,
            match fill.side {
                TradeSide::Buy => "BUY",
                TradeSide::Sell => "SELL",
            },
            fill.quantity,
            fill.price,
            fill.fee_usd
        );
        StrategyDecision {
            intents: Vec::new(),
            notes: vec![note],
        }
    }
}

#[derive(Debug)]
pub struct UnlawfulShearStrategy {
    config: UnlawfulShearConfig,
    cooldown_ms: u64,
    market_states: HashMap<MarketId, UnlawfulMarketState>,
}

impl UnlawfulShearStrategy {
    pub fn new(config: UnlawfulShearConfig, cooldown_ms: u64) -> Self {
        Self {
            config,
            cooldown_ms,
            market_states: HashMap::new(),
        }
    }

    pub fn with_defaults() -> Self {
        Self::new(
            UnlawfulShearConfig::from_env(),
            parse_u64("WHALE_PAIR_UNLAWFUL_SHEAR_COOLDOWN_MS", 400),
        )
    }

    pub fn taker_fee_coeff(&self) -> f64 {
        self.config.taker_fee_coeff
    }

    fn best_bid(quote: &QuoteSnapshot) -> Option<f64> {
        quote
            .best_bid
            .as_ref()
            .map(|level| level.price)
            .filter(|price| price.is_finite() && *price > 0.0)
    }

    fn best_ask(quote: &QuoteSnapshot) -> Option<f64> {
        quote
            .best_ask
            .as_ref()
            .map(|level| level.price)
            .filter(|price| price.is_finite() && *price > 0.0)
    }

    fn position_state<'a>(
        &self,
        inventory: &'a InventorySnapshot,
        instrument_id: &InstrumentId,
    ) -> Option<&'a PositionState> {
        inventory
            .positions
            .iter()
            .find(|position| &position.instrument_id == instrument_id)
    }

    fn gross_cost_usd(&self, inventory: &InventorySnapshot, market_id: &MarketId) -> f64 {
        inventory
            .positions
            .iter()
            .filter(|position| &position.market_id == market_id)
            .map(|position| position.quantity.abs() * position.avg_price)
            .sum()
    }

    fn build_order(
        &mut self,
        market_id: MarketId,
        instrument_id: InstrumentId,
        side: TradeSide,
        price: f64,
        quantity: f64,
        reduce_only: bool,
        reason: String,
        now_ms: EpochMillis,
    ) -> OrderIntent {
        let market_state = self.market_states.entry(market_id.clone()).or_default();
        market_state.last_seq = market_state.last_seq.saturating_add(1);
        market_state.last_action_ms = Some(now_ms);
        let client_order_id = ClientOrderId::from(format!(
            "{}-{}-{}-{}-{}",
            market_id.as_str(),
            market_state.last_seq,
            instrument_id.as_str(),
            match side {
                TradeSide::Buy => "buy",
                TradeSide::Sell => "sell",
            },
            now_ms
        ));

        OrderIntent {
            client_order_id,
            market_id,
            instrument_id,
            side,
            limit_price: price,
            quantity,
            reduce_only,
            reason,
            created_at_ms: now_ms,
        }
    }

    fn push_buy(
        &mut self,
        intents: &mut Vec<OrderIntent>,
        remaining_gross: &mut f64,
        market_id: &MarketId,
        instrument_id: &InstrumentId,
        price: f64,
        clip_usd: f64,
        reason: String,
        now_ms: EpochMillis,
    ) {
        if price <= 0.0 || clip_usd <= 0.0 || *remaining_gross <= 0.0 {
            return;
        }
        let notional = clip_usd.min(*remaining_gross);
        if notional < 1e-3 {
            return;
        }
        let quantity = notional / price;
        if quantity <= 0.0 {
            return;
        }
        intents.push(self.build_order(
            market_id.clone(),
            instrument_id.clone(),
            TradeSide::Buy,
            price,
            quantity,
            false,
            reason,
            now_ms,
        ));
        *remaining_gross -= quantity * price;
    }

    fn push_sell(
        &mut self,
        intents: &mut Vec<OrderIntent>,
        market_id: &MarketId,
        instrument_id: &InstrumentId,
        price: f64,
        quantity: f64,
        reason: String,
        now_ms: EpochMillis,
    ) {
        if price <= 0.0 || quantity <= 0.0 {
            return;
        }
        intents.push(self.build_order(
            market_id.clone(),
            instrument_id.clone(),
            TradeSide::Sell,
            price,
            quantity,
            true,
            reason,
            now_ms,
        ));
    }

    fn window_progress(&self, context: &StrategyContext) -> Option<f64> {
        let market = context.market_context.as_ref()?;
        let start = market.event_start_time_ms?;
        let end = market.event_end_time_ms?;
        if end <= start {
            return None;
        }
        let elapsed = context.now_ms.saturating_sub(start);
        let total = end.saturating_sub(start).max(1);
        Some((elapsed as f64 / total as f64).clamp(0.0, 1.0))
    }

    fn window_elapsed_seconds(&self, context: &StrategyContext) -> Option<u64> {
        let market = context.market_context.as_ref()?;
        let start = market.event_start_time_ms?;
        if context.now_ms <= start {
            return Some(0);
        }
        let end = market.event_end_time_ms?;
        if end <= start {
            return None;
        }
        Some((context.now_ms.saturating_sub(start)) / 1_000)
    }

    fn determine_phase(&self, context: &StrategyContext) -> UnlawfulShearPhase {
        let elapsed = match self.window_elapsed_seconds(context) {
            Some(value) => value,
            None => {
                return match self.window_progress(context) {
                    Some(progress) if progress <= 0.22 => UnlawfulShearPhase::Early,
                    Some(progress) if progress <= 0.62 => UnlawfulShearPhase::Mid,
                    Some(progress) if progress <= 0.80 => UnlawfulShearPhase::Late,
                    Some(_) => UnlawfulShearPhase::VeryLate,
                    None => UnlawfulShearPhase::Unknown,
                };
            }
        };

        if elapsed <= 60 {
            UnlawfulShearPhase::Early
        } else if elapsed <= 180 {
            UnlawfulShearPhase::Mid
        } else if elapsed <= 240 {
            UnlawfulShearPhase::Late
        } else {
            UnlawfulShearPhase::VeryLate
        }
    }

    fn phase_clip_scale(&self, phase: UnlawfulShearPhase) -> f64 {
        match phase {
            UnlawfulShearPhase::Early => 0.35,
            UnlawfulShearPhase::Mid => 0.9,
            UnlawfulShearPhase::Late => 1.05,
            UnlawfulShearPhase::VeryLate => 1.35,
            UnlawfulShearPhase::Unknown => 1.0,
        }
    }

    fn window_at_or_past_end(&self, context: &StrategyContext) -> bool {
        let Some(market) = context.market_context.as_ref() else {
            return false;
        };
        let Some(end_ms) = market.event_end_time_ms else {
            return false;
        };
        context.now_ms >= end_ms
    }

    fn is_up_like(instrument_id: &InstrumentId) -> bool {
        let id = instrument_id.as_str().to_ascii_lowercase();
        id.contains("up") || id.contains("long") || id.contains("bull")
    }

    fn is_down_like(instrument_id: &InstrumentId) -> bool {
        let id = instrument_id.as_str().to_ascii_lowercase();
        id.contains("down") || id.contains("short") || id.contains("bear")
    }

    fn infer_winning_leg(
        &self,
        market: &crate::market_context::MarketContextRecord,
        cheap_id: &InstrumentId,
        expensive_id: &InstrumentId,
    ) -> SettlementLeg {
        let Some(final_price) = market.final_price else {
            return SettlementLeg::Unknown;
        };
        let Some(price_to_beat) = market.price_to_beat else {
            return SettlementLeg::Unknown;
        };
        let expect_up_wins = final_price >= price_to_beat;
        if expect_up_wins {
            if Self::is_up_like(cheap_id) && !Self::is_up_like(expensive_id) {
                SettlementLeg::Up
            } else if Self::is_up_like(expensive_id) && !Self::is_up_like(cheap_id) {
                SettlementLeg::Up
            } else {
                SettlementLeg::Unknown
            }
        } else if Self::is_down_like(cheap_id) && !Self::is_down_like(expensive_id) {
            SettlementLeg::Down
        } else if Self::is_down_like(expensive_id) && !Self::is_down_like(cheap_id) {
            SettlementLeg::Down
        } else {
            SettlementLeg::Unknown
        }
    }

    fn fallback_close_fraction(&self, phase: UnlawfulShearPhase, progress: Option<f64>) -> f64 {
        let base = match phase {
            UnlawfulShearPhase::Early => 0.25,
            UnlawfulShearPhase::Mid => 0.40,
            UnlawfulShearPhase::Late => 0.65,
            UnlawfulShearPhase::VeryLate => 0.90,
            UnlawfulShearPhase::Unknown => 0.55,
        };
        if progress.unwrap_or(0.0) >= 0.995 {
            1.0
        } else {
            base
        }
    }

    fn winning_instrument_id(
        &self,
        market: &crate::market_context::MarketContextRecord,
        cheap_id: &InstrumentId,
        expensive_id: &InstrumentId,
    ) -> Option<InstrumentId> {
        match self.infer_winning_leg(market, cheap_id, expensive_id) {
            SettlementLeg::Up => {
                if Self::is_up_like(cheap_id) {
                    Some(cheap_id.clone())
                } else if Self::is_up_like(expensive_id) {
                    Some(expensive_id.clone())
                } else {
                    None
                }
            }
            SettlementLeg::Down => {
                if Self::is_down_like(cheap_id) {
                    Some(cheap_id.clone())
                } else if Self::is_down_like(expensive_id) {
                    Some(expensive_id.clone())
                } else {
                    None
                }
            }
            SettlementLeg::Unknown => None,
        }
    }
}

impl Strategy for UnlawfulShearStrategy {
    fn name(&self) -> &str {
        "unlawful_shear"
    }

    fn on_market_snapshot(
        &mut self,
        context: &StrategyContext,
        snapshot: &MarketSnapshot,
    ) -> StrategyDecision {
        if context.runtime_status != RuntimeStatus::Running {
            return StrategyDecision::none();
        }
        if context.open_orders_total >= self.config.max_open_orders_total {
            return StrategyDecision::none();
        }

        let (mut sides, last_action_ms) = {
            let market_state = self.market_states.entry(snapshot.market_id.clone()).or_default();
            let last_action_ms = market_state.last_action_ms.unwrap_or_default();
            market_state
                .quotes
                .insert(snapshot.instrument_id.clone(), snapshot.quote.clone());
            let sides = market_state
                .quotes
                .iter()
                .map(|(instrument_id, quote)| (instrument_id.clone(), quote.clone()))
                .collect::<Vec<_>>();
            (sides, last_action_ms)
        };

        if self.cooldown_ms > 0 && context.now_ms.saturating_sub(last_action_ms) < self.cooldown_ms
        {
            return StrategyDecision::none();
        }

        sides.retain(|(_, quote)| Self::best_ask(quote).is_some());
        if sides.len() < 2 {
            return StrategyDecision::none();
        }

        sides.sort_by(|left, right| {
            let left_ask = Self::best_ask(&left.1).unwrap_or(1.0);
            let right_ask = Self::best_ask(&right.1).unwrap_or(1.0);
            left_ask.total_cmp(&right_ask)
        });
        let (cheap_id, cheap_quote) = (&sides[0].0, &sides[0].1);
        let (expensive_id, expensive_quote) = (&sides[sides.len() - 1].0, &sides[sides.len() - 1].1);
        let Some(cheap_ask) = Self::best_ask(cheap_quote) else {
            return StrategyDecision::none();
        };
        let Some(expensive_ask) = Self::best_ask(expensive_quote) else {
            return StrategyDecision::none();
        };
        let price_gap = expensive_ask - cheap_ask;

        let cheap_position = self.position_state(&context.inventory, cheap_id);
        let expensive_position = self.position_state(&context.inventory, expensive_id);

        let cheap_qty = cheap_position.map(|position| position.quantity).unwrap_or(0.0);
        let cheap_avg = cheap_position.map(|position| position.avg_price).unwrap_or(0.0);
        let cheap_cost = cheap_qty * cheap_avg;
        let expensive_qty = expensive_position.map(|position| position.quantity).unwrap_or(0.0);
        let expensive_avg = expensive_position.map(|position| position.avg_price).unwrap_or(0.0);
        let expensive_cost = expensive_qty * expensive_avg;
        let paired = cheap_qty > 0.0 && expensive_qty > 0.0;
        let hedge_ratio = if expensive_cost > 0.0 {
            cheap_cost / expensive_cost.max(1e-9)
        } else {
            0.0
        };

        let mut intents = Vec::new();
        let progress = self.window_progress(context);
        let phase = self.determine_phase(context);
        let phase_clip_scale = self.phase_clip_scale(phase);
        let salvage_phase = matches!(phase, UnlawfulShearPhase::Late | UnlawfulShearPhase::VeryLate)
            || progress.is_none();
        let mut notes = vec![format!(
            "market={} cheap={} ask={:.4} expensive={} ask={:.4} gap={:.4} hedge_ratio={:.4} progress={:?}",
            snapshot.market_id,
            cheap_id,
            cheap_ask,
            expensive_id,
            expensive_ask,
            price_gap,
            hedge_ratio,
            progress
        )];

        let expensive_bid = Self::best_bid(expensive_quote).unwrap_or(0.0);
        let cheap_bid = Self::best_bid(cheap_quote).unwrap_or(0.0);
        let near_end = self.window_at_or_past_end(context)
            || progress.is_some_and(|value| value >= 0.96);
        let close_fraction = self.fallback_close_fraction(phase, progress);
        if near_end {
            if let Some(market) = context.market_context.as_ref() {
                if market.final_price.is_some() {
                    let winner = self.winning_instrument_id(market, cheap_id, expensive_id);
                    if let Some(winner_id) = winner {
                        if cheap_qty > 0.0 && cheap_bid > 0.0 {
                            self.push_sell(
                                &mut intents,
                                &snapshot.market_id,
                                cheap_id,
                                cheap_bid,
                                cheap_qty,
                                format!(
                                    "unlawful-shear end-window close winner={}",
                                    if cheap_id == &winner_id { "cheap" } else { "loser" },
                                ),
                                context.now_ms,
                            );
                        }
                        if expensive_qty > 0.0 && expensive_bid > 0.0 {
                            self.push_sell(
                                &mut intents,
                                &snapshot.market_id,
                                expensive_id,
                                expensive_bid,
                                expensive_qty,
                                format!(
                                    "unlawful-shear end-window close winner={}",
                                    if expensive_id == &winner_id {
                                        "expensive"
                                    } else {
                                        "loser"
                                    },
                                ),
                                context.now_ms,
                            );
                        }
                        notes.push(format!(
                            "end-window settlement anchor final={} beat={} winner={}",
                            market.final_price.unwrap_or(0.0),
                            market.price_to_beat.unwrap_or(0.0),
                            if Self::is_up_like(&winner_id) {
                                "up"
                            } else if Self::is_down_like(&winner_id) {
                                "down"
                            } else {
                                "unknown"
                            }
                        ));
                    } else {
                        notes.push(format!(
                            "end-window settlement attempted but winner leg could not be inferred from ids {} and {}",
                            cheap_id,
                            expensive_id
                        ));
                    }
                } else {
                    notes.push(format!(
                        "end-window settlement skipped: final_price missing for market={}",
                        snapshot.market_id
                    ));
                }
            } else {
                notes.push(format!(
                    "end-window settlement skipped: market context missing for market={}",
                    snapshot.market_id
                ));
            }
            if intents.is_empty() {
                let close_qty = |qty: f64, fraction: f64| (qty * fraction).min(qty);
                if cheap_qty > 0.0 && cheap_bid > 0.0 {
                    let amount = close_qty(cheap_qty, close_fraction);
                    if amount > 0.0 {
                        self.push_sell(
                            &mut intents,
                            &snapshot.market_id,
                            cheap_id,
                            cheap_bid,
                            amount,
                            format!(
                                "unlawful-shear end-window fallback cleanup fraction={:.2}",
                                close_fraction
                            ),
                            context.now_ms,
                        );
                    }
                }
                if expensive_qty > 0.0 && expensive_bid > 0.0 {
                    let amount = close_qty(expensive_qty, close_fraction);
                    if amount > 0.0 {
                        self.push_sell(
                            &mut intents,
                            &snapshot.market_id,
                            expensive_id,
                            expensive_bid,
                            amount,
                            format!(
                                "unlawful-shear end-window fallback cleanup fraction={:.2}",
                                close_fraction
                            ),
                            context.now_ms,
                        );
                    }
                }
                if !intents.is_empty() {
                    notes.push(format!(
                        "end-window fallback cleanup executed fraction={:.2}",
                        close_fraction
                    ));
                }
            }
            if !intents.is_empty() {
                if let Some(market) = context.market_context.as_ref() {
                    if let Some(end_ms) = market.event_end_time_ms {
                        notes.push(format!("forced close at {} phase={:?}", end_ms, phase));
                    }
                }
                notes.push("unlawful-shear closing window for attribution".to_string());
                return StrategyDecision { intents, notes };
            }
        }

        if paired {
            if expensive_avg > 0.0
                && expensive_bid >= self.config.salvage_bid_floor
                && expensive_bid <= expensive_avg * (1.0 - self.config.salvage_drawdown_ratio)
                && expensive_cost > cheap_cost * 0.8
                && salvage_phase
            {
                let trim_qty = (expensive_qty * self.config.trim_clip_fraction).min(expensive_qty);
                self.push_sell(
                    &mut intents,
                    &snapshot.market_id,
                    expensive_id,
                    expensive_bid,
                    trim_qty,
                    format!(
                        "unlawful-shear salvage expensive bid={:.4} avg={:.4}",
                        expensive_bid, expensive_avg
                    ),
                    context.now_ms,
                );
            } else if cheap_avg > 0.0
                && cheap_bid >= self.config.salvage_bid_floor
                && cheap_bid <= cheap_avg * (1.0 - self.config.salvage_drawdown_ratio)
                && cheap_cost > expensive_cost * self.config.target_hedge_ratio_max
                && salvage_phase
            {
                let trim_qty = (cheap_qty * self.config.trim_clip_fraction).min(cheap_qty);
                self.push_sell(
                    &mut intents,
                    &snapshot.market_id,
                    cheap_id,
                    cheap_bid,
                    trim_qty,
                    format!(
                        "unlawful-shear salvage hedge bid={:.4} avg={:.4}",
                        cheap_bid, cheap_avg
                    ),
                    context.now_ms,
                );
            }
        }

        let mut remaining_gross =
            self.config.max_gross_cost_usd - self.gross_cost_usd(&context.inventory, &snapshot.market_id);

        let core_candidate = price_gap >= self.config.min_price_gap
            && expensive_ask >= self.config.core_price_min
            && expensive_ask <= self.config.core_price_max;
        let cheap_candidate = cheap_ask <= self.config.cheap_hedge_price_max;
        if expensive_cost <= 0.0 && core_candidate {
            let core_clip_base = self.config.core_clip_usd * phase_clip_scale;
            let core_clip_usd = match phase {
                UnlawfulShearPhase::VeryLate => core_clip_base * 1.25,
                UnlawfulShearPhase::Early => self.config.probe_clip_usd.max(core_clip_base),
                _ => core_clip_base,
            };
            self.push_buy(
                &mut intents,
                &mut remaining_gross,
                &snapshot.market_id,
                expensive_id,
                expensive_ask,
                core_clip_usd,
                format!(
                    "unlawful-shear core-entry gap={:.4} ask={:.4}",
                    price_gap, expensive_ask
                ),
                context.now_ms,
            );
        }

        if cheap_cost <= 0.0 && cheap_candidate {
            let hedge_probe_usd = if matches!(phase, UnlawfulShearPhase::Early) {
                self.config.probe_clip_usd * 0.75
            } else {
                (self.config.hedge_clip_usd * phase_clip_scale).min(self.config.hedge_clip_usd)
            };
            self.push_buy(
                &mut intents,
                &mut remaining_gross,
                &snapshot.market_id,
                cheap_id,
                cheap_ask,
                hedge_probe_usd,
                format!(
                    "unlawful-shear hedge-probe gap={:.4} ask={:.4}",
                    price_gap, cheap_ask
                ),
                context.now_ms,
            );
        }

        if paired {
            if hedge_ratio < self.config.target_hedge_ratio_min && cheap_candidate {
                let hedge_clip_usd = match phase {
                    UnlawfulShearPhase::VeryLate => self.config.hedge_clip_usd * 1.35,
                    UnlawfulShearPhase::Late => self.config.hedge_clip_usd * 1.15,
                    _ => self.config.hedge_clip_usd * phase_clip_scale,
                };
                self.push_buy(
                    &mut intents,
                    &mut remaining_gross,
                    &snapshot.market_id,
                    cheap_id,
                    cheap_ask,
                    hedge_clip_usd,
                    format!(
                        "unlawful-shear add-hedge ratio={:.4} ask={:.4}",
                        hedge_ratio, cheap_ask
                    ),
                    context.now_ms,
                );
            } else if hedge_ratio > self.config.target_hedge_ratio_max && core_candidate {
                let rebalance_clip_usd = match phase {
                    UnlawfulShearPhase::VeryLate => self.config.rebalance_clip_usd * 1.35,
                    UnlawfulShearPhase::Late => self.config.rebalance_clip_usd * 1.2,
                    _ => self.config.rebalance_clip_usd * phase_clip_scale,
                };
                self.push_buy(
                    &mut intents,
                    &mut remaining_gross,
                    &snapshot.market_id,
                    expensive_id,
                    expensive_ask,
                    rebalance_clip_usd,
                    format!(
                        "unlawful-shear rebalance-core ratio={:.4} ask={:.4}",
                        hedge_ratio, expensive_ask
                    ),
                    context.now_ms,
                );
            } else if core_candidate
                && cheap_candidate
                && price_gap >= self.config.min_price_gap * 1.5
                && remaining_gross > self.config.probe_clip_usd
            {
                let target_id = if expensive_cost <= cheap_cost {
                    expensive_id
                } else {
                    cheap_id
                };
                let target_price = if target_id == expensive_id {
                    expensive_ask
                } else {
                    cheap_ask
                };
                self.push_buy(
                    &mut intents,
                    &mut remaining_gross,
                    &snapshot.market_id,
                    target_id,
                    target_price,
                    match phase {
                        UnlawfulShearPhase::VeryLate => self.config.rebalance_clip_usd * 1.35,
                        UnlawfulShearPhase::Late => self.config.rebalance_clip_usd * 1.2,
                        _ => self.config.rebalance_clip_usd * phase_clip_scale,
                    },
                    format!(
                        "unlawful-shear high-flip rebalance gap={:.4} target={}",
                        price_gap, target_id
                    ),
                    context.now_ms,
                );
            }
        } else if cheap_candidate
            && cheap_cost <= 0.0
            && !intents.iter().any(|intent| {
                intent.side == TradeSide::Buy && intent.instrument_id == *cheap_id
            })
        {
            self.push_buy(
                &mut intents,
                &mut remaining_gross,
                &snapshot.market_id,
                cheap_id,
                cheap_ask,
                (self.config.probe_clip_usd * phase_clip_scale).max(1.0),
                format!(
                    "unlawful-shear early-probe ask={:.4} gap={:.4}",
                    cheap_ask, price_gap
                ),
                context.now_ms,
            );
        }

        if intents.is_empty() {
            return StrategyDecision::none();
        }

        if let Some(market) = &context.market_context {
            if let Some(price_to_beat) = market.price_to_beat {
                notes.push(format!(
                    "gamma-context price_to_beat={:.4} final_price={:?}",
                    price_to_beat, market.final_price
                ));
            }
        }
        notes.push("unlawful-shear strategy fired from paired-book geometry".to_string());
        let phase_name = match phase {
            UnlawfulShearPhase::Early => "early",
            UnlawfulShearPhase::Mid => "mid",
            UnlawfulShearPhase::Late => "late",
            UnlawfulShearPhase::VeryLate => "very_late",
            UnlawfulShearPhase::Unknown => "unknown",
        };
        notes.push(format!(
            "phase={} clip_scale={:.2} elapsed_sec={:?}",
            phase_name,
            phase_clip_scale,
            self.window_elapsed_seconds(context)
        ));
        StrategyDecision { intents, notes }
    }

    fn on_fill(
        &mut self,
        _context: &StrategyContext,
        fill: &crate::types::FillReport,
    ) -> StrategyDecision {
        let note = format!(
            "unlawful-shear fill {} {} qty={}@{} fee={:.4}",
            fill.instrument_id,
            match fill.side {
                TradeSide::Buy => "BUY",
                TradeSide::Sell => "SELL",
            },
            fill.quantity,
            fill.price,
            fill.fee_usd
        );
        StrategyDecision {
            intents: Vec::new(),
            notes: vec![note],
        }
    }
}

#[derive(Debug, Default, PartialEq)]
pub struct NoopStrategy;

impl Strategy for NoopStrategy {
    fn name(&self) -> &str {
        "noop"
    }
}

fn parse_f64(key: &str, default: f64) -> f64 {
    env::var(key)
        .ok()
        .and_then(|raw| raw.parse::<f64>().ok())
        .unwrap_or(default)
}

fn parse_u64(key: &str, default: u64) -> u64 {
    env::var(key)
        .ok()
        .and_then(|raw| raw.parse::<u64>().ok())
        .unwrap_or(default)
}

fn parse_usize(key: &str, default: usize) -> usize {
    env::var(key)
        .ok()
        .and_then(|raw| raw.parse::<usize>().ok())
        .unwrap_or(default)
}

#[cfg(test)]
mod tests {
    use super::{
        GoatPairConfig, GoatPairStrategy, NoopStrategy, QuoteSnapshot, Strategy, StrategyContext,
        StrategyDecision, UnlawfulShearConfig, UnlawfulShearStrategy,
    };
    use crate::inventory::{InventorySnapshot, PositionState};
    use crate::types::{BookLevel, InstrumentId, MarketId, MarketSnapshot, RuntimeStatus, TradeSide};

    fn snapshot(asset: &str, market: &str, bid: f64, ask: f64, ts: u64) -> MarketSnapshot {
        MarketSnapshot {
            market_id: MarketId::from(market),
            instrument_id: InstrumentId::from(asset),
            quote: QuoteSnapshot {
                best_bid: Some(BookLevel::new(bid, 1000.0)),
                best_ask: Some(BookLevel::new(ask, 1000.0)),
                last_trade_price: Some(ask),
                observed_at_ms: ts,
            },
        }
    }

    fn context(positions: Vec<PositionState>) -> StrategyContext {
        StrategyContext {
            now_ms: 10,
            runtime_status: RuntimeStatus::Running,
            inventory: InventorySnapshot {
                free_cash_usd: 1_000.0,
                reserved_cash_usd: 0.0,
                total_cash_usd: 1_000.0,
                realized_pnl_usd: 0.0,
                gross_exposure_usd: positions
                    .iter()
                    .map(|position| position.quantity * position.avg_price)
                    .sum(),
                positions,
            },
            open_orders_total: 0,
            market_context: None,
        }
    }

    #[test]
    fn noop_stays_idle() {
        let mut strategy = NoopStrategy;
        let decision =
            strategy.on_market_snapshot(&context(Vec::new()), &snapshot("token-up", "market", 0.4, 0.5, 1));
        assert!(matches!(decision, StrategyDecision { intents: ref i, notes: ref n } if i.is_empty() && n.is_empty()));
    }

    #[test]
    fn goat_pairs_builds_order_when_side_cheap() {
        let mut strategy = GoatPairStrategy::new(
            GoatPairConfig {
                accumulate_price_max: 1.0,
                aggressive_price_max: 0.6,
                base_clip_usd: 20.0,
                aggressive_clip_usd: 50.0,
                max_gross_cost_usd: 1000.0,
                completion_min_pnl_per_share: 0.0,
                max_imbalance_ratio: 9.0,
                taker_fee_coeff: 0.072,
            },
            0,
        );
        strategy.on_market_snapshot(&context(Vec::new()), &snapshot("up", "market-a", 0.5, 0.5, 10));
        let decision =
            strategy.on_market_snapshot(&context(Vec::new()), &snapshot("down", "market-a", 0.5, 0.52, 10));
        assert!(!matches!(decision, StrategyDecision { intents: ref i, .. } if i.is_empty()));
    }

    #[test]
    fn unlawful_shear_builds_core_and_hedge() {
        let mut strategy = UnlawfulShearStrategy::new(
            UnlawfulShearConfig {
                cheap_hedge_price_max: 0.40,
                core_price_min: 0.50,
                core_price_max: 0.95,
                min_price_gap: 0.10,
                probe_clip_usd: 10.0,
                core_clip_usd: 35.0,
                hedge_clip_usd: 15.0,
                rebalance_clip_usd: 20.0,
                trim_clip_fraction: 0.25,
                max_gross_cost_usd: 120.0,
                target_hedge_ratio_min: 0.20,
                target_hedge_ratio_max: 0.60,
                salvage_drawdown_ratio: 0.20,
                salvage_bid_floor: 0.05,
                max_open_orders_total: 8,
                taker_fee_coeff: 0.072,
            },
            0,
        );
        let ctx = context(Vec::new());
        strategy.on_market_snapshot(&ctx, &snapshot("up", "market-a", 0.70, 0.74, 10));
        let decision =
            strategy.on_market_snapshot(&ctx, &snapshot("down", "market-a", 0.18, 0.22, 10));
        assert_eq!(decision.intents.len(), 2);
        assert!(decision.intents.iter().any(|intent| intent.side == TradeSide::Buy));
    }

    #[test]
    fn unlawful_shear_salvages_losing_leg() {
        let mut strategy = UnlawfulShearStrategy::new(
            UnlawfulShearConfig {
                cheap_hedge_price_max: 0.40,
                core_price_min: 0.50,
                core_price_max: 0.95,
                min_price_gap: 0.10,
                probe_clip_usd: 10.0,
                core_clip_usd: 35.0,
                hedge_clip_usd: 15.0,
                rebalance_clip_usd: 20.0,
                trim_clip_fraction: 0.25,
                max_gross_cost_usd: 120.0,
                target_hedge_ratio_min: 0.20,
                target_hedge_ratio_max: 0.60,
                salvage_drawdown_ratio: 0.15,
                salvage_bid_floor: 0.05,
                max_open_orders_total: 8,
                taker_fee_coeff: 0.072,
            },
            0,
        );
        let positions = vec![
            PositionState {
                market_id: MarketId::from("market-a"),
                instrument_id: InstrumentId::from("up"),
                quantity: 100.0,
                avg_price: 0.80,
                mark_price: Some(0.60),
                updated_at_ms: 1,
            },
            PositionState {
                market_id: MarketId::from("market-a"),
                instrument_id: InstrumentId::from("down"),
                quantity: 60.0,
                avg_price: 0.18,
                mark_price: Some(0.24),
                updated_at_ms: 1,
            },
        ];
        let ctx = context(positions);
        strategy.on_market_snapshot(&ctx, &snapshot("up", "market-a", 0.55, 0.60, 10));
        let decision =
            strategy.on_market_snapshot(&ctx, &snapshot("down", "market-a", 0.22, 0.25, 10));
        assert!(decision
            .intents
            .iter()
            .any(|intent| intent.side == TradeSide::Sell && intent.reduce_only));
    }
}
