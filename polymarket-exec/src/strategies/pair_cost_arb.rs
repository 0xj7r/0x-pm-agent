//! Pair-cost hedged arbitrage strategy adapter.
//!
//! This is the market-agnostic version of the Gabagool-style loop:
//! buy only the cheap leg when fair-value edge and projected pair cost allow,
//! buy the light side when that is the best route back to merge, and emit
//! merge commands for balanced inventory.

use crate::market_making::pairing::capital_recycler::{
    choose_capital_recycle, CapitalRecycleConfig, CapitalRecycleDecision,
};
use crate::market_making::pairing::pair_cost_tracker::{Leg, PairCostTracker};
use crate::market_making::pairing::types::PairedMarketSnapshot;
use crate::markets::MarketDescriptor;
use crate::strategies::traits::{StrategyInput, TradingStrategy};
use crate::types::{
    ClientOrderId, InstrumentId, IntentKind, MergeIntent, OrderIntent, QuoteSnapshot,
    StrategyDecision,
};

#[derive(Clone, Debug, PartialEq)]
pub struct PairCostArbStrategyConfig {
    pub pair_cost_threshold: f64,
    pub high_vol_pair_cost_threshold: f64,
    pub min_merge_usd: f64,
    pub merge_gas_cost_usd: f64,
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
    pub enable_buy_light_side_rebalance: bool,
    pub recycle_min_imbalance_qty: f64,
    pub recycle_min_time_remaining_ms: u64,
    pub recycle_max_light_side_spread: f64,
    pub fractional_kelly: f64,
    pub capital_scale_factor: f64,
    pub maker_safety_ticks: f64,
    pub rescue_enabled: bool,
    pub rescue_late_window_sec: u64,
    pub rescue_min_excess_usd: f64,
    pub rescue_hold_threshold: f64,
    pub rescue_threshold: f64,
    pub rescue_max_fraction: f64,
    pub rescue_rehedge_pair_cost_threshold: f64,
}

impl Default for PairCostArbStrategyConfig {
    fn default() -> Self {
        Self {
            pair_cost_threshold: 0.99,
            high_vol_pair_cost_threshold: 0.97,
            min_merge_usd: 50.0,
            merge_gas_cost_usd: 0.30,
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
            enable_buy_light_side_rebalance: true,
            recycle_min_imbalance_qty: 5.0,
            recycle_min_time_remaining_ms: 90_000,
            recycle_max_light_side_spread: 0.10,
            fractional_kelly: 0.15,
            capital_scale_factor: 1.0,
            maker_safety_ticks: 2.0,
            rescue_enabled: true,
            rescue_late_window_sec: 90,
            rescue_min_excess_usd: 4.0,
            rescue_hold_threshold: 0.0,
            rescue_threshold: -0.04,
            rescue_max_fraction: 0.35,
            rescue_rehedge_pair_cost_threshold: 1.02,
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

    fn merge_intent<M: MarketDescriptor>(&self, input: &StrategyInput<M>) -> Option<MergeIntent> {
        let quantity = input.inventory.yes_qty.min(input.inventory.no_qty).max(0.0);
        if quantity <= 1e-9 {
            return None;
        }
        let expected_cash_usd = quantity;
        if expected_cash_usd + 1e-9 < self.config.min_merge_usd {
            return None;
        }
        let expected_cost_usd = quantity * input.inventory.yes_avg_cost.max(0.0)
            + quantity * input.inventory.no_avg_cost.max(0.0);
        let pair_cost = if quantity > 0.0 {
            expected_cost_usd / quantity
        } else {
            0.0
        };
        Some(MergeIntent {
            command_id: ClientOrderId::from(format!(
                "pair-cost-merge:{}:{:.8}:{}",
                input.snapshot.market_id, quantity, input.now_ms
            )),
            market_id: input.snapshot.market_id.clone(),
            condition_id: None,
            yes_instrument_id: input.snapshot.yes_instrument_id.clone(),
            no_instrument_id: input.snapshot.no_instrument_id.clone(),
            quantity,
            expected_cash_usd,
            expected_cost_usd,
            expected_fee_usd: 0.0,
            expected_gas_usd: self.config.merge_gas_cost_usd.max(0.0),
            reason: format!(
                "pair-cost arb merge paired inventory qty={quantity:.4} pair_cost={pair_cost:.4}"
            ),
            created_at_ms: input.now_ms,
        })
    }

    fn capital_recycle_config(&self, target_pair_cost: f64) -> CapitalRecycleConfig {
        CapitalRecycleConfig {
            pair_cost_target: target_pair_cost,
            min_imbalance_qty: self.config.recycle_min_imbalance_qty.max(0.0),
            max_buy_qty: 10_000.0,
            max_buy_notional_usd: self.config.max_clip_usd.max(self.config.min_clip_usd),
            min_time_remaining_ms: self.config.recycle_min_time_remaining_ms,
            max_light_side_spread: self.config.recycle_max_light_side_spread.max(0.0),
            race_buffer_ticks: self.config.maker_safety_ticks.max(0.0),
        }
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
            Leg::Yes => weighted_avg(
                pair_cost.yes_qty,
                pair_cost.yes_avg_cost,
                buy_qty,
                buy_price,
            ),
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

    fn late_window_rescue_intent<M: MarketDescriptor>(
        &self,
        input: &StrategyInput<M>,
    ) -> Option<(OrderIntent, String)> {
        if !self.config.rescue_enabled {
            return None;
        }
        let remaining_ms = input.market.time_remaining_ms(input.now_ms)?;
        if remaining_ms > self.config.rescue_late_window_sec.saturating_mul(1_000) {
            return None;
        }

        let (leg, excess_qty, avg_cost, fair_win_prob, quote, instrument_id) =
            if input.inventory.yes_qty > input.inventory.no_qty {
                (
                    Leg::Yes,
                    input.inventory.yes_qty - input.inventory.no_qty,
                    input.inventory.yes_avg_cost,
                    input.fair_value.p_up,
                    &input.snapshot.yes_quote,
                    input.snapshot.yes_instrument_id.clone(),
                )
            } else if input.inventory.no_qty > input.inventory.yes_qty {
                (
                    Leg::No,
                    input.inventory.no_qty - input.inventory.yes_qty,
                    input.inventory.no_avg_cost,
                    input.fair_value.p_down,
                    &input.snapshot.no_quote,
                    input.snapshot.no_instrument_id.clone(),
                )
            } else {
                return None;
            };

        let best_bid = quote.best_bid.as_ref()?.price;
        if !best_bid.is_finite() || best_bid <= 0.0 {
            return None;
        }
        let excess_usd = excess_qty * best_bid;
        if excess_usd + 1e-9 < self.config.rescue_min_excess_usd {
            return None;
        }

        let opposite_ask = match leg {
            Leg::Yes => input.snapshot.no_quote.best_ask.as_ref()?.price,
            Leg::No => input.snapshot.yes_quote.best_ask.as_ref()?.price,
        };
        let rehedge_pair_cost = avg_cost + opposite_ask;
        if rehedge_pair_cost <= self.config.rescue_rehedge_pair_cost_threshold {
            return None;
        }

        let delta_ev = (fair_win_prob.clamp(0.0, 1.0) - best_bid) * excess_qty;
        if delta_ev >= self.config.rescue_hold_threshold {
            return None;
        }
        if delta_ev > self.config.rescue_threshold {
            return None;
        }

        let tick = input.market.tick_size().max(0.001);
        let limit_price = (best_bid - tick * self.config.maker_safety_ticks.max(0.0))
            .clamp(tick, 1.0 - tick);
        let qty = (excess_qty * self.config.rescue_max_fraction.clamp(0.0, 1.0))
            .min(excess_qty)
            .max(0.0);
        if qty * limit_price < self.config.min_clip_usd {
            return None;
        }

        let leg_name = match leg {
            Leg::Yes => "yes",
            Leg::No => "no",
        };
        let mut intent = OrderIntent::new_sell(
            strategy_client_order_id(
                "pair-cost-rescue",
                &instrument_id,
                leg_name,
                limit_price,
                qty,
                input.now_ms,
            ),
            input.snapshot.market_id.clone(),
            instrument_id,
            limit_price,
            qty,
            format!(
                "pair-cost late-window EV rescue sell {leg_name} delta_ev={delta_ev:.4} fair={fair_win_prob:.4} bid={best_bid:.4} rehedge_pair_cost={rehedge_pair_cost:.4}"
            ),
            input.now_ms,
        );
        intent.quote_level_tag = Some(format!("pair-cost-arb:ev-rescue:{leg_name}"));
        intent.kind = IntentKind::Close;
        Some((
            intent,
            format!(
                "pair-cost EV rescue emitted leg={leg:?} qty={qty:.4} delta_ev={delta_ev:.4} rehedge_pair_cost={rehedge_pair_cost:.4}"
            ),
        ))
    }

    fn cheap_leg_intent<M: MarketDescriptor>(
        &self,
        input: &StrategyInput<M>,
        leg: Leg,
        fair_price: f64,
        target_pair_cost: f64,
    ) -> Option<(OrderIntent, f64)> {
        let (instrument_id, quote) = match leg {
            Leg::Yes => (&input.snapshot.yes_instrument_id, &input.snapshot.yes_quote),
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

        let projected_pair_cost =
            self.projected_pair_cost(&input.pair_cost, &input.snapshot, leg, quantity, bid_price)?;
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
        let target = self.effective_pair_cost_target(input.btc_regime.realized_vol_5m_bps);
        let convex_winner = self.active_convex_winner(&input);
        let mut notes = vec![format!(
            "pair-cost arb fair p_up={:.4} p_down={:.4} target={:.4}",
            input.fair_value.p_up, input.fair_value.p_down, target
        )];

        if let Some(intent) = self.merge_intent(&input) {
            notes.push(format!(
                "pair-cost merge emitted qty={:.4} expected_net_gain={:.4}",
                intent.quantity,
                intent.expected_net_gain_usd()
            ));
            return StrategyDecision::Merge { intent, notes };
        }

        if self.is_extreme_vol_paused(input.btc_regime.realized_vol_5m_bps) {
            notes.push("pair-cost arb suppressed: extreme volatility pause".to_string());
            return StrategyDecision::Noop { notes };
        }

        if self.config.enable_buy_light_side_rebalance && convex_winner.is_none() {
            match choose_capital_recycle(
                &input.market,
                &input.snapshot,
                &input.inventory,
                self.capital_recycle_config(target),
                input.now_ms,
            ) {
                CapitalRecycleDecision::BuyLightSide {
                    intent,
                    projected_pair_cost,
                    reason,
                    ..
                } => {
                    notes.push(reason);
                    notes.push(format!(
                        "pair-cost recycle emitted projected_pair_cost={projected_pair_cost:.4}"
                    ));
                    return StrategyDecision::capital_recycle(vec![intent], notes);
                }
                CapitalRecycleDecision::Wait { reason } => notes.push(reason),
            }
        } else if convex_winner.is_some() {
            notes.push(
                "pair-cost recycle skipped: late-window convex winner excess is active".to_string(),
            );
        }

        if let Some((intent, reason)) = self.late_window_rescue_intent(&input) {
            notes.push(reason);
            return StrategyDecision::rescue(vec![intent], notes);
        }

        let mut candidates = Vec::new();
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
    use crate::market_making::pairing::types::PairedInventorySnapshot;
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

    fn input(
        yes_quote: QuoteSnapshot,
        no_quote: QuoteSnapshot,
    ) -> StrategyInput<BinaryOutcomeMarket> {
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
        assert_eq!(
            decision.intents()[0].instrument_id,
            InstrumentId::from("yes")
        );
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

    #[test]
    fn late_window_ev_rescue_sells_fraction_of_bad_excess() {
        let mut strategy = PairCostArbStrategy::new(PairCostArbStrategyConfig {
            rescue_enabled: true,
            rescue_late_window_sec: 90,
            rescue_min_excess_usd: 2.0,
            rescue_threshold: -0.04,
            rescue_rehedge_pair_cost_threshold: 1.02,
            min_edge_bps: 35.0,
            ..Default::default()
        });
        let mut input = input(quote(0.30, 0.32), quote(0.82, 0.84));
        input.market.event_end_ms = Some(80_000);
        input.now_ms = 10;
        input.inventory.yes_qty = 20.0;
        input.inventory.no_qty = 5.0;
        input.inventory.yes_avg_cost = 0.60;
        input.inventory.no_avg_cost = 0.35;
        input.fair_value.p_up = 0.05;
        input.fair_value.p_down = 0.95;

        let decision = strategy.on_tick(input);

        assert_eq!(decision.intents().len(), 1);
        assert_eq!(decision.intents()[0].side, TradeSide::Sell);
        assert_eq!(decision.intents()[0].kind, IntentKind::Close);
        assert_eq!(
            decision.intents()[0].quote_level_tag.as_deref(),
            Some("pair-cost-arb:ev-rescue:yes")
        );
    }
}
