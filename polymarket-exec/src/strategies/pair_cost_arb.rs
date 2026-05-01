//! Pair-cost hedged arbitrage strategy adapter.
//!
//! This is the market-agnostic version of the Gabagool-style loop:
//! buy only the cheap leg when fair-value edge and projected pair cost allow,
//! then let runtime merge balanced inventory.

use crate::market_making::paired_mm::pair_cost_tracker::{Leg, PairCostTracker};
use crate::market_making::paired_mm::types::PairedMarketSnapshot;
use crate::markets::MarketDescriptor;
use crate::strategies::traits::{StrategyInput, TradingStrategy};
use crate::types::{
    ClientOrderId, InstrumentId, IntentKind, OrderIntent, QuoteSnapshot, StrategyDecision,
};

#[derive(Clone, Debug, PartialEq)]
pub struct PairCostArbStrategyConfig {
    pub pair_cost_threshold: f64,
    pub high_vol_pair_cost_threshold: f64,
    pub high_vol_atr_threshold: f64,
    pub pause_in_extreme_vol: bool,
    pub price_deviation_bps: f64,
    pub min_edge_bps: f64,
    pub base_clip_usd: f64,
    pub min_clip_usd: f64,
    pub max_clip_usd: f64,
    pub max_excess_usd: f64,
    pub late_window_sec: u64,
    pub convex_p_threshold: f64,
    pub avoid_rehedging_when_convex: bool,
    pub allow_extra_clip_on_winner: bool,
    pub fractional_kelly: f64,
    pub capital_scale_factor: f64,
    pub maker_safety_ticks: f64,
}

impl Default for PairCostArbStrategyConfig {
    fn default() -> Self {
        Self {
            pair_cost_threshold: 0.99,
            high_vol_pair_cost_threshold: 0.97,
            high_vol_atr_threshold: 0.00085,
            pause_in_extreme_vol: true,
            price_deviation_bps: 35.0,
            min_edge_bps: 100.0,
            base_clip_usd: 1.5,
            min_clip_usd: 0.5,
            max_clip_usd: 4.0,
            max_excess_usd: 8.0,
            late_window_sec: 120,
            convex_p_threshold: 0.70,
            avoid_rehedging_when_convex: true,
            allow_extra_clip_on_winner: true,
            fractional_kelly: 0.15,
            capital_scale_factor: 1.0,
            maker_safety_ticks: 2.0,
        }
    }
}

#[derive(Clone, Debug)]
pub struct PairCostArbStrategy {
    config: PairCostArbStrategyConfig,
}

impl PairCostArbStrategy {
    pub fn new(config: PairCostArbStrategyConfig) -> Self {
        Self { config }
    }

    pub fn config(&self) -> &PairCostArbStrategyConfig {
        &self.config
    }

    fn effective_pair_cost_target(&self, realized_vol_5m_bps: Option<f64>) -> f64 {
        let high_vol = realized_vol_5m_bps
            .map(|bps| (bps / 10_000.0) >= self.config.high_vol_atr_threshold)
            .unwrap_or(false);
        if high_vol {
            self.config.high_vol_pair_cost_threshold
        } else {
            self.config.pair_cost_threshold
        }
    }

    fn is_extreme_vol_paused(&self, realized_vol_5m_bps: Option<f64>) -> bool {
        self.config.pause_in_extreme_vol
            && realized_vol_5m_bps
                .map(|bps| (bps / 10_000.0) >= self.config.high_vol_atr_threshold * 2.0)
                .unwrap_or(false)
    }

    fn active_convex_winner<M: MarketDescriptor>(&self, input: &StrategyInput<M>) -> Option<Leg> {
        let remaining_ms = input.market.time_remaining_ms(input.now_ms)?;
        if remaining_ms > self.config.late_window_sec.saturating_mul(1_000) {
            return None;
        }

        let winner = if input.fair_value.p_up >= self.config.convex_p_threshold {
            Leg::Yes
        } else if input.fair_value.p_down >= self.config.convex_p_threshold {
            Leg::No
        } else {
            return None;
        };

        let has_winner_excess = match winner {
            Leg::Yes => input.inventory.yes_qty > input.inventory.no_qty + 1e-9,
            Leg::No => input.inventory.no_qty > input.inventory.yes_qty + 1e-9,
        };
        has_winner_excess.then_some(winner)
    }

    fn maker_bid_price<M: MarketDescriptor>(
        &self,
        market: &M,
        quote: &QuoteSnapshot,
        max_bid: f64,
    ) -> Option<f64> {
        let best_bid = quote.best_bid.as_ref()?.price;
        let best_ask = quote.best_ask.as_ref()?.price;
        let tick = market.tick_size().max(0.001);
        let maker_cap = best_ask - tick * self.config.maker_safety_ticks.max(0.0);
        let raw = best_bid.min(max_bid).min(maker_cap);
        let price = (raw / tick).floor() * tick;
        (price >= tick && price < best_ask).then_some(price)
    }

    fn projected_pair_cost(
        &self,
        pair_cost: &PairCostTracker,
        snapshot: &PairedMarketSnapshot,
        leg: Leg,
        buy_qty: f64,
        buy_price: f64,
    ) -> Option<f64> {
        if buy_qty <= 0.0 || buy_price <= 0.0 {
            return None;
        }
        let yes_avg = match leg {
            Leg::Yes => weighted_avg(pair_cost.yes_qty, pair_cost.yes_avg_cost, buy_qty, buy_price),
            Leg::No if pair_cost.yes_qty > 0.0 && pair_cost.yes_avg_cost > 0.0 => {
                pair_cost.yes_avg_cost
            }
            Leg::No => snapshot.yes_quote.best_ask.as_ref()?.price,
        };
        let no_avg = match leg {
            Leg::No => weighted_avg(pair_cost.no_qty, pair_cost.no_avg_cost, buy_qty, buy_price),
            Leg::Yes if pair_cost.no_qty > 0.0 && pair_cost.no_avg_cost > 0.0 => {
                pair_cost.no_avg_cost
            }
            Leg::Yes => snapshot.no_quote.best_ask.as_ref()?.price,
        };
        Some(yes_avg + no_avg)
    }

    fn cheap_leg_intent<M: MarketDescriptor>(
        &self,
        input: &StrategyInput<M>,
        leg: Leg,
        fair_price: f64,
        target_pair_cost: f64,
    ) -> Option<(OrderIntent, f64)> {
        let (instrument_id, quote) = match leg {
            Leg::Yes => (
                &input.snapshot.yes_instrument_id,
                &input.snapshot.yes_quote,
            ),
            Leg::No => (&input.snapshot.no_instrument_id, &input.snapshot.no_quote),
        };
        let mid = quote.mid_price()?;
        let required_edge = self
            .config
            .price_deviation_bps
            .max(self.config.min_edge_bps)
            / 10_000.0;
        if mid + required_edge > fair_price {
            return None;
        }

        let max_bid = (fair_price - self.config.min_edge_bps / 10_000.0).clamp(0.0, 0.99);
        let bid_price = self.maker_bid_price(&input.market, quote, max_bid)?;
        let edge_scale = ((fair_price - mid) / required_edge.max(1e-9)).clamp(1.0, 3.0);
        let clip_usd = (self.config.base_clip_usd
            * self.config.capital_scale_factor
            * self.config.fractional_kelly
            * edge_scale)
            .clamp(self.config.min_clip_usd, self.config.max_clip_usd)
            .min(input.inventory.free_cash_usd.max(0.0));
        let mut quantity = clip_usd / bid_price.max(input.market.tick_size().max(0.001));
        if quantity + 1e-9 < input.market.min_order_size() {
            quantity = input.market.min_order_size();
        }
        let notional = quantity * bid_price;
        if notional < self.config.min_clip_usd
            || notional > self.config.max_clip_usd + 1e-9
            || notional > input.inventory.free_cash_usd.max(0.0) + 1e-9
        {
            return None;
        }
        let projected_yes_qty = match leg {
            Leg::Yes => input.inventory.yes_qty + quantity,
            Leg::No => input.inventory.yes_qty,
        };
        let projected_no_qty = match leg {
            Leg::No => input.inventory.no_qty + quantity,
            Leg::Yes => input.inventory.no_qty,
        };
        let current_excess_usd =
            (input.inventory.yes_qty - input.inventory.no_qty).abs() * bid_price;
        let projected_excess_usd = (projected_yes_qty - projected_no_qty).abs() * bid_price;
        if self.config.max_excess_usd > 0.0
            && projected_excess_usd > self.config.max_excess_usd + 1e-9
            && projected_excess_usd > current_excess_usd + 1e-9
        {
            return None;
        }

        let projected_pair_cost = self.projected_pair_cost(
            &input.pair_cost,
            &input.snapshot,
            leg,
            quantity,
            bid_price,
        )?;
        if projected_pair_cost > target_pair_cost {
            return None;
        }

        let leg_name = match leg {
            Leg::Yes => "yes",
            Leg::No => "no",
        };
        let mut intent = OrderIntent::new_buy(
            strategy_client_order_id(
                "pair-cost-arb",
                instrument_id,
                leg_name,
                bid_price,
                quantity,
                input.now_ms,
            ),
            input.snapshot.market_id.clone(),
            instrument_id.clone(),
            bid_price,
            quantity,
            format!(
                "pair-cost cheap-leg buy {leg_name} fair={fair_price:.4} mid={mid:.4} projected_pair_cost={projected_pair_cost:.4} target={target_pair_cost:.4}"
            ),
            input.now_ms,
        );
        intent.quote_level_tag = Some(format!("pair-cost-arb:cheap-leg:{leg_name}"));
        intent.kind = IntentKind::Entry;
        Some((intent, projected_pair_cost))
    }
}

impl<M> TradingStrategy<M> for PairCostArbStrategy
where
    M: MarketDescriptor,
{
    fn name(&self) -> &'static str {
        "pair_cost_arb"
    }

    fn on_tick(&mut self, input: StrategyInput<M>) -> StrategyDecision {
        if self.is_extreme_vol_paused(input.btc_regime.realized_vol_5m_bps) {
            return StrategyDecision::Noop {
                notes: vec!["pair-cost arb suppressed: extreme volatility pause".to_string()],
            };
        }

        let target = self.effective_pair_cost_target(input.btc_regime.realized_vol_5m_bps);
        let convex_winner = self.active_convex_winner(&input);
        let mut candidates = Vec::new();
        let mut notes = vec![format!(
            "pair-cost arb fair p_up={:.4} p_down={:.4} target={:.4}",
            input.fair_value.p_up, input.fair_value.p_down, target
        )];

        let mut allow_leg = |leg: Leg| -> bool {
            let Some(winner) = convex_winner else {
                return true;
            };
            if self.config.avoid_rehedging_when_convex && leg != winner {
                notes.push(format!(
                    "pair-cost convex rule active: suppressing opposite-leg rehedge leg={leg:?} winner={winner:?}"
                ));
                return false;
            }
            if leg == winner && !self.config.allow_extra_clip_on_winner {
                notes.push(format!(
                    "pair-cost convex rule active: winner extra clip disabled winner={winner:?}"
                ));
                return false;
            }
            true
        };

        if allow_leg(Leg::Yes) {
            if let Some(candidate) =
                self.cheap_leg_intent(&input, Leg::Yes, input.fair_value.p_up, target)
            {
                candidates.push(candidate);
            }
        }
        if allow_leg(Leg::No) {
            if let Some(candidate) =
                self.cheap_leg_intent(&input, Leg::No, input.fair_value.p_down, target)
            {
                candidates.push(candidate);
            }
        }
        candidates.sort_by(|left, right| {
            left.1
                .partial_cmp(&right.1)
                .unwrap_or(std::cmp::Ordering::Equal)
        });

        if let Some((intent, projected_pair_cost)) = candidates.into_iter().next() {
            notes.push(format!(
                "pair-cost cheap-leg intent emitted projected_pair_cost={projected_pair_cost:.4}"
            ));
            StrategyDecision::quote_set(vec![intent], notes)
        } else {
            notes.push(
                "pair-cost cheap-leg wait: no leg passed fair-value and pair-cost gates"
                    .to_string(),
            );
            StrategyDecision::Noop { notes }
        }
    }
}

fn weighted_avg(existing_qty: f64, existing_avg: f64, new_qty: f64, new_price: f64) -> f64 {
    if existing_qty > 0.0 && existing_avg > 0.0 {
        ((existing_qty * existing_avg) + (new_qty * new_price)) / (existing_qty + new_qty)
    } else {
        new_price
    }
}

fn strategy_client_order_id(
    prefix: &str,
    instrument_id: &InstrumentId,
    leg_name: &str,
    price: f64,
    quantity: f64,
    now_ms: u64,
) -> ClientOrderId {
    ClientOrderId::from(format!(
        "{prefix}:{instrument_id}:{leg_name}:{:.0}:{:.0}:{now_ms}",
        price * 10_000.0,
        quantity * 10_000.0,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::market_making::paired_mm::types::PairedInventorySnapshot;
    use crate::markets::BinaryOutcomeMarket;
    use crate::signals::{BtcRegimeSnapshot, FairValueEstimate, FairValueModel};
    use crate::types::{BookLevel, MarketId, TradeSide};

    fn quote(bid: f64, ask: f64) -> QuoteSnapshot {
        QuoteSnapshot {
            best_bid: Some(BookLevel::new(bid, 1_000.0)),
            best_ask: Some(BookLevel::new(ask, 1_000.0)),
            bid_levels: vec![BookLevel::new(bid, 1_000.0)],
            ask_levels: vec![BookLevel::new(ask, 1_000.0)],
            depth_observed_at_ms: Some(10),
            last_trade_price: Some(ask),
            taker_buy_qty_60s: 0.0,
            taker_sell_qty_60s: 0.0,
            observed_at_ms: 10,
        }
    }

    fn input(yes_quote: QuoteSnapshot, no_quote: QuoteSnapshot) -> StrategyInput<BinaryOutcomeMarket> {
        let market = BinaryOutcomeMarket::btc_5m(
            MarketId::from("market"),
            InstrumentId::from("yes"),
            InstrumentId::from("no"),
        );
        StrategyInput {
            market,
            snapshot: PairedMarketSnapshot {
                market_id: MarketId::from("market"),
                yes_instrument_id: InstrumentId::from("yes"),
                no_instrument_id: InstrumentId::from("no"),
                yes_quote,
                no_quote,
            },
            inventory: PairedInventorySnapshot {
                free_cash_usd: 50.0,
                equity_usd: 50.0,
                ..Default::default()
            },
            pair_cost: PairCostTracker::default(),
            fair_value: FairValueEstimate {
                p_up: 0.92,
                p_down: 0.08,
                log_moneyness: 0.0,
                sigma_remaining: 0.0,
                time_remaining_s: 120.0,
                model: FairValueModel::BsmBinary,
            },
            btc_regime: BtcRegimeSnapshot {
                realized_vol_5m_bps: Some(10.0),
                ..Default::default()
            },
            now_ms: 10,
        }
    }

    #[test]
    fn buys_only_the_fair_value_cheap_leg_when_pair_cost_allows() {
        let mut strategy = PairCostArbStrategy::new(PairCostArbStrategyConfig {
            min_edge_bps: 35.0,
            ..Default::default()
        });

        let decision = strategy.on_tick(input(quote(0.48, 0.50), quote(0.28, 0.30)));

        assert_eq!(decision.intents().len(), 1);
        assert_eq!(decision.intents()[0].instrument_id, InstrumentId::from("yes"));
        assert_eq!(decision.intents()[0].side, TradeSide::Buy);
        assert_eq!(
            decision.intents()[0].quote_level_tag.as_deref(),
            Some("pair-cost-arb:cheap-leg:yes")
        );
    }

    #[test]
    fn waits_when_projected_pair_cost_breaches_threshold() {
        let mut strategy = PairCostArbStrategy::new(PairCostArbStrategyConfig {
            min_edge_bps: 35.0,
            ..Default::default()
        });

        let decision = strategy.on_tick(input(quote(0.70, 0.72), quote(0.33, 0.35)));

        assert!(decision.intents().is_empty());
    }

    #[test]
    fn caps_new_asymmetric_excess() {
        let mut strategy = PairCostArbStrategy::new(PairCostArbStrategyConfig {
            min_edge_bps: 35.0,
            max_excess_usd: 1.0,
            ..Default::default()
        });

        let decision = strategy.on_tick(input(quote(0.48, 0.50), quote(0.28, 0.30)));

        assert!(decision.intents().is_empty());
    }
}
