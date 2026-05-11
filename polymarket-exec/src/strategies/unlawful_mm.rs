//! First-class `unlawful_mm` strategy entrypoint.
//!
//! This strategy is implemented in one file and keeps `unlawful`/`unlawful_mm`
//! runtime wiring explicit while delegating execution to the canonical
//! `CoreHedgeMmStrategy`.
//!
//! The strategy config is expected to encode pre-pivot merge gating and
//! post-pivot redeem-only behavior from Canonical.md.

use crate::markets::MarketDescriptor;
use crate::strategies::core_hedge_mm::{
    CoreHedgeMmStrategy, CoreHedgeMmStrategyConfig,
};
use crate::strategies::traits::{StrategyFillInput, StrategyInput, TradingStrategy};
use crate::types::StrategyDecision;

pub type UnlawfulMmStrategyConfig = CoreHedgeMmStrategyConfig;

#[derive(Clone, Debug)]
pub struct UnlawfulMmStrategy {
    core_hedge_mm: CoreHedgeMmStrategy,
}

impl UnlawfulMmStrategy {
    pub fn new(config: UnlawfulMmStrategyConfig) -> Self {
        Self {
            core_hedge_mm: CoreHedgeMmStrategy::new(config),
        }
    }

    pub fn config(&self) -> &UnlawfulMmStrategyConfig {
        self.core_hedge_mm.config()
    }
}

impl<M: MarketDescriptor> TradingStrategy<M> for UnlawfulMmStrategy {
    fn name(&self) -> &'static str {
        "unlawful_mm"
    }

    fn on_tick(&mut self, input: StrategyInput<M>) -> StrategyDecision {
        self.core_hedge_mm.on_tick(input)
    }

    fn on_fill(&mut self, input: StrategyFillInput<M>) -> StrategyDecision {
        self.core_hedge_mm.on_fill(input)
    }
}
