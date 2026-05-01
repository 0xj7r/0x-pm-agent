//! Strategy registry for multi-market / multi-strategy execution.

use std::collections::BTreeMap;

use crate::markets::MarketDescriptor;
use crate::strategies::paired_mm::{PairedMmStrategy, PairedMmStrategyConfig};
use crate::strategies::traits::{StrategyFillInput, StrategyInput, TradingStrategy};
use crate::types::StrategyDecision;

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct StrategyKey {
    pub market_id: String,
    pub strategy_name: String,
}

impl StrategyKey {
    pub fn new(market_id: impl Into<String>, strategy_name: impl Into<String>) -> Self {
        Self {
            market_id: market_id.into(),
            strategy_name: strategy_name.into(),
        }
    }
}

#[derive(Clone, Debug)]
pub enum RegisteredStrategy {
    PairedMm(PairedMmStrategy),
}

impl RegisteredStrategy {
    pub fn name(&self) -> &'static str {
        match self {
            Self::PairedMm(strategy) => <PairedMmStrategy as TradingStrategy<
                crate::markets::BinaryOutcomeMarket,
            >>::name(strategy),
        }
    }

    pub fn on_tick<M>(&mut self, input: StrategyInput<M>) -> StrategyDecision
    where
        M: MarketDescriptor,
    {
        match self {
            Self::PairedMm(strategy) => strategy.on_tick(input),
        }
    }

    pub fn on_fill<M>(&mut self, input: StrategyFillInput<M>) -> StrategyDecision
    where
        M: MarketDescriptor,
    {
        match self {
            Self::PairedMm(strategy) => strategy.on_fill(input),
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct StrategyRegistry {
    strategies: BTreeMap<StrategyKey, RegisteredStrategy>,
}

impl StrategyRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register_paired_mm(
        &mut self,
        market_id: impl Into<String>,
        config: PairedMmStrategyConfig,
    ) {
        let key = StrategyKey::new(market_id, "paired_mm");
        self.strategies.insert(
            key,
            RegisteredStrategy::PairedMm(PairedMmStrategy::new(config)),
        );
    }

    pub fn paired_mm_mut(&mut self, market_id: &str) -> Option<&mut PairedMmStrategy> {
        self.strategies
            .get_mut(&StrategyKey::new(market_id, "paired_mm"))
            .and_then(|strategy| match strategy {
                RegisteredStrategy::PairedMm(strategy) => Some(strategy),
            })
    }

    pub fn strategy_mut(
        &mut self,
        market_id: &str,
        strategy_name: &str,
    ) -> Option<&mut RegisteredStrategy> {
        self.strategies
            .get_mut(&StrategyKey::new(market_id, strategy_name))
    }

    pub fn on_tick<M>(&mut self, market_id: &str, input: StrategyInput<M>) -> Vec<StrategyDecision>
    where
        M: MarketDescriptor + Clone,
    {
        self.strategies
            .iter_mut()
            .filter(|(key, _)| key.market_id == market_id)
            .map(|(_, strategy)| strategy.on_tick(input.clone()))
            .collect()
    }

    pub fn on_fill<M>(
        &mut self,
        market_id: &str,
        input: StrategyFillInput<M>,
    ) -> Vec<StrategyDecision>
    where
        M: MarketDescriptor + Clone,
    {
        self.strategies
            .iter_mut()
            .filter(|(key, _)| key.market_id == market_id)
            .map(|(_, strategy)| strategy.on_fill(input.clone()))
            .collect()
    }

    pub fn len(&self) -> usize {
        self.strategies.len()
    }

    pub fn is_empty(&self) -> bool {
        self.strategies.is_empty()
    }
}
