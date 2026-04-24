use std::collections::HashMap;

use crate::types::{EpochMillis, FillReport, InstrumentId, MarketId, TradeSide};

const EPSILON_QTY: f64 = 1e-9;

#[derive(Clone, Copy, Debug, PartialEq)]
enum PairLeg {
    Yes,
    No,
}

#[derive(Clone, Debug, PartialEq)]
pub struct PairLot {
    pub quantity: f64,
    pub avg_price: f64,
    pub acquired_at_ms: EpochMillis,
}

impl PairLot {
    pub fn new(quantity: f64, avg_price: f64, acquired_at_ms: EpochMillis) -> Self {
        Self {
            quantity,
            avg_price,
            acquired_at_ms,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct StrandedLot {
    pub quantity: f64,
    pub avg_price: f64,
    pub acquired_at_ms: EpochMillis,
}

impl From<PairLot> for StrandedLot {
    fn from(lot: PairLot) -> Self {
        Self {
            quantity: lot.quantity,
            avg_price: lot.avg_price,
            acquired_at_ms: lot.acquired_at_ms,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct MarketPairState {
    pub market_id: MarketId,
    pub yes_instrument_id: Option<InstrumentId>,
    pub no_instrument_id: Option<InstrumentId>,
    pub yes_lots: Vec<PairLot>,
    pub no_lots: Vec<PairLot>,
    pub yes_stranded_lots: Vec<StrandedLot>,
    pub no_stranded_lots: Vec<StrandedLot>,
    pub paired_qty: f64,
    pub paired_cost_usd: f64,
    pub stranded_yes_qty: f64,
    pub stranded_no_qty: f64,
    pub stranded_yes_cost_usd: f64,
    pub stranded_no_cost_usd: f64,
    pub mergeable_notional_usd: f64,
    pub expected_merge_gain_usd: f64,
    pub last_merge_at_ms: Option<EpochMillis>,
    pub last_update_ms: EpochMillis,
}

impl MarketPairState {
    pub fn new(market_id: MarketId) -> Self {
        Self {
            market_id,
            yes_instrument_id: None,
            no_instrument_id: None,
            yes_lots: Vec::new(),
            no_lots: Vec::new(),
            yes_stranded_lots: Vec::new(),
            no_stranded_lots: Vec::new(),
            paired_qty: 0.0,
            paired_cost_usd: 0.0,
            stranded_yes_qty: 0.0,
            stranded_no_qty: 0.0,
            stranded_yes_cost_usd: 0.0,
            stranded_no_cost_usd: 0.0,
            mergeable_notional_usd: 0.0,
            expected_merge_gain_usd: 0.0,
            last_merge_at_ms: None,
            last_update_ms: 0,
        }
    }

    pub fn merged_qty(&self) -> f64 {
        self.paired_qty
    }

    pub fn stranded_yes_instruments(&self) -> &[StrandedLot] {
        &self.yes_stranded_lots
    }

    pub fn stranded_no_instruments(&self) -> &[StrandedLot] {
        &self.no_stranded_lots
    }

    fn classify(instrument_id: &InstrumentId) -> Option<PairLeg> {
        let id = instrument_id.as_str().to_ascii_lowercase();
        if id.contains("yes") || id.contains("up") {
            Some(PairLeg::Yes)
        } else if id.contains("no") || id.contains("down") {
            Some(PairLeg::No)
        } else {
            None
        }
    }

    fn append_lot(&mut self, leg: PairLeg, instrument_id: InstrumentId, lot: PairLot) {
        match leg {
            PairLeg::Yes => {
                if self.yes_instrument_id.is_none() {
                    self.yes_instrument_id = Some(instrument_id);
                }
                self.yes_lots.push(lot);
            }
            PairLeg::No => {
                if self.no_instrument_id.is_none() {
                    self.no_instrument_id = Some(instrument_id);
                }
                self.no_lots.push(lot);
            }
        }
    }

    fn recalc(&mut self, observed_at_ms: EpochMillis) {
        let pairing = pair_fifo(&self.yes_lots, &self.no_lots, None);
        let payout_per_pair = merged_payout(self.market_id.as_str()).unwrap_or(1.0);

        self.paired_qty = pairing.paired_qty;
        self.paired_cost_usd = pairing.paired_cost_usd;
        self.yes_stranded_lots = pairing.yes_stranded;
        self.no_stranded_lots = pairing.no_stranded;
        self.stranded_yes_qty = pairing.yes_stranded_qty;
        self.stranded_no_qty = pairing.no_stranded_qty;
        self.stranded_yes_cost_usd = pairing.yes_stranded_cost_usd;
        self.stranded_no_cost_usd = pairing.no_stranded_cost_usd;
        self.mergeable_notional_usd = pairing.paired_qty * payout_per_pair;
        self.expected_merge_gain_usd = expected_merge_gain(
            pairing.paired_qty,
            pairing.paired_cost_usd,
            0.0,
            0.0,
            payout_per_pair,
        );
        self.last_update_ms = observed_at_ms;
    }

    pub fn ingest_fill(&mut self, fill: &FillReport) {
        let Some(leg) = Self::classify(&fill.instrument_id) else {
            return;
        };

        if !matches!(fill.side, TradeSide::Buy | TradeSide::Sell) {
            return;
        }

        if fill.quantity <= EPSILON_QTY {
            return;
        }

        match fill.side {
            TradeSide::Buy => {
                self.append_lot(
                    leg,
                    fill.instrument_id.clone(),
                    PairLot::new(fill.quantity, fill.price, fill.observed_at_ms),
                );
            }
            TradeSide::Sell => {
                let instrument_lots = match leg {
                    PairLeg::Yes => &mut self.yes_lots,
                    PairLeg::No => &mut self.no_lots,
                };
                if total_quantity(instrument_lots) + EPSILON_QTY < fill.quantity {
                    return;
                }
                let removed = remove_fifo(instrument_lots, fill.quantity);
                if removed.is_none() {
                    return;
                }
                let removed = removed.unwrap();
                debug_assert!(
                    removed + EPSILON_QTY >= fill.quantity,
                    "sell fill should be fully consumed after availability check"
                );
            }
        }

        self.recalc(fill.observed_at_ms);
    }

    pub fn preview_merge_plan(&self, requested_qty: f64) -> Option<MergePlan> {
        let yes_instrument_id = self.yes_instrument_id.clone()?;
        let no_instrument_id = self.no_instrument_id.clone()?;
        if requested_qty <= EPSILON_QTY {
            return None;
        }

        let pairing = pair_fifo(&self.yes_lots, &self.no_lots, Some(requested_qty));
        if pairing.paired_qty <= EPSILON_QTY {
            return None;
        }

        let merged_qty = pairing.paired_qty;
        let payout_per_pair = merged_payout(self.market_id.as_str()).unwrap_or(1.0);
        let expected_cash_usd = merged_qty * payout_per_pair;

        Some(MergePlan {
            market_id: self.market_id.clone(),
            requested_qty,
            merged_qty,
            yes_instrument_id,
            no_instrument_id,
            expected_cash_usd,
            expected_cost_usd: pairing.paired_cost_usd,
            expected_gain_usd: expected_merge_gain(
                merged_qty,
                pairing.paired_cost_usd,
                0.0,
                0.0,
                payout_per_pair,
            ),
            legs_to_close: 2,
        })
    }

    pub fn apply_merge(
        &mut self,
        requested_qty: f64,
        observed_at_ms: EpochMillis,
    ) -> Option<MergePlan> {
        let mut plan = self.preview_merge_plan(requested_qty)?;
        let merged_qty = plan.merged_qty;
        if merged_qty <= EPSILON_QTY {
            return None;
        }

        let mut yes_lots = self.yes_lots.clone();
        let mut no_lots = self.no_lots.clone();
        let consumed = consume_pairwise(&mut yes_lots, &mut no_lots, merged_qty)?;
        if consumed <= EPSILON_QTY {
            return None;
        }
        self.yes_lots = yes_lots;
        self.no_lots = no_lots;

        self.last_merge_at_ms = Some(observed_at_ms);
        self.recalc(observed_at_ms);
        plan.expected_gain_usd = expected_merge_gain(
            plan.merged_qty,
            plan.expected_cost_usd,
            0.0,
            0.0,
            merged_payout(self.market_id.as_str()).unwrap_or(1.0),
        );
        Some(plan)
    }

    pub fn merge_candidate(&self, taker_fee_rate: f64, gas_fee_usd: f64) -> Option<MergeCandidate> {
        if self.yes_instrument_id.is_none() || self.no_instrument_id.is_none() {
            return None;
        }

        let merged_qty = self.merged_qty();
        if merged_qty <= EPSILON_QTY {
            return None;
        }

        let payout_per_pair = merged_payout(self.market_id.as_str()).unwrap_or(1.0);
        let expected_cash = merged_qty * payout_per_pair;
        let expected_fee = taker_fee(expected_cash, taker_fee_rate);
        let expected_net = expected_cash - self.paired_cost_usd - expected_fee - gas_fee_usd;

        Some(MergeCandidate {
            market_id: self.market_id.clone(),
            yes_instrument_id: self.yes_instrument_id.clone(),
            no_instrument_id: self.no_instrument_id.clone(),
            paired_qty: merged_qty,
            stranded_yes_qty: self.stranded_yes_qty,
            stranded_no_qty: self.stranded_no_qty,
            stranded_yes_cost_usd: self.stranded_yes_cost_usd,
            stranded_no_cost_usd: self.stranded_no_cost_usd,
            expected_cash_usd: expected_cash,
            expected_cost_usd: self.paired_cost_usd,
            expected_gas_usd: gas_fee_usd,
            expected_fee_usd: expected_fee,
            expected_net_gain_usd: expected_net,
            last_merge_at_ms: self.last_merge_at_ms,
            observed_at_ms: self.last_update_ms,
        })
    }
}

fn total_quantity(lots: &[PairLot]) -> f64 {
    lots.iter().map(|lot| lot.quantity.max(0.0)).sum()
}

#[derive(Clone, Debug, PartialEq)]
pub struct MergePlan {
    pub market_id: MarketId,
    pub requested_qty: f64,
    pub merged_qty: f64,
    pub yes_instrument_id: InstrumentId,
    pub no_instrument_id: InstrumentId,
    pub expected_cash_usd: f64,
    pub expected_cost_usd: f64,
    pub expected_gain_usd: f64,
    pub legs_to_close: usize,
}

#[derive(Clone, Debug, PartialEq)]
pub struct MergeCandidate {
    pub market_id: MarketId,
    pub yes_instrument_id: Option<InstrumentId>,
    pub no_instrument_id: Option<InstrumentId>,
    pub paired_qty: f64,
    pub stranded_yes_qty: f64,
    pub stranded_no_qty: f64,
    pub stranded_yes_cost_usd: f64,
    pub stranded_no_cost_usd: f64,
    pub expected_cash_usd: f64,
    pub expected_cost_usd: f64,
    pub expected_gas_usd: f64,
    pub expected_fee_usd: f64,
    pub expected_net_gain_usd: f64,
    pub last_merge_at_ms: Option<EpochMillis>,
    pub observed_at_ms: EpochMillis,
}

#[derive(Default)]
struct PairingOutcome {
    paired_qty: f64,
    paired_cost_usd: f64,
    yes_stranded_qty: f64,
    no_stranded_qty: f64,
    yes_stranded_cost_usd: f64,
    no_stranded_cost_usd: f64,
    yes_stranded: Vec<StrandedLot>,
    no_stranded: Vec<StrandedLot>,
}

fn pair_fifo(yes: &[PairLot], no: &[PairLot], target_qty: Option<f64>) -> PairingOutcome {
    let mut target = target_qty.unwrap_or(f64::INFINITY);
    let mut yes_cursor = yes.to_vec();
    let mut no_cursor = no.to_vec();
    let mut yes_idx = 0;
    let mut no_idx = 0;
    let mut out = PairingOutcome::default();

    while yes_idx < yes_cursor.len()
        && no_idx < no_cursor.len()
        && target > EPSILON_QTY
        && yes_cursor[yes_idx].quantity > EPSILON_QTY
        && no_cursor[no_idx].quantity > EPSILON_QTY
    {
        let pair_qty = yes_cursor[yes_idx]
            .quantity
            .min(no_cursor[no_idx].quantity)
            .min(target);
        if pair_qty <= EPSILON_QTY {
            break;
        }

        out.paired_qty += pair_qty;
        out.paired_cost_usd +=
            pair_qty * yes_cursor[yes_idx].avg_price + pair_qty * no_cursor[no_idx].avg_price;

        yes_cursor[yes_idx].quantity -= pair_qty;
        no_cursor[no_idx].quantity -= pair_qty;
        target -= pair_qty;

        if yes_cursor[yes_idx].quantity <= EPSILON_QTY {
            yes_idx += 1;
        }
        if no_cursor[no_idx].quantity <= EPSILON_QTY {
            no_idx += 1;
        }
    }

    for lot in yes_cursor.into_iter().skip(yes_idx) {
        if lot.quantity <= EPSILON_QTY {
            continue;
        }
        out.yes_stranded_qty += lot.quantity;
        out.yes_stranded_cost_usd += lot.quantity * lot.avg_price;
        out.yes_stranded.push(StrandedLot::from(lot));
    }

    for lot in no_cursor.into_iter().skip(no_idx) {
        if lot.quantity <= EPSILON_QTY {
            continue;
        }
        out.no_stranded_qty += lot.quantity;
        out.no_stranded_cost_usd += lot.quantity * lot.avg_price;
        out.no_stranded.push(StrandedLot::from(lot));
    }

    out
}

fn remove_fifo(lots: &mut Vec<PairLot>, requested_qty: f64) -> Option<f64> {
    let mut working = lots.clone();
    let mut remaining = requested_qty.max(0.0);
    let mut removed = 0.0;
    while remaining > EPSILON_QTY && !working.is_empty() {
        let consume = working[0].quantity.min(remaining);
        if consume <= EPSILON_QTY {
            working.remove(0);
            continue;
        }
        working[0].quantity -= consume;
        removed += consume;
        remaining -= consume;
        if working[0].quantity <= EPSILON_QTY {
            working.remove(0);
        }
    }
    if remaining > EPSILON_QTY {
        return None;
    }
    *lots = working;
    Some(removed)
}

fn consume_pairwise(
    yes_lots: &mut Vec<PairLot>,
    no_lots: &mut Vec<PairLot>,
    qty: f64,
) -> Option<f64> {
    if qty <= EPSILON_QTY {
        return Some(0.0);
    }

    let mut yes_working = yes_lots.clone();
    let mut no_working = no_lots.clone();
    let mut remaining = qty;
    let mut consumed = 0.0;

    while remaining > EPSILON_QTY && !yes_working.is_empty() && !no_working.is_empty() {
        let pair_qty = yes_working[0]
            .quantity
            .min(no_working[0].quantity)
            .min(remaining);
        if pair_qty <= EPSILON_QTY {
            yes_working.remove(0);
            no_working.remove(0);
            continue;
        }

        yes_working[0].quantity -= pair_qty;
        no_working[0].quantity -= pair_qty;
        remaining -= pair_qty;
        consumed += pair_qty;

        if yes_working[0].quantity <= EPSILON_QTY {
            yes_working.remove(0);
        }
        if no_working[0].quantity <= EPSILON_QTY {
            no_working.remove(0);
        }
    }

    if remaining > EPSILON_QTY {
        return None;
    }
    *yes_lots = yes_working;
    *no_lots = no_working;
    Some(consumed)
}

fn merged_payout(_market_id: &str) -> Option<f64> {
    Some(1.0)
}

fn taker_fee(cash_usd: f64, taker_fee_rate: f64) -> f64 {
    if cash_usd <= 0.0 {
        return 0.0;
    }
    (cash_usd * taker_fee_rate).max(0.0)
}

fn expected_merge_gain(
    merged_qty: f64,
    paired_cost_usd: f64,
    taker_fee_rate: f64,
    gas_fee_usd: f64,
    payout_per_pair: f64,
) -> f64 {
    let payout = merged_qty * payout_per_pair;
    let fee = taker_fee(payout, taker_fee_rate);
    payout - paired_cost_usd - fee - gas_fee_usd
}

#[derive(Clone, Debug, Default)]
pub struct MarketPairLedger {
    pub states: HashMap<MarketId, MarketPairState>,
}

impl MarketPairLedger {
    pub fn new() -> Self {
        Self {
            states: HashMap::new(),
        }
    }

    pub fn state(&self, market_id: &MarketId) -> Option<&MarketPairState> {
        self.states.get(market_id)
    }

    pub fn ingest_fill(&mut self, fill: &FillReport) {
        let state = self
            .states
            .entry(fill.market_id.clone())
            .or_insert_with(|| MarketPairState::new(fill.market_id.clone()));

        if !matches!(fill.close_method, Some(crate::types::CloseMethod::Merge)) {
            state.ingest_fill(fill);
        }
    }

    pub fn merge_candidate(
        &self,
        market_id: &MarketId,
        fee_rate: f64,
        gas_fee_usd: f64,
    ) -> Option<MergeCandidate> {
        self.states
            .get(market_id)
            .and_then(|state| state.merge_candidate(fee_rate, gas_fee_usd))
    }

    pub fn preview_merge(&self, market_id: &MarketId, requested_qty: f64) -> Option<MergePlan> {
        self.states
            .get(market_id)
            .and_then(|state| state.preview_merge_plan(requested_qty))
    }

    pub fn apply_merge(
        &mut self,
        market_id: &MarketId,
        requested_qty: f64,
        observed_at_ms: EpochMillis,
    ) -> Option<MergePlan> {
        self.states
            .get_mut(market_id)
            .and_then(|state| state.apply_merge(requested_qty, observed_at_ms))
    }
}

#[cfg(test)]
mod tests {
    use super::{MarketPairLedger, MergeCandidate, MergePlan};
    use crate::types::{FillReport, InstrumentId, MarketId, TradeSide};

    fn mk_fill(instrument: &str, side: TradeSide, qty: f64, price: f64) -> FillReport {
        FillReport {
            order_id: None,
            client_order_id: None,
            market_id: MarketId::from("market-1"),
            instrument_id: InstrumentId::from(instrument),
            side,
            price,
            quantity: qty,
            fee_usd: 0.0,
            liquidity: crate::types::FillLiquidity::Unknown,
            close_method: None,
            observed_at_ms: 1,
        }
    }

    #[test]
    fn fifo_pairing_tracks_stranded_and_paired() {
        let mut ledger = MarketPairLedger::new();
        ledger.ingest_fill(&mk_fill("token-up", TradeSide::Buy, 12.0, 0.4));
        ledger.ingest_fill(&mk_fill("token-down", TradeSide::Buy, 8.0, 0.6));
        ledger.ingest_fill(&mk_fill("token-up", TradeSide::Buy, 5.0, 0.3));

        let state = ledger.state(&MarketId::from("market-1")).expect("state");
        assert!((state.paired_qty - 8.0).abs() < 1e-9);
        assert!((state.stranded_yes_qty - 9.0).abs() < 1e-9);
    }

    #[test]
    fn merge_plan_prefers_fifo() {
        let mut ledger = MarketPairLedger::new();
        ledger.ingest_fill(&mk_fill("yes-a-up", TradeSide::Buy, 4.0, 0.50));
        ledger.ingest_fill(&mk_fill("yes-a-up", TradeSide::Buy, 3.0, 0.60));
        ledger.ingest_fill(&mk_fill("no-a-down", TradeSide::Buy, 5.0, 0.40));

        let plan = ledger
            .preview_merge(&MarketId::from("market-1"), 4.0)
            .expect("plan");

        assert_eq!(plan.legs_to_close, 2);
        assert!((plan.merged_qty - 4.0).abs() < 1e-9);
        assert!((plan.expected_cost_usd - 3.6).abs() < 1e-9);
    }

    #[test]
    fn merges_reduce_inventory_pairs() {
        let mut ledger = MarketPairLedger::new();
        ledger.ingest_fill(&mk_fill("yes-a-up", TradeSide::Buy, 2.0, 0.20));
        ledger.ingest_fill(&mk_fill("no-a-down", TradeSide::Buy, 2.0, 0.80));

        let applied = ledger
            .apply_merge(&MarketId::from("market-1"), 1.5, 10)
            .expect("merge apply");

        let state = ledger.state(&MarketId::from("market-1")).expect("state");
        assert!((applied.merged_qty - 1.5).abs() < 1e-9);
        assert!((state.paired_qty - 0.5).abs() < 1e-9);
        assert_eq!(state.yes_lots.len(), 1);
        assert_eq!(state.no_lots.len(), 1);
        assert!((state.yes_lots[0].quantity - 0.5).abs() < 1e-9);
    }

    #[test]
    fn merge_candidate_reports_expected_gain() {
        let mut ledger = MarketPairLedger::new();
        ledger.ingest_fill(&mk_fill("yes-a-up", TradeSide::Buy, 3.0, 0.20));
        ledger.ingest_fill(&mk_fill("no-a-down", TradeSide::Buy, 3.0, 0.30));
        let candidate: MergeCandidate = ledger
            .merge_candidate(&MarketId::from("market-1"), 0.001, 0.5)
            .expect("candidate");

        assert!((candidate.paired_qty - 3.0).abs() < 1e-9);
        assert!((candidate.expected_cost_usd - 1.5).abs() < 1e-9);
        assert!(
            (candidate.expected_net_gain_usd > 0.0)
                == (candidate.expected_cash_usd > candidate.expected_cost_usd)
        );
    }

    #[test]
    fn consume_pairwise_is_transactional_on_shortfall() {
        let mut yes_lots = vec![super::PairLot::new(2.0, 0.20, 1)];
        let mut no_lots = vec![super::PairLot::new(1.0, 0.30, 1)];
        let yes_before = yes_lots.clone();
        let no_before = no_lots.clone();

        assert_eq!(
            super::consume_pairwise(&mut yes_lots, &mut no_lots, 1.5),
            None
        );
        assert_eq!(yes_lots, yes_before);
        assert_eq!(no_lots, no_before);
    }

    #[test]
    fn remove_fifo_is_transactional_on_shortfall() {
        let mut lots = vec![super::PairLot::new(2.0, 0.25, 1)];
        let before = lots.clone();

        assert_eq!(super::remove_fifo(&mut lots, 3.0), None);
        assert_eq!(lots, before);
    }

    #[test]
    fn merge_plan_fields() {
        let plan = MergePlan {
            market_id: MarketId::from("market-1"),
            requested_qty: 1.0,
            merged_qty: 0.8,
            yes_instrument_id: InstrumentId::from("yes-1"),
            no_instrument_id: InstrumentId::from("no-1"),
            expected_cash_usd: 1.0,
            expected_cost_usd: 0.8,
            expected_gain_usd: 0.2,
            legs_to_close: 2,
        };
        assert!(plan.expected_gain_usd >= 0.0);
    }
}
