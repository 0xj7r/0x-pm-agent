//! Shared types for the paired-MM algorithm seam.

use crate::types::{InstrumentId, MarketId, QuoteSnapshot};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum LadderLeg {
    Yes,
    No,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LadderRegime {
    LowVolOscillating,
    Normal,
    DirectionalDefensive,
    LateBar,
    InventoryImbalanced,
}

#[derive(Clone, Debug, PartialEq)]
pub struct PairedMarketSnapshot {
    pub market_id: MarketId,
    pub yes_instrument_id: InstrumentId,
    pub no_instrument_id: InstrumentId,
    pub yes_quote: QuoteSnapshot,
    pub no_quote: QuoteSnapshot,
}

impl PairedMarketSnapshot {
    pub fn total_visible_levels(&self) -> usize {
        self.yes_quote.bid_levels.len()
            + self.yes_quote.ask_levels.len()
            + self.no_quote.bid_levels.len()
            + self.no_quote.ask_levels.len()
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct PairedInventorySnapshot {
    pub yes_qty: f64,
    pub no_qty: f64,
    pub yes_avg_cost: f64,
    pub no_avg_cost: f64,
    pub free_cash_usd: f64,
    pub equity_usd: f64,
}

impl PairedInventorySnapshot {
    pub fn side_imbalance_qty(&self) -> f64 {
        (self.yes_qty - self.no_qty).abs()
    }

    pub fn gross_cost_usd(&self) -> f64 {
        (self.yes_qty.max(0.0) * self.yes_avg_cost.max(0.0))
            + (self.no_qty.max(0.0) * self.no_avg_cost.max(0.0))
    }

    pub fn imbalance_ratio(&self) -> f64 {
        let denom = (self.yes_qty.abs() + self.no_qty.abs()).max(1e-9);
        ((self.yes_qty - self.no_qty).abs() / denom).clamp(0.0, 1.0)
    }

    pub fn net_for_leg(&self, leg: LadderLeg) -> f64 {
        match leg {
            LadderLeg::Yes => self.yes_qty,
            LadderLeg::No => self.no_qty,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct RunningInventoryCaps {
    pub max_gross_cost_usd: f64,
    pub max_side_imbalance_qty: f64,
    pub max_entry_notional_usd: f64,
}

impl Default for RunningInventoryCaps {
    fn default() -> Self {
        Self {
            max_gross_cost_usd: 150.0,
            max_side_imbalance_qty: 150.0,
            max_entry_notional_usd: 150.0,
        }
    }
}
