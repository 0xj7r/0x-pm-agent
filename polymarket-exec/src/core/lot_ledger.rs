//! FIFO lot ledger for per-fill cost basis.

use std::collections::VecDeque;

use crate::types::EpochMillis;

const EPSILON_QTY: f64 = 1e-9;

#[derive(Clone, Debug, PartialEq)]
pub struct Lot {
    pub quantity: f64,
    pub price: f64,
    pub acquired_at_ms: EpochMillis,
}

impl Lot {
    pub fn new(quantity: f64, price: f64, acquired_at_ms: EpochMillis) -> Self {
        Self {
            quantity,
            price,
            acquired_at_ms,
        }
    }

    pub fn cost_usd(&self) -> f64 {
        self.quantity.max(0.0) * self.price.max(0.0)
    }
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct LotLedger {
    lots: VecDeque<Lot>,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct ConsumedLots {
    pub quantity: f64,
    pub cost_usd: f64,
}

impl ConsumedLots {
    pub fn avg_cost(&self) -> Option<f64> {
        (self.quantity > EPSILON_QTY).then_some(self.cost_usd / self.quantity)
    }
}

impl LotLedger {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn push_buy(&mut self, quantity: f64, price: f64, acquired_at_ms: EpochMillis) {
        if quantity <= EPSILON_QTY || price <= 0.0 || !quantity.is_finite() || !price.is_finite() {
            return;
        }
        self.lots
            .push_back(Lot::new(quantity, price, acquired_at_ms));
    }

    pub fn consume_fifo(&mut self, quantity: f64) -> Option<ConsumedLots> {
        if quantity <= EPSILON_QTY {
            return Some(ConsumedLots::default());
        }
        if self.quantity() + EPSILON_QTY < quantity {
            return None;
        }

        let mut remaining = quantity;
        let mut consumed = ConsumedLots::default();
        while remaining > EPSILON_QTY {
            let mut lot = self.lots.pop_front()?;
            let take = lot.quantity.min(remaining);
            consumed.quantity += take;
            consumed.cost_usd += take * lot.price;
            lot.quantity -= take;
            remaining -= take;
            if lot.quantity > EPSILON_QTY {
                self.lots.push_front(lot);
            }
        }
        Some(consumed)
    }

    pub fn quantity(&self) -> f64 {
        self.lots.iter().map(|lot| lot.quantity.max(0.0)).sum()
    }

    pub fn cost_usd(&self) -> f64 {
        self.lots.iter().map(Lot::cost_usd).sum()
    }

    pub fn avg_cost(&self) -> Option<f64> {
        let quantity = self.quantity();
        (quantity > EPSILON_QTY).then_some(self.cost_usd() / quantity)
    }

    pub fn lots(&self) -> impl Iterator<Item = &Lot> {
        self.lots.iter()
    }
}
