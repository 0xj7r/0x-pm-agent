//! Position, cash, and exposure accounting with reservation and fill application helpers.

use std::collections::{BTreeMap, HashMap};
use std::error::Error;
use std::fmt;

use crate::event_log::{EventCategory, EventMetrics, EventRecord};
use crate::types::{
    ClientOrderId, EpochMillis, FillReport, InstrumentId, MarketId, OrderIntent, TradeSide,
};

#[derive(Clone, Debug, PartialEq)]
pub struct PositionState {
    pub market_id: MarketId,
    pub instrument_id: InstrumentId,
    pub quantity: f64,
    pub avg_price: f64,
    pub mark_price: Option<f64>,
    pub updated_at_ms: EpochMillis,
}

impl PositionState {
    pub fn mark_or_cost(&self) -> f64 {
        self.mark_price.unwrap_or(self.avg_price)
    }

    pub fn gross_notional_usd(&self) -> f64 {
        self.quantity.abs() * self.mark_or_cost()
    }

    pub fn net_notional_usd(&self) -> f64 {
        self.quantity * self.mark_or_cost()
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct CashReservation {
    pub client_order_id: ClientOrderId,
    pub market_id: MarketId,
    pub instrument_id: InstrumentId,
    pub amount_usd: f64,
    pub created_at_ms: EpochMillis,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InventoryAdjustmentReason {
    Reserved,
    Released,
    FillApplied,
    MergeApplied,
    MarkUpdated,
}

#[derive(Clone, Debug, PartialEq)]
pub struct InventoryAdjustment {
    pub reason: InventoryAdjustmentReason,
    pub observed_at_ms: EpochMillis,
    pub market_id: Option<MarketId>,
    pub instrument_id: Option<InstrumentId>,
    pub client_order_id: Option<ClientOrderId>,
    pub cash_delta_usd: f64,
    pub reserved_cash_delta_usd: f64,
    pub position_delta: f64,
    pub realized_pnl_delta_usd: f64,
    pub free_cash_after_usd: f64,
    pub reserved_cash_after_usd: f64,
    pub gross_exposure_after_usd: f64,
}

impl InventoryAdjustment {
    pub fn to_event(&self, message: impl Into<String>) -> EventRecord {
        let metrics = EventMetrics {
            price: None,
            quantity: Some(self.position_delta.abs()),
            notional_usd: None,
            cash_delta_usd: Some(self.cash_delta_usd),
            position_delta: Some(self.position_delta),
            free_cash_after_usd: Some(self.free_cash_after_usd),
            gross_exposure_after_usd: Some(self.gross_exposure_after_usd),
            risk_reject_reason: None,
        };
        let mut record = EventRecord::new(EventCategory::Inventory, self.observed_at_ms, message)
            .with_metrics(metrics);
        if let Some(market_id) = &self.market_id {
            record = record.with_market(market_id.clone());
        }
        if let Some(instrument_id) = &self.instrument_id {
            record = record.with_instrument(instrument_id.clone());
        }
        if let Some(client_order_id) = &self.client_order_id {
            record = record.with_client_order(client_order_id.clone());
        }
        record
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct InventorySnapshot {
    pub free_cash_usd: f64,
    pub reserved_cash_usd: f64,
    pub total_cash_usd: f64,
    pub realized_pnl_usd: f64,
    pub gross_exposure_usd: f64,
    pub positions: Vec<PositionState>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct VenuePositionSnapshot {
    pub market_id: MarketId,
    pub condition_id: Option<String>,
    pub instrument_id: InstrumentId,
    pub quantity: f64,
    pub average_cost_usd: f64,
    pub mark_price: Option<f64>,
    pub observed_at_ms: EpochMillis,
}

#[derive(Clone, Debug, PartialEq)]
pub struct InventoryPositionReconciliation {
    pub market_id: MarketId,
    pub instrument_id: InstrumentId,
    pub local_quantity_before: f64,
    pub venue_quantity: f64,
    pub quantity_delta: f64,
    pub local_avg_price_before: Option<f64>,
    pub venue_avg_price: f64,
}

#[derive(Clone, Debug, PartialEq)]
pub struct StrandedMarketInventory {
    pub market_id: MarketId,
    pub paired_quantity: f64,
    pub stranded_positions: Vec<PositionState>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct InventoryReconciliationReport {
    pub observed_at_ms: EpochMillis,
    pub position_count: usize,
    pub deltas: Vec<InventoryPositionReconciliation>,
    pub stranded_markets: Vec<StrandedMarketInventory>,
    pub gross_exposure_after_usd: f64,
}

#[derive(Clone, Debug, PartialEq)]
pub enum InventoryError {
    InsufficientFreeCash {
        required_usd: f64,
        available_usd: f64,
    },
    DuplicateReservation(ClientOrderId),
    Oversell {
        instrument_id: InstrumentId,
        available_qty: f64,
        attempted_qty: f64,
    },
    InvalidFill(&'static str),
}

impl fmt::Display for InventoryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InsufficientFreeCash {
                required_usd,
                available_usd,
            } => write!(
                f,
                "insufficient free cash: required={required_usd:.4} available={available_usd:.4}"
            ),
            Self::DuplicateReservation(client_order_id) => {
                write!(f, "duplicate reservation for client_order_id={client_order_id}")
            }
            Self::Oversell {
                instrument_id,
                available_qty,
                attempted_qty,
            } => write!(
                f,
                "oversell on instrument={instrument_id}: available={available_qty:.6} attempted={attempted_qty:.6}"
            ),
            Self::InvalidFill(message) => write!(f, "invalid fill: {message}"),
        }
    }
}

impl Error for InventoryError {}

#[derive(Clone, Debug)]
pub struct InventoryState {
    free_cash_usd: f64,
    reserved_cash_usd: f64,
    realized_pnl_usd: f64,
    positions: HashMap<InstrumentId, PositionState>,
    reservations: HashMap<ClientOrderId, CashReservation>,
}

impl InventoryState {
    pub fn new(starting_cash_usd: f64) -> Self {
        Self {
            free_cash_usd: starting_cash_usd,
            reserved_cash_usd: 0.0,
            realized_pnl_usd: 0.0,
            positions: HashMap::new(),
            reservations: HashMap::new(),
        }
    }

    pub fn free_cash_usd(&self) -> f64 {
        self.free_cash_usd
    }

    pub fn reserved_cash_usd(&self) -> f64 {
        self.reserved_cash_usd
    }

    pub fn total_cash_usd(&self) -> f64 {
        self.free_cash_usd + self.reserved_cash_usd
    }

    pub fn realized_pnl_usd(&self) -> f64 {
        self.realized_pnl_usd
    }

    pub fn positions(&self) -> impl Iterator<Item = &PositionState> {
        self.positions.values()
    }

    pub fn position(&self, instrument_id: &InstrumentId) -> Option<&PositionState> {
        self.positions.get(instrument_id)
    }

    pub fn position_qty(&self, instrument_id: &InstrumentId) -> f64 {
        self.positions
            .get(instrument_id)
            .map(|position| position.quantity)
            .unwrap_or(0.0)
    }

    pub fn gross_exposure_usd(&self) -> f64 {
        self.positions
            .values()
            .map(PositionState::gross_notional_usd)
            .sum()
    }

    pub fn net_exposure_for_market_usd(&self, market_id: &MarketId) -> f64 {
        self.positions
            .values()
            .filter(|position| &position.market_id == market_id)
            .map(PositionState::net_notional_usd)
            .sum()
    }

    pub fn snapshot(&self) -> InventorySnapshot {
        let mut positions = self.positions.values().cloned().collect::<Vec<_>>();
        positions.sort_by(|left, right| left.instrument_id.cmp(&right.instrument_id));
        InventorySnapshot {
            free_cash_usd: self.free_cash_usd,
            reserved_cash_usd: self.reserved_cash_usd,
            total_cash_usd: self.total_cash_usd(),
            realized_pnl_usd: self.realized_pnl_usd,
            gross_exposure_usd: self.gross_exposure_usd(),
            positions,
        }
    }

    pub fn reconcile_venue_positions(
        &mut self,
        venue_positions: &[VenuePositionSnapshot],
        observed_at_ms: EpochMillis,
    ) -> Result<InventoryReconciliationReport, InventoryError> {
        let mut deltas = Vec::new();

        tracing::debug!(
            target: "inventory.reconcile",
            venue_position_count = venue_positions.len(),
            local_position_count = self.positions.len(),
            "reconcile_venue_positions begin"
        );

        for venue_position in venue_positions {
            if !venue_position.quantity.is_finite()
                || !venue_position.average_cost_usd.is_finite()
                || venue_position.quantity < -1e-9
                || venue_position.average_cost_usd < 0.0
                || venue_position
                    .mark_price
                    .is_some_and(|mark| !mark.is_finite() || mark < 0.0)
            {
                return Err(InventoryError::InvalidFill(
                    "invalid venue position snapshot",
                ));
            }

            let local = self.positions.get(&venue_position.instrument_id).cloned();
            let local_quantity_before = local.as_ref().map_or(0.0, |position| position.quantity);
            let local_avg_price_before = local.as_ref().map(|position| position.avg_price);
            let venue_quantity = venue_position.quantity.max(0.0);
            let quantity_delta = venue_quantity - local_quantity_before;

            if quantity_delta.abs() <= 1e-9 {
                if let Some(existing) = self.positions.get_mut(&venue_position.instrument_id) {
                    existing.market_id = venue_position.market_id.clone();
                    existing.avg_price = venue_position.average_cost_usd;
                    existing.mark_price = venue_position.mark_price;
                    existing.updated_at_ms = observed_at_ms;
                } else if venue_quantity > 1e-9 {
                    // Bug fix: previously this branch was a no-op. If local
                    // position doesn't exist AND venue says we have nothing
                    // (delta=0 because both 0), that's fine. But if local
                    // doesn't exist AND venue says we have N>0 (which would
                    // produce delta=N, not 0), we'd hit the insert branch
                    // below. So reaching here means delta < 1e-9 and local
                    // is None and venue_quantity > 0 — which means venue is
                    // reporting a position that's vanishingly small relative
                    // to itself somehow. INSERT IT anyway so the merge
                    // planner can see it.
                    tracing::warn!(
                        target: "inventory.reconcile",
                        instrument_id = %venue_position.instrument_id,
                        venue_quantity,
                        "venue position with delta~0 but no local — forcing insert"
                    );
                    self.positions.insert(
                        venue_position.instrument_id.clone(),
                        PositionState {
                            market_id: venue_position.market_id.clone(),
                            instrument_id: venue_position.instrument_id.clone(),
                            quantity: venue_quantity,
                            avg_price: venue_position.average_cost_usd,
                            mark_price: venue_position.mark_price,
                            updated_at_ms: observed_at_ms,
                        },
                    );
                }
                continue;
            }

            if venue_quantity <= 1e-9 {
                self.positions.remove(&venue_position.instrument_id);
            } else {
                self.positions.insert(
                    venue_position.instrument_id.clone(),
                    PositionState {
                        market_id: venue_position.market_id.clone(),
                        instrument_id: venue_position.instrument_id.clone(),
                        quantity: venue_quantity,
                        avg_price: venue_position.average_cost_usd,
                        mark_price: venue_position.mark_price,
                        updated_at_ms: observed_at_ms,
                    },
                );
            }

            deltas.push(InventoryPositionReconciliation {
                market_id: venue_position.market_id.clone(),
                instrument_id: venue_position.instrument_id.clone(),
                local_quantity_before,
                venue_quantity,
                quantity_delta,
                local_avg_price_before,
                venue_avg_price: venue_position.average_cost_usd,
            });
        }

        Ok(InventoryReconciliationReport {
            observed_at_ms,
            position_count: self.positions.len(),
            deltas,
            stranded_markets: self.stranded_market_inventory(),
            gross_exposure_after_usd: self.gross_exposure_usd(),
        })
    }

    pub fn stranded_market_inventory(&self) -> Vec<StrandedMarketInventory> {
        let mut by_market = BTreeMap::<MarketId, Vec<PositionState>>::new();
        for position in self.positions.values() {
            if position.quantity > 1e-9 {
                by_market
                    .entry(position.market_id.clone())
                    .or_default()
                    .push(position.clone());
            }
        }

        by_market
            .into_iter()
            .filter_map(|(market_id, mut positions)| {
                positions.sort_by(|left, right| left.instrument_id.cmp(&right.instrument_id));
                let paired_quantity = if positions.len() >= 2 {
                    positions
                        .iter()
                        .map(|position| position.quantity)
                        .fold(f64::INFINITY, f64::min)
                } else {
                    0.0
                };
                let paired_quantity = if paired_quantity.is_finite() && paired_quantity > 1e-9 {
                    paired_quantity
                } else {
                    0.0
                };
                let stranded_positions = positions
                    .into_iter()
                    .filter_map(|mut position| {
                        let stranded_qty = position.quantity - paired_quantity;
                        if stranded_qty > 1e-9 {
                            position.quantity = stranded_qty;
                            Some(position)
                        } else {
                            None
                        }
                    })
                    .collect::<Vec<_>>();

                (!stranded_positions.is_empty()).then_some(StrandedMarketInventory {
                    market_id,
                    paired_quantity,
                    stranded_positions,
                })
            })
            .collect()
    }

    pub fn reserve_for_order(
        &mut self,
        order: &OrderIntent,
    ) -> Result<InventoryAdjustment, InventoryError> {
        if order.side == TradeSide::Sell || order.reduce_only {
            return Ok(self.noop_adjustment(
                InventoryAdjustmentReason::Reserved,
                order.created_at_ms,
                Some(order.market_id.clone()),
                Some(order.instrument_id.clone()),
                Some(order.client_order_id.clone()),
            ));
        }
        if self.reservations.contains_key(&order.client_order_id) {
            return Err(InventoryError::DuplicateReservation(
                order.client_order_id.clone(),
            ));
        }
        let amount_usd = order.notional_usd();
        if self.free_cash_usd + 1e-9 < amount_usd {
            return Err(InventoryError::InsufficientFreeCash {
                required_usd: amount_usd,
                available_usd: self.free_cash_usd,
            });
        }

        self.free_cash_usd -= amount_usd;
        self.reserved_cash_usd += amount_usd;
        self.reservations.insert(
            order.client_order_id.clone(),
            CashReservation {
                client_order_id: order.client_order_id.clone(),
                market_id: order.market_id.clone(),
                instrument_id: order.instrument_id.clone(),
                amount_usd,
                created_at_ms: order.created_at_ms,
            },
        );

        Ok(InventoryAdjustment {
            reason: InventoryAdjustmentReason::Reserved,
            observed_at_ms: order.created_at_ms,
            market_id: Some(order.market_id.clone()),
            instrument_id: Some(order.instrument_id.clone()),
            client_order_id: Some(order.client_order_id.clone()),
            cash_delta_usd: -amount_usd,
            reserved_cash_delta_usd: amount_usd,
            position_delta: 0.0,
            realized_pnl_delta_usd: 0.0,
            free_cash_after_usd: self.free_cash_usd,
            reserved_cash_after_usd: self.reserved_cash_usd,
            gross_exposure_after_usd: self.gross_exposure_usd(),
        })
    }

    pub fn release_reservation(
        &mut self,
        client_order_id: &ClientOrderId,
        observed_at_ms: EpochMillis,
    ) -> Option<InventoryAdjustment> {
        let reservation = self.reservations.remove(client_order_id)?;
        self.reserved_cash_usd -= reservation.amount_usd;
        self.free_cash_usd += reservation.amount_usd;
        Some(InventoryAdjustment {
            reason: InventoryAdjustmentReason::Released,
            observed_at_ms,
            market_id: Some(reservation.market_id),
            instrument_id: Some(reservation.instrument_id),
            client_order_id: Some(reservation.client_order_id),
            cash_delta_usd: reservation.amount_usd,
            reserved_cash_delta_usd: -reservation.amount_usd,
            position_delta: 0.0,
            realized_pnl_delta_usd: 0.0,
            free_cash_after_usd: self.free_cash_usd,
            reserved_cash_after_usd: self.reserved_cash_usd,
            gross_exposure_after_usd: self.gross_exposure_usd(),
        })
    }

    pub fn mark_price(
        &mut self,
        market_id: &MarketId,
        instrument_id: &InstrumentId,
        price: f64,
        observed_at_ms: EpochMillis,
    ) -> InventoryAdjustment {
        if let Some(entry) = self.positions.get_mut(instrument_id) {
            entry.market_id = market_id.clone();
            entry.mark_price = Some(price);
            entry.updated_at_ms = observed_at_ms;
        }

        InventoryAdjustment {
            reason: InventoryAdjustmentReason::MarkUpdated,
            observed_at_ms,
            market_id: Some(market_id.clone()),
            instrument_id: Some(instrument_id.clone()),
            client_order_id: None,
            cash_delta_usd: 0.0,
            reserved_cash_delta_usd: 0.0,
            position_delta: 0.0,
            realized_pnl_delta_usd: 0.0,
            free_cash_after_usd: self.free_cash_usd,
            reserved_cash_after_usd: self.reserved_cash_usd,
            gross_exposure_after_usd: self.gross_exposure_usd(),
        }
    }

    pub fn apply_fill(&mut self, fill: &FillReport) -> Result<InventoryAdjustment, InventoryError> {
        if fill.price <= 0.0 || fill.quantity <= 0.0 {
            return Err(InventoryError::InvalidFill(
                "price and quantity must be positive",
            ));
        }

        let client_order_id = fill.client_order_id.clone();
        let notional_usd = fill.notional_usd();
        let cash_delta_usd;
        let mut realized_pnl_delta_usd = 0.0;
        let position_delta = fill.quantity * fill.side.sign();
        let reservation_amount = client_order_id
            .as_ref()
            .and_then(|client_order_id| self.reservations.get(client_order_id))
            .map(|reservation| reservation.amount_usd)
            .unwrap_or(0.0);

        match fill.side {
            TradeSide::Buy => {
                let total_cost = notional_usd + fill.fee_usd;
                if self.free_cash_usd + reservation_amount + 1e-9 < total_cost {
                    return Err(InventoryError::InsufficientFreeCash {
                        required_usd: total_cost,
                        available_usd: self.free_cash_usd + reservation_amount,
                    });
                }
                if let Some(client_order_id) = &client_order_id {
                    if let Some(reservation) = self.reservations.remove(client_order_id) {
                        self.reserved_cash_usd -= reservation.amount_usd;
                        self.free_cash_usd += reservation.amount_usd;
                    }
                }
                self.free_cash_usd -= total_cost;
                cash_delta_usd = -total_cost;

                let entry = self
                    .positions
                    .entry(fill.instrument_id.clone())
                    .or_insert_with(|| PositionState {
                        market_id: fill.market_id.clone(),
                        instrument_id: fill.instrument_id.clone(),
                        quantity: 0.0,
                        avg_price: fill.price,
                        mark_price: Some(fill.price),
                        updated_at_ms: fill.observed_at_ms,
                    });
                let prior_qty = entry.quantity;
                let next_qty = prior_qty + fill.quantity;
                entry.avg_price = if next_qty <= f64::EPSILON {
                    fill.price
                } else {
                    ((entry.avg_price * prior_qty) + notional_usd) / next_qty
                };
                entry.quantity = next_qty;
                entry.mark_price = Some(fill.price);
                entry.updated_at_ms = fill.observed_at_ms;
            }
            TradeSide::Sell => {
                let remove_after;
                {
                    let entry = self.positions.get_mut(&fill.instrument_id).ok_or_else(|| {
                        InventoryError::Oversell {
                            instrument_id: fill.instrument_id.clone(),
                            available_qty: 0.0,
                            attempted_qty: fill.quantity,
                        }
                    })?;
                    if entry.quantity + 1e-9 < fill.quantity {
                        return Err(InventoryError::Oversell {
                            instrument_id: fill.instrument_id.clone(),
                            available_qty: entry.quantity,
                            attempted_qty: fill.quantity,
                        });
                    }
                    if let Some(client_order_id) = &client_order_id {
                        if let Some(reservation) = self.reservations.remove(client_order_id) {
                            self.reserved_cash_usd -= reservation.amount_usd;
                            self.free_cash_usd += reservation.amount_usd;
                        }
                    }
                    let proceeds = notional_usd - fill.fee_usd;
                    self.free_cash_usd += proceeds;
                    cash_delta_usd = proceeds;
                    realized_pnl_delta_usd =
                        (fill.price - entry.avg_price) * fill.quantity - fill.fee_usd;
                    self.realized_pnl_usd += realized_pnl_delta_usd;
                    entry.quantity -= fill.quantity;
                    entry.mark_price = Some(fill.price);
                    entry.updated_at_ms = fill.observed_at_ms;
                    remove_after = entry.quantity.abs() <= 1e-9;
                }
                if remove_after {
                    self.positions.remove(&fill.instrument_id);
                }
            }
        }

        Ok(InventoryAdjustment {
            reason: InventoryAdjustmentReason::FillApplied,
            observed_at_ms: fill.observed_at_ms,
            market_id: Some(fill.market_id.clone()),
            instrument_id: Some(fill.instrument_id.clone()),
            client_order_id,
            cash_delta_usd,
            reserved_cash_delta_usd: 0.0,
            position_delta,
            realized_pnl_delta_usd,
            free_cash_after_usd: self.free_cash_usd,
            reserved_cash_after_usd: self.reserved_cash_usd,
            gross_exposure_after_usd: self.gross_exposure_usd(),
        })
    }

    pub fn apply_merge(
        &mut self,
        market_id: &MarketId,
        yes_instrument_id: &InstrumentId,
        no_instrument_id: &InstrumentId,
        merged_qty: f64,
        merged_cost_usd: f64,
        merged_cash_usd: f64,
        additional_fee_usd: f64,
        observed_at_ms: EpochMillis,
    ) -> Result<InventoryAdjustment, InventoryError> {
        if merged_qty <= 0.0 || merged_cash_usd < 0.0 || merged_cost_usd < 0.0 {
            return Err(InventoryError::InvalidFill("invalid merge quantity/cash"));
        }

        let yes_position_qty = self.position_qty(yes_instrument_id);
        if yes_position_qty + 1e-9 < merged_qty {
            return Err(InventoryError::Oversell {
                instrument_id: yes_instrument_id.clone(),
                available_qty: yes_position_qty,
                attempted_qty: merged_qty,
            });
        }

        let no_position_qty = self.position_qty(no_instrument_id);
        if no_position_qty + 1e-9 < merged_qty {
            return Err(InventoryError::Oversell {
                instrument_id: no_instrument_id.clone(),
                available_qty: no_position_qty,
                attempted_qty: merged_qty,
            });
        }

        let net_cash_usd = merged_cash_usd - additional_fee_usd;
        let realized_pnl_delta_usd = net_cash_usd - merged_cost_usd;
        let mut remove_yes = false;
        let mut remove_no = false;

        if let Some(entry) = self.positions.get_mut(yes_instrument_id) {
            if entry.quantity + 1e-9 < merged_qty {
                return Err(InventoryError::Oversell {
                    instrument_id: yes_instrument_id.clone(),
                    available_qty: entry.quantity,
                    attempted_qty: merged_qty,
                });
            }
            entry.quantity -= merged_qty;
            entry.updated_at_ms = observed_at_ms;
            if entry.quantity <= 1e-9 {
                remove_yes = true;
            }
        } else {
            return Err(InventoryError::Oversell {
                instrument_id: yes_instrument_id.clone(),
                available_qty: 0.0,
                attempted_qty: merged_qty,
            });
        }

        if let Some(entry) = self.positions.get_mut(no_instrument_id) {
            if entry.quantity + 1e-9 < merged_qty {
                return Err(InventoryError::Oversell {
                    instrument_id: no_instrument_id.clone(),
                    available_qty: entry.quantity,
                    attempted_qty: merged_qty,
                });
            }
            entry.quantity -= merged_qty;
            entry.updated_at_ms = observed_at_ms;
            if entry.quantity <= 1e-9 {
                remove_no = true;
            }
        } else {
            return Err(InventoryError::Oversell {
                instrument_id: no_instrument_id.clone(),
                available_qty: 0.0,
                attempted_qty: merged_qty,
            });
        }

        if remove_yes {
            self.positions.remove(yes_instrument_id);
        }
        if remove_no {
            self.positions.remove(no_instrument_id);
        }

        self.free_cash_usd += net_cash_usd;
        self.realized_pnl_usd += realized_pnl_delta_usd;

        Ok(InventoryAdjustment {
            reason: InventoryAdjustmentReason::MergeApplied,
            observed_at_ms,
            market_id: Some(market_id.clone()),
            instrument_id: None,
            client_order_id: None,
            cash_delta_usd: net_cash_usd,
            reserved_cash_delta_usd: 0.0,
            position_delta: 0.0,
            realized_pnl_delta_usd,
            free_cash_after_usd: self.free_cash_usd,
            reserved_cash_after_usd: self.reserved_cash_usd,
            gross_exposure_after_usd: self.gross_exposure_usd(),
        })
    }

    fn noop_adjustment(
        &self,
        reason: InventoryAdjustmentReason,
        observed_at_ms: EpochMillis,
        market_id: Option<MarketId>,
        instrument_id: Option<InstrumentId>,
        client_order_id: Option<ClientOrderId>,
    ) -> InventoryAdjustment {
        InventoryAdjustment {
            reason,
            observed_at_ms,
            market_id,
            instrument_id,
            client_order_id,
            cash_delta_usd: 0.0,
            reserved_cash_delta_usd: 0.0,
            position_delta: 0.0,
            realized_pnl_delta_usd: 0.0,
            free_cash_after_usd: self.free_cash_usd,
            reserved_cash_after_usd: self.reserved_cash_usd,
            gross_exposure_after_usd: self.gross_exposure_usd(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{InventoryState, VenuePositionSnapshot};
    use crate::types::{
        ClientOrderId, FillLiquidity, FillReport, InstrumentId, MarketId, OrderIntent, TradeSide,
    };

    #[test]
    fn reserve_buy_and_apply_round_trip_sell() {
        let market_id = MarketId::from("market-a");
        let instrument_id = InstrumentId::from("token-a");
        let client_order_id = ClientOrderId::from("client-1");
        let mut inventory = InventoryState::new(100.0);

        let reserve = inventory
            .reserve_for_order(&OrderIntent {
                client_order_id: client_order_id.clone(),
                market_id: market_id.clone(),
                instrument_id: instrument_id.clone(),
                side: TradeSide::Buy,
                limit_price: 0.40,
                quantity: 10.0,
                reduce_only: false,
                reason: "test".into(),
                quote_level_tag: None,
                created_at_ms: 10,
                pair_id: None,
            kind: crate::types::IntentKind::Entry,
            })
            .expect("buy reserve");
        assert_eq!(reserve.cash_delta_usd, -4.0);
        assert_eq!(inventory.free_cash_usd(), 96.0);
        assert_eq!(inventory.reserved_cash_usd(), 4.0);

        inventory
            .apply_fill(&FillReport {
                order_id: None,
                client_order_id: Some(client_order_id.clone()),
                market_id: market_id.clone(),
                instrument_id: instrument_id.clone(),
                side: TradeSide::Buy,
                price: 0.39,
                quantity: 10.0,
                fee_usd: 0.10,
                liquidity: FillLiquidity::Taker,
                close_method: None,
                observed_at_ms: 12,
            })
            .expect("buy fill");
        assert!((inventory.free_cash_usd() - 96.0).abs() < 1e-9);
        assert_eq!(inventory.reserved_cash_usd(), 0.0);
        assert_eq!(inventory.position_qty(&instrument_id), 10.0);

        inventory
            .apply_fill(&FillReport {
                order_id: None,
                client_order_id: None,
                market_id,
                instrument_id: instrument_id.clone(),
                side: TradeSide::Sell,
                price: 0.50,
                quantity: 10.0,
                fee_usd: 0.10,
                liquidity: FillLiquidity::Taker,
                close_method: None,
                observed_at_ms: 20,
            })
            .expect("sell fill");

        assert_eq!(inventory.position_qty(&instrument_id), 0.0);
        assert!((inventory.free_cash_usd() - 100.9).abs() < 1e-9);
        assert!((inventory.realized_pnl_usd() - 1.0).abs() < 1e-9);
    }

    #[test]
    fn venue_position_snapshot_recovers_local_flat_inventory() {
        let mut inventory = InventoryState::new(100.0);
        let report = inventory
            .reconcile_venue_positions(
                &[VenuePositionSnapshot {
                    market_id: MarketId::from("market-a"),
                    condition_id: None,
                    instrument_id: InstrumentId::from("token-down"),
                    quantity: 6.5,
                    average_cost_usd: 0.80,
                    mark_price: Some(1.0),
                    observed_at_ms: 100,
                }],
                110,
            )
            .expect("venue reconciliation");

        let position = inventory
            .position(&InstrumentId::from("token-down"))
            .expect("venue position should be represented locally");
        assert_eq!(position.quantity, 6.5);
        assert_eq!(position.avg_price, 0.80);
        assert_eq!(position.mark_price, Some(1.0));
        assert_eq!(report.deltas.len(), 1);
        assert_eq!(report.deltas[0].local_quantity_before, 0.0);
        assert_eq!(report.deltas[0].venue_quantity, 6.5);
        assert_eq!(report.stranded_markets.len(), 1);
        assert_eq!(
            report.stranded_markets[0].market_id,
            MarketId::from("market-a")
        );
        assert_eq!(
            report.stranded_markets[0].stranded_positions[0].quantity,
            6.5
        );
    }

    #[test]
    fn venue_position_snapshot_marks_stranded_imbalance_explicitly() {
        let mut inventory = InventoryState::new(100.0);
        inventory
            .reconcile_venue_positions(
                &[
                    VenuePositionSnapshot {
                        market_id: MarketId::from("market-a"),
                        condition_id: None,
                        instrument_id: InstrumentId::from("token-up"),
                        quantity: 5.0,
                        average_cost_usd: 0.80,
                        mark_price: Some(0.5),
                        observed_at_ms: 100,
                    },
                    VenuePositionSnapshot {
                        market_id: MarketId::from("market-a"),
                        condition_id: None,
                        instrument_id: InstrumentId::from("token-down"),
                        quantity: 6.5,
                        average_cost_usd: 0.18,
                        mark_price: Some(0.5),
                        observed_at_ms: 100,
                    },
                ],
                110,
            )
            .expect("venue reconciliation");

        let stranded = inventory.stranded_market_inventory();
        assert_eq!(stranded.len(), 1);
        assert_eq!(stranded[0].paired_quantity, 5.0);
        assert_eq!(stranded[0].stranded_positions.len(), 1);
        assert_eq!(
            stranded[0].stranded_positions[0].instrument_id,
            InstrumentId::from("token-down")
        );
        assert!((stranded[0].stranded_positions[0].quantity - 1.5).abs() < 1e-9);
    }
}
