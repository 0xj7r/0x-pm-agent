//! Pair-cost tracker for convex paired-MM accounting.

use crate::market_making::pairing::types::{LadderLeg, PairedInventorySnapshot};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Leg {
    Yes,
    No,
}

impl From<LadderLeg> for Leg {
    fn from(value: LadderLeg) -> Self {
        match value {
            LadderLeg::Yes => Self::Yes,
            LadderLeg::No => Self::No,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct PairCostTracker {
    pub yes_qty: f64,
    pub yes_avg_cost: f64,
    pub no_qty: f64,
    pub no_avg_cost: f64,
}

impl PairCostTracker {
    pub fn from_inventory(inventory: &PairedInventorySnapshot) -> Self {
        Self {
            yes_qty: inventory.yes_qty.max(0.0),
            yes_avg_cost: inventory.yes_avg_cost.max(0.0),
            no_qty: inventory.no_qty.max(0.0),
            no_avg_cost: inventory.no_avg_cost.max(0.0),
        }
    }

    pub fn record_buy(&mut self, leg: Leg, qty: f64, price: f64) {
        if qty <= 0.0 || price <= 0.0 || !qty.is_finite() || !price.is_finite() {
            return;
        }
        match leg {
            Leg::Yes => {
                let cost = self.yes_qty * self.yes_avg_cost + qty * price;
                self.yes_qty += qty;
                self.yes_avg_cost = cost / self.yes_qty.max(1e-9);
            }
            Leg::No => {
                let cost = self.no_qty * self.no_avg_cost + qty * price;
                self.no_qty += qty;
                self.no_avg_cost = cost / self.no_qty.max(1e-9);
            }
        }
    }

    pub fn pair_cost(&self) -> Option<f64> {
        if self.yes_qty > 0.0 && self.no_qty > 0.0 {
            Some(self.yes_avg_cost + self.no_avg_cost)
        } else {
            None
        }
    }

    pub fn paired_qty(&self) -> f64 {
        self.yes_qty.min(self.no_qty).max(0.0)
    }

    pub fn imbalance_ratio(&self) -> f64 {
        let denom = (self.yes_qty + self.no_qty).max(1e-9);
        ((self.yes_qty - self.no_qty).abs() / denom).clamp(0.0, 1.0)
    }

    pub fn cheap_leg(&self) -> Option<Leg> {
        if self.yes_avg_cost <= 0.0 || self.no_avg_cost <= 0.0 {
            return None;
        }
        if self.yes_avg_cost <= self.no_avg_cost {
            Some(Leg::Yes)
        } else {
            Some(Leg::No)
        }
    }
}
