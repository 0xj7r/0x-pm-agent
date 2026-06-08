//! Live shadow adapter for the BackToExplore taker strategy.
//!
//! This is deliberately read-only: it runs the shared `BackToExploreTaker`
//! decision logic from `polymarket-backtest` against live-shaped events and
//! returns the orders it would place. Runtime execution remains a separate,
//! explicit wiring step.

use std::collections::VecDeque;

use pm_strategy::{BackToExploreConfig, BackToExploreTaker, OrderRequest, Side, Strategy};
use pm_types::{
    BookLevel, MarketId, ReplayEvent, ReplayFlags, SpotHistory, SpotTick, TradeHistory, TAPE_DEPTH,
};

use crate::runtime::br2_shadow::{SpotTrade, YesTopOfBook};

const SPOT_RETENTION_NS: i64 = 600 * 1_000_000_000;

#[derive(Debug, Clone, Copy, Default)]
pub struct BteDecisionPosition {
    pub events_seen: u64,
    pub yes_shares: f64,
    pub no_shares: f64,
    pub yes_avg_price: f64,
    pub no_avg_price: f64,
    pub cash_usdc: f64,
    pub current_market_net_exposure_shares: f64,
    pub btc_net_exposure_shares: f64,
    pub eth_net_exposure_shares: f64,
    pub daily_start_cash_usdc: f64,
    pub daily_loss_cap_pct: f64,
    pub current_daily_loss_pct: f64,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct BteRegimeInputs {
    pub whipsaw_score: f32,
    pub path_efficiency: f32,
    pub reversal_pressure: f32,
    pub sign_flip_rate: f32,
    pub realized_vol_180s_bps: f32,
}

#[derive(Debug, Clone)]
pub struct BteShadowOrder {
    pub market_id: u32,
    pub side: Side,
    pub shares: f64,
    pub max_depth: usize,
    pub limit_price: Option<f32>,
    pub tag: &'static str,
}

impl BteShadowOrder {
    pub fn from_request(market_id: u32, req: &OrderRequest) -> Self {
        Self {
            market_id,
            side: req.side,
            shares: req.shares,
            max_depth: req.max_depth,
            limit_price: req.limit_price,
            tag: req.tag,
        }
    }

    pub fn log_line(&self) -> String {
        let (outcome, px) = match self.side {
            Side::BuyYes | Side::SellYes => ("YES", self.limit_price),
            Side::BuyNo | Side::SellNo => ("NO", self.limit_price.map(|p| 1.0 - p)),
        };
        let action = match self.side {
            Side::BuyYes | Side::BuyNo => "BUY",
            Side::SellYes | Side::SellNo => "SELL",
        };
        let px_str = px
            .map(|p| format!("{p:.4}"))
            .unwrap_or_else(|| "MKT".to_string());
        format!(
            "BTE_SHADOW market={} {} {} clip={:.2} price={} depth={} tag={}",
            self.market_id, action, outcome, self.shares, px_str, self.max_depth, self.tag
        )
    }
}

struct MarketState {
    market_id: u32,
    close_ns: i64,
    spot_at_open: f64,
    strategy: BackToExploreTaker,
    yes_min: f32,
    yes_max: f32,
    events_seen: u64,
}

pub struct BteShadowAdapter {
    cfg: BackToExploreConfig,
    spot: VecDeque<SpotTick>,
    market: Option<MarketState>,
}

impl BteShadowAdapter {
    pub fn new(cfg: BackToExploreConfig) -> Self {
        Self {
            cfg,
            spot: VecDeque::with_capacity(8192),
            market: None,
        }
    }

    pub fn strategy_config(&self) -> BackToExploreConfig {
        self.cfg.clone()
    }

    pub fn on_spot_trade(&mut self, trade: SpotTrade) {
        self.spot.push_back(SpotTick {
            ts_ns: trade.ts_ns,
            price: trade.price,
            quantity: trade.quantity,
            is_buyer_maker: trade.is_buyer_maker,
        });
        let cutoff = trade.ts_ns - SPOT_RETENTION_NS;
        while self.spot.front().is_some_and(|front| front.ts_ns < cutoff) {
            self.spot.pop_front();
        }
    }

    pub fn on_market_open(&mut self, market_id: u32, open_ns: i64, close_ns: i64) -> bool {
        let spot_hist = self.spot_history();
        let Some(spot_at_open) = spot_hist
            .price_at_or_before(open_ns)
            .filter(|price| *price > 0.0)
        else {
            return false;
        };
        let mut cfg = self.cfg.clone();
        cfg.market_window_ns = (close_ns - open_ns).max(1);
        self.market = Some(MarketState {
            market_id,
            close_ns,
            spot_at_open,
            strategy: BackToExploreTaker::new(cfg),
            yes_min: f32::INFINITY,
            yes_max: f32::NEG_INFINITY,
            events_seen: 0,
        });
        true
    }

    pub fn on_market_close(&mut self) {
        self.market = None;
    }

    pub fn build_live_event(&self, tob: &YesTopOfBook) -> Option<ReplayEvent> {
        let market = self.market.as_ref()?;
        let spot_now = self
            .spot_history()
            .price_at_or_before(tob.ts_ns)
            .unwrap_or(market.spot_at_open) as f32;
        Some(build_replay_event(market.market_id, tob, spot_now))
    }

    pub fn on_decision(
        &mut self,
        tob: &YesTopOfBook,
        pos: BteDecisionPosition,
        regime: BteRegimeInputs,
        trades: &TradeHistory,
    ) -> Vec<BteShadowOrder> {
        let Some(event) = self.build_live_event(tob) else {
            return Vec::new();
        };
        self.on_decision_event(&event, pos, regime, trades)
    }

    pub fn on_decision_event(
        &mut self,
        event: &ReplayEvent,
        pos: BteDecisionPosition,
        regime: BteRegimeInputs,
        trades: &TradeHistory,
    ) -> Vec<BteShadowOrder> {
        let spot_hist = self.spot_history();
        let market_close_ns;
        let market_id;
        match &self.market {
            Some(market) => {
                market_close_ns = market.close_ns;
                market_id = market.market_id;
            }
            None => return Vec::new(),
        }

        let market = self.market.as_mut().expect("checked above");
        market.events_seen += 1;
        market.yes_min = market.yes_min.min(event.yes_mid);
        market.yes_max = market.yes_max.max(event.yes_mid);
        let yes_range_so_far = if market.yes_min.is_finite() && market.yes_max.is_finite() {
            market.yes_max - market.yes_min
        } else {
            0.0
        };

        let ctx = pm_strategy::Ctx {
            events_seen: pos.events_seen.max(market.events_seen),
            yes_shares: pos.yes_shares,
            no_shares: pos.no_shares,
            cash_usdc: pos.cash_usdc,
            market_yes_range_so_far: yes_range_so_far,
            regime_whipsaw_score: regime.whipsaw_score,
            regime_path_efficiency: regime.path_efficiency,
            regime_reversal_pressure: regime.reversal_pressure,
            regime_sign_flip_rate: regime.sign_flip_rate,
            regime_realized_vol_180s_bps: regime.realized_vol_180s_bps,
            market_close_ns,
            btc_net_exposure_shares: pos.btc_net_exposure_shares,
            eth_net_exposure_shares: pos.eth_net_exposure_shares,
            daily_start_cash_usdc: pos.daily_start_cash_usdc,
            daily_loss_cap_pct: pos.daily_loss_cap_pct,
            current_daily_loss_pct: pos.current_daily_loss_pct,
            ..pm_strategy::Ctx::default()
        };

        let out = market.strategy.on_event(event, &ctx, &spot_hist, trades);
        out.orders
            .iter()
            .map(|req| BteShadowOrder::from_request(market_id, req))
            .collect()
    }

    fn spot_history(&self) -> SpotHistory {
        SpotHistory::new(self.spot.iter().copied().collect())
    }
}

fn build_replay_event(market_id: u32, tob: &YesTopOfBook, spot_price: f32) -> ReplayEvent {
    let mut bids = [BookLevel::default(); TAPE_DEPTH];
    let mut asks = [BookLevel::default(); TAPE_DEPTH];
    bids[0] = BookLevel {
        price: tob.yes_bid,
        size: tob.yes_bid_size,
    };
    asks[0] = BookLevel {
        price: tob.yes_ask,
        size: tob.yes_ask_size,
    };
    let yes_mid = if tob.yes_bid > 0.0 && tob.yes_ask > 0.0 {
        0.5 * (tob.yes_bid + tob.yes_ask)
    } else if tob.yes_ask > 0.0 {
        tob.yes_ask
    } else {
        tob.yes_bid
    };
    ReplayEvent {
        ts_ns: tob.ts_ns,
        market_id: MarketId(market_id),
        yes_mid,
        yes_bid: tob.yes_bid,
        yes_ask: tob.yes_ask,
        volume: 0.0,
        bids,
        asks,
        spot_price,
        flags: ReplayFlags::BOOK_UPDATE,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ms(n: i64) -> i64 {
        n * 1_000_000
    }

    #[test]
    fn bte_shadow_emits_without_submitting() {
        let mut cfg = BackToExploreConfig {
            base_participation_rate: 1.0,
            two_sided_preference: 0.0,
            refresh_secs: 1.0,
            high_activity_hours: (0..=23).collect(),
            ..BackToExploreConfig::default()
        };
        cfg.max_clip_usdc = 8.0;
        let mut adapter = BteShadowAdapter::new(cfg);
        let open_ns = ms(1_000_000);
        let close_ns = open_ns + 300_000_000_000;
        adapter.on_spot_trade(SpotTrade {
            ts_ns: open_ns,
            price: 100.0,
            quantity: 1.0,
            is_buyer_maker: false,
        });
        adapter.on_spot_trade(SpotTrade {
            ts_ns: open_ns + 160_000_000_000,
            price: 100.1,
            quantity: 1.0,
            is_buyer_maker: false,
        });

        assert!(adapter.on_market_open(7, open_ns, close_ns));
        let orders = adapter.on_decision(
            &YesTopOfBook {
                ts_ns: open_ns + 160_000_000_000,
                yes_bid: 0.51,
                yes_bid_size: 500.0,
                yes_ask: 0.52,
                yes_ask_size: 500.0,
            },
            BteDecisionPosition {
                cash_usdc: 2_700.0,
                ..BteDecisionPosition::default()
            },
            BteRegimeInputs {
                path_efficiency: 1.0,
                ..BteRegimeInputs::default()
            },
            &TradeHistory::default(),
        );

        assert!(orders
            .iter()
            .all(|order| order.tag.starts_with("back_to_explore")));
    }
}
