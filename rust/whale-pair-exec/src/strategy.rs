use std::collections::HashMap;
use std::env;

use crate::inventory::InventorySnapshot;
use crate::types::{
    BookLevel, ClientOrderId, EpochMillis, InstrumentId, MarketId, MarketSnapshot, OrderIntent,
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
            completion_min_pnl_per_share: parse_f64("WHALE_PAIR_COMPLETION_MIN_PNL_PER_SHARE", 0.002),
            max_imbalance_ratio: parse_f64("WHALE_PAIR_MAX_IMBALANCE_RATIO", 3.0),
            taker_fee_coeff: parse_f64("WHALE_PAIR_TAKER_FEE_COEFF", 0.072),
        }
    }
}

#[derive(Debug, Default)]
struct MarketState {
    asks: HashMap<InstrumentId, f64>,
    last_seq: u64,
    last_fill_ms: Option<EpochMillis>,
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
pub struct GoatPairStrategy {
    config: GoatPairConfig,
    cooldown_ms: u64,
    market_states: HashMap<MarketId, MarketState>,
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

    fn position_for(&self, inventory: &InventorySnapshot, instrument_id: &InstrumentId) -> (f64, f64) {
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

    fn choose_clip_usd(
        &self,
        best_ask: f64,
    ) -> Option<f64> {
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
        let ask = book.best_ask.as_ref().map(|level| level.price).filter(|price| *price > 0.0)?;
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
        for (side_instrument, side_ask) in sides {
            let (this_qty, this_avg) = self.position_for(&context.inventory, &side_instrument);
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
            })?;

            if opposite_has {
                let projected_same = this_qty + clip_usd / side_ask;
                if opp_qty > 0.0 {
                    let ratio = projected_same / (opp_qty.max(1e-9));
                    if ratio > self.config.max_imbalance_ratio {
                        continue;
                    }
                }
            }

            let remaining_gross = self.config.max_gross_cost_usd - self.gross_cost_usd(&context.inventory, &snapshot.market_id);
            let remaining_qty = (remaining_gross.max(0.0)) / side_ask;
            if remaining_qty <= 0.0 {
                return StrategyDecision::none();
            }
            if remaining_qty * side_ask < 5e-4 {
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

        if candidate.is_none() {
            return StrategyDecision::none();
        }

        let order = candidate.expect("candidate must exist");
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

#[cfg(test)]
mod tests {
    use super::{GoatPairConfig, GoatPairStrategy, QuoteSnapshot, Strategy, StrategyContext, StrategyDecision};
    use crate::types::{BookLevel, InstrumentId, MarketId, MarketSnapshot, RuntimeStatus};

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

    #[test]
    fn noop_stays_idle() {
        let mut strategy = NoopStrategy;
        let context = StrategyContext {
            now_ms: 1,
            runtime_status: RuntimeStatus::Running,
            inventory: crate::inventory::InventorySnapshot {
                free_cash_usd: 0.0,
                reserved_cash_usd: 0.0,
                total_cash_usd: 0.0,
                realized_pnl_usd: 0.0,
                gross_exposure_usd: 0.0,
                positions: Vec::new(),
            },
            open_orders_total: 0,
        };
        let decision = strategy.on_market_snapshot(&context, &snapshot("token-up", "market", 0.4, 0.5, 1));
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
        let context = StrategyContext {
            now_ms: 10,
            runtime_status: RuntimeStatus::Running,
            inventory: crate::inventory::InventorySnapshot {
                free_cash_usd: 1000.0,
                reserved_cash_usd: 0.0,
                total_cash_usd: 1000.0,
                realized_pnl_usd: 0.0,
                gross_exposure_usd: 0.0,
                positions: Vec::new(),
            },
            open_orders_total: 0,
        };
        strategy.on_market_snapshot(&context, &snapshot("up", "market-a", 0.5, 0.5, 10));
        let decision = strategy.on_market_snapshot(&context, &snapshot("down", "market-a", 0.5, 0.52, 10));
        assert!(!matches!(decision, StrategyDecision { intents: ref i, .. } if i.is_empty()));
    }
}
