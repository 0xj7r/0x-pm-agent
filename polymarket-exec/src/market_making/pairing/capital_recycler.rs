//! Routine buy-light-side capital recycling.

use crate::market_making::pairing::types::{
    LadderLeg, PairedInventorySnapshot, PairedMarketSnapshot,
};
use crate::markets::MarketDescriptor;
use crate::types::{ClientOrderId, EpochMillis, IntentKind, MmQuoteKind, OrderIntent, TradeSide};

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct CapitalRecycleConfig {
    pub pair_cost_target: f64,
    pub min_imbalance_qty: f64,
    pub max_buy_qty: f64,
    pub max_buy_notional_usd: f64,
    pub min_time_remaining_ms: u64,
    pub max_light_side_spread: f64,
    pub race_buffer_ticks: f64,
}

impl Default for CapitalRecycleConfig {
    fn default() -> Self {
        Self {
            pair_cost_target: 0.99,
            min_imbalance_qty: 1.0,
            max_buy_qty: 25.0,
            max_buy_notional_usd: 25.0,
            min_time_remaining_ms: 120_000,
            max_light_side_spread: 0.10,
            race_buffer_ticks: 1.0,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum CapitalRecycleDecision {
    BuyLightSide {
        intent: OrderIntent,
        leg: LadderLeg,
        projected_pair_cost: f64,
        reason: String,
    },
    Wait {
        reason: String,
    },
}

pub fn choose_capital_recycle<M: MarketDescriptor>(
    market: &M,
    snapshot: &PairedMarketSnapshot,
    inventory: &PairedInventorySnapshot,
    config: CapitalRecycleConfig,
    now_ms: EpochMillis,
) -> CapitalRecycleDecision {
    let remaining_ms = market
        .time_remaining_ms(now_ms)
        .unwrap_or(market.window_ms());
    if remaining_ms < config.min_time_remaining_ms {
        return CapitalRecycleDecision::Wait {
            reason: format!(
                "capital recycle wait: remaining_ms={remaining_ms} below min {}",
                config.min_time_remaining_ms
            ),
        };
    }

    let imbalance_qty = inventory.side_imbalance_qty();
    if imbalance_qty < config.min_imbalance_qty {
        return CapitalRecycleDecision::Wait {
            reason: format!(
                "capital recycle wait: imbalance {:.4} below min {:.4}",
                imbalance_qty, config.min_imbalance_qty
            ),
        };
    }

    let (light_leg, heavy_avg_cost, light_qty, light_avg_cost, light_quote, light_instrument_id) =
        if inventory.yes_qty > inventory.no_qty {
            (
                LadderLeg::No,
                inventory.yes_avg_cost,
                inventory.no_qty,
                inventory.no_avg_cost,
                &snapshot.no_quote,
                market.no_instrument_id().clone(),
            )
        } else {
            (
                LadderLeg::Yes,
                inventory.no_avg_cost,
                inventory.yes_qty,
                inventory.yes_avg_cost,
                &snapshot.yes_quote,
                market.yes_instrument_id().clone(),
            )
        };

    if !heavy_avg_cost.is_finite() || heavy_avg_cost <= 0.0 {
        return CapitalRecycleDecision::Wait {
            reason: format!("capital recycle wait: invalid heavy avg cost {heavy_avg_cost:.4}"),
        };
    }

    let Some(best_ask) = light_quote.best_ask.as_ref().map(|level| level.price) else {
        return CapitalRecycleDecision::Wait {
            reason: "capital recycle wait: missing light-side ask".to_string(),
        };
    };
    if !best_ask.is_finite() || best_ask <= 0.0 || best_ask >= 1.0 {
        return CapitalRecycleDecision::Wait {
            reason: format!("capital recycle wait: invalid light-side ask {best_ask:.4}"),
        };
    }

    if light_quote
        .spread()
        .is_some_and(|spread| spread > config.max_light_side_spread)
    {
        return CapitalRecycleDecision::Wait {
            reason: format!(
                "capital recycle wait: light-side spread {:?} above max {:.4}",
                light_quote.spread(),
                config.max_light_side_spread
            ),
        };
    }

    let tick_size = market.tick_size().max(0.0001);
    let limit_price = (best_ask + tick_size * config.race_buffer_ticks.max(0.0))
        .clamp(tick_size, 1.0 - tick_size);

    let qty_by_notional = config.max_buy_notional_usd / limit_price.max(tick_size);
    let quantity = imbalance_qty.min(config.max_buy_qty).min(qty_by_notional);
    if quantity + 1e-9 < market.min_order_size() {
        return CapitalRecycleDecision::Wait {
            reason: format!(
                "capital recycle wait: quantity {:.4} below venue min {:.4}",
                quantity,
                market.min_order_size()
            ),
        };
    }
    if !quantity.is_finite() || quantity <= 0.0 {
        return CapitalRecycleDecision::Wait {
            reason: "capital recycle wait: computed quantity invalid".to_string(),
        };
    }

    let projected_light_avg_cost =
        if light_qty > 0.0 && light_avg_cost.is_finite() && light_avg_cost > 0.0 {
            ((light_qty * light_avg_cost) + (quantity * limit_price)) / (light_qty + quantity)
        } else {
            limit_price
        };
    let projected_pair_cost = heavy_avg_cost + projected_light_avg_cost;
    if projected_pair_cost > config.pair_cost_target {
        return CapitalRecycleDecision::Wait {
            reason: format!(
                "capital recycle wait: projected_pair_cost={projected_pair_cost:.4} target={:.4}",
                config.pair_cost_target
            ),
        };
    }

    let leg_tag = match light_leg {
        LadderLeg::Yes => "yes",
        LadderLeg::No => "no",
    };
    let intent = OrderIntent {
        client_order_id: ClientOrderId::from(format!(
            "capital-recycle:{}:{}:{}",
            market.market_id(),
            leg_tag,
            now_ms
        )),
        market_id: market.market_id().clone(),
        instrument_id: light_instrument_id,
        side: TradeSide::Buy,
        limit_price,
        quantity,
        reduce_only: false,
        reason: format!(
            "capital recycle buy light side {leg_tag} projected_pair_cost={projected_pair_cost:.4}"
        ),
        quote_level_tag: Some(format!(
            "mm-capital-recycle:{leg_tag}:{:?}",
            MmQuoteKind::CapitalRecycle
        )),
        created_at_ms: now_ms,
        pair_id: Some(format!("capital-recycle:{}:{now_ms}", market.market_id())),
        kind: IntentKind::Close,
    };

    CapitalRecycleDecision::BuyLightSide {
        intent,
        leg: light_leg,
        projected_pair_cost,
        reason: format!(
            "capital recycle buy light side leg={light_leg:?} qty={quantity:.4} price={limit_price:.4} projected_pair_cost={projected_pair_cost:.4}"
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::markets::BinaryOutcomeMarket;
    use crate::types::{BookLevel, InstrumentId, MarketId, QuoteSnapshot};

    fn market() -> BinaryOutcomeMarket {
        let mut market = BinaryOutcomeMarket::btc_5m(
            MarketId::from("m"),
            InstrumentId::from("yes"),
            InstrumentId::from("no"),
        );
        market.event_end_ms = Some(300_000);
        market
    }

    fn snapshot() -> PairedMarketSnapshot {
        PairedMarketSnapshot {
            market_id: MarketId::from("m"),
            yes_instrument_id: InstrumentId::from("yes"),
            no_instrument_id: InstrumentId::from("no"),
            yes_quote: QuoteSnapshot {
                best_bid: Some(BookLevel::new(0.50, 10.0)),
                best_ask: Some(BookLevel::new(0.52, 10.0)),
                ..QuoteSnapshot::default()
            },
            no_quote: QuoteSnapshot {
                best_bid: Some(BookLevel::new(0.40, 10.0)),
                best_ask: Some(BookLevel::new(0.42, 10.0)),
                ..QuoteSnapshot::default()
            },
        }
    }

    #[test]
    fn buys_light_side_when_pair_cost_is_favorable() {
        let decision = choose_capital_recycle(
            &market(),
            &snapshot(),
            &PairedInventorySnapshot {
                yes_qty: 20.0,
                no_qty: 5.0,
                yes_avg_cost: 0.45,
                no_avg_cost: 0.42,
                free_cash_usd: 100.0,
                equity_usd: 100.0,
            },
            CapitalRecycleConfig {
                pair_cost_target: 0.90,
                min_imbalance_qty: 5.0,
                max_buy_qty: 10.0,
                max_buy_notional_usd: 10.0,
                min_time_remaining_ms: 60_000,
                max_light_side_spread: 0.10,
                race_buffer_ticks: 0.0,
            },
            0,
        );
        assert!(matches!(
            decision,
            CapitalRecycleDecision::BuyLightSide {
                leg: LadderLeg::No,
                ..
            }
        ));
    }

    #[test]
    fn projected_pair_cost_uses_existing_light_side_average() {
        let decision = choose_capital_recycle(
            &market(),
            &snapshot(),
            &PairedInventorySnapshot {
                yes_qty: 20.0,
                no_qty: 10.0,
                yes_avg_cost: 0.45,
                no_avg_cost: 0.30,
                free_cash_usd: 100.0,
                equity_usd: 100.0,
            },
            CapitalRecycleConfig {
                pair_cost_target: 0.82,
                min_imbalance_qty: 5.0,
                max_buy_qty: 10.0,
                max_buy_notional_usd: 10.0,
                min_time_remaining_ms: 60_000,
                max_light_side_spread: 0.10,
                race_buffer_ticks: 0.0,
            },
            0,
        );

        let CapitalRecycleDecision::BuyLightSide {
            projected_pair_cost,
            ..
        } = decision
        else {
            panic!("expected buy-light-side decision");
        };

        assert!((projected_pair_cost - 0.81).abs() < 1e-9);
    }
}
