//! Merge and redeem execution planning against paired YES/NO inventory.

use crate::inventory::{InventoryAdjustment, InventoryError, InventoryState};
use crate::market_making::pairing::pair_ledger::{
    MarketPairLedger, MergeCandidate, MergePlan, ResolvedRedeemCandidate, ResolvedWinningLeg,
};
use crate::types::{ClientOrderId, CloseMethod, EpochMillis, FillReport, MarketId, MergeIntent};

#[derive(Clone, Debug, PartialEq)]
pub struct MergeExecution {
    pub market_id: MarketId,
    pub requested_qty: f64,
    pub merged_qty: f64,
    pub yes_instrument_id: crate::types::InstrumentId,
    pub no_instrument_id: crate::types::InstrumentId,
    pub expected_cash_usd: f64,
    pub expected_cost_usd: f64,
    pub expected_fee_usd: f64,
    pub expected_gas_usd: f64,
    pub net_gain_usd: f64,
    pub adjustment: InventoryAdjustment,
    pub observed_at_ms: EpochMillis,
}

#[derive(Clone, Debug)]
pub struct MergeExecutor {
    ledger: MarketPairLedger,
    taker_fee_rate: f64,
    gas_fee_usd: f64,
}

impl MergeExecutor {
    pub fn new() -> Self {
        Self::with_fees(0.0, 0.0)
    }

    pub fn with_fees(taker_fee_rate: f64, gas_fee_usd: f64) -> Self {
        Self {
            ledger: MarketPairLedger::new(),
            taker_fee_rate,
            gas_fee_usd,
        }
    }

    pub fn ledger(&self) -> &MarketPairLedger {
        &self.ledger
    }

    pub fn on_fill(&mut self, fill: &FillReport) {
        self.ledger.ingest_fill(fill);
    }

    pub fn merge_candidate(&self, market_id: &MarketId) -> Option<MergeCandidate> {
        self.ledger
            .merge_candidate(market_id, self.taker_fee_rate, self.gas_fee_usd)
    }

    pub fn merge_intent(
        &self,
        market_id: &MarketId,
        now_ms: EpochMillis,
        reason: impl Into<String>,
    ) -> Option<MergeIntent> {
        let candidate = self.merge_candidate(market_id)?;
        let yes_instrument_id = candidate.yes_instrument_id?;
        let no_instrument_id = candidate.no_instrument_id?;
        if candidate.paired_qty <= 1e-9 {
            return None;
        }

        Some(MergeIntent {
            command_id: ClientOrderId::from(format!(
                "merge:{}:{:.8}:{}",
                market_id, candidate.paired_qty, now_ms
            )),
            market_id: market_id.clone(),
            condition_id: None,
            yes_instrument_id,
            no_instrument_id,
            quantity: candidate.paired_qty,
            expected_cash_usd: candidate.expected_cash_usd,
            expected_cost_usd: candidate.expected_cost_usd,
            expected_fee_usd: candidate.expected_fee_usd,
            expected_gas_usd: candidate.expected_gas_usd,
            reason: reason.into(),
            created_at_ms: now_ms,
        })
    }

    pub fn apply_merge(
        &mut self,
        fill: &FillReport,
        inventory: &mut InventoryState,
    ) -> Result<Option<MergeExecution>, InventoryError> {
        match fill.close_method {
            Some(CloseMethod::Merge) => {}
            Some(CloseMethod::Settlement) | Some(CloseMethod::Settle) => {}
            _ => return Ok(None),
        }

        let plan = match self.ledger.preview_merge(&fill.market_id, fill.quantity) {
            Some(plan) => plan,
            None => return Ok(None),
        };
        if plan.merged_qty <= 0.0 {
            return Ok(None);
        }

        let inventory_yes = inventory.position_qty(&plan.yes_instrument_id);
        if inventory_yes + 1e-9 < plan.merged_qty {
            return Err(InventoryError::Oversell {
                instrument_id: plan.yes_instrument_id,
                available_qty: inventory_yes,
                attempted_qty: plan.merged_qty,
            });
        }
        let inventory_no = inventory.position_qty(&plan.no_instrument_id);
        if inventory_no + 1e-9 < plan.merged_qty {
            return Err(InventoryError::Oversell {
                instrument_id: plan.no_instrument_id,
                available_qty: inventory_no,
                attempted_qty: plan.merged_qty,
            });
        }

        let taker_fee = fee_for_merge(plan.expected_cash_usd, self.taker_fee_rate);
        let external_fee = fill.fee_usd + self.gas_fee_usd + taker_fee;

        let mut next_inventory = inventory.clone();
        let adjustment = next_inventory.apply_merge(
            &plan.market_id,
            &plan.yes_instrument_id,
            &plan.no_instrument_id,
            plan.merged_qty,
            plan.expected_cost_usd,
            plan.expected_cash_usd,
            external_fee,
            fill.observed_at_ms,
        )?;
        let applied = self
            .ledger
            .apply_merge(&fill.market_id, plan.merged_qty, fill.observed_at_ms)
            .ok_or(InventoryError::InvalidFill(
                "merge failed due to unexpected state transition",
            ))?;
        let plan_to_apply = applied;
        *inventory = next_inventory;

        Ok(Some(MergeExecution {
            market_id: plan_to_apply.market_id,
            requested_qty: plan_to_apply.requested_qty,
            merged_qty: plan_to_apply.merged_qty,
            yes_instrument_id: plan_to_apply.yes_instrument_id,
            no_instrument_id: plan_to_apply.no_instrument_id,
            expected_cash_usd: plan_to_apply.expected_cash_usd,
            expected_cost_usd: plan_to_apply.expected_cost_usd,
            expected_fee_usd: external_fee,
            expected_gas_usd: self.gas_fee_usd,
            net_gain_usd: (plan_to_apply.expected_cash_usd - external_fee)
                - plan_to_apply.expected_cost_usd,
            adjustment,
            observed_at_ms: fill.observed_at_ms,
        }))
    }

    pub fn preview_merge(&self, market_id: &MarketId, requested_qty: f64) -> Option<MergePlan> {
        self.ledger.preview_merge(market_id, requested_qty)
    }

    pub fn resolved_redeem_candidate(
        &self,
        market_id: &MarketId,
        winning_leg: ResolvedWinningLeg,
    ) -> Option<ResolvedRedeemCandidate> {
        self.ledger
            .resolved_redeem_candidate(market_id, winning_leg)
    }
}

fn fee_for_merge(cash_usd: f64, taker_fee_rate: f64) -> f64 {
    if cash_usd <= 0.0 || taker_fee_rate <= 0.0 {
        return 0.0;
    }
    cash_usd * taker_fee_rate
}
