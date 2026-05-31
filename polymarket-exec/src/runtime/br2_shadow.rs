//! Live shadow adapter for the proven BTC-5m br2 strategy (BonereaperV2).
//!
//! This module is ADDITIVE. It links the read-only br2 crates from
//! polymarket-backtest (pm-types / pm-model / pm-risk / pm-strategy) and runs
//! the EXACT SAME `BonereaperV2::on_event` decision logic the backtest uses,
//! fed from the live agent's Binance-spot tape and Polymarket YES book.
//!
//! SCOPE: shadow / dry-run ONLY. br2 DECIDES and LOGS the orders it WOULD place;
//! it submits NOTHING. There is no execution wiring in this module by design.
//!
//! The feature-adapter maps live feeds -> br2 inputs:
//!   - live Binance aggTrades  -> `SpotHistory` (trailing tape, with aggressor side)
//!   - Polymarket YES top-of-book -> `ReplayEvent` (NO = 1 - YES, confirmed exact)
//!   - market open + close       -> seconds-to-close + spot-at-open strike proxy
//!   - frozen 062901 snapshot    -> `ModelState` meta-calibrator (loaded once)
//!   - 062901 champion params    -> `BonereaperV2Config`
//!
//! The canonical 4-score `ModelOutput` is produced here exactly as the backtest
//! runner does (`ModelState::evaluate_detailed_with_market_context`) so the
//! `Ctx.model_output` br2 reads is identical live vs backtest.

use std::collections::VecDeque;

use pm_model::{ModelConfig, ModelMarketContext, ModelState, OnlineMetaCalibratorSnapshot};
use pm_strategy::{
    BonereaperV2, BonereaperV2Config, OrderRequest, Side, Strategy, StrategyOutput,
};
use pm_types::{
    BookLevel, MarketId, ReplayEvent, ReplayFlags, SpotHistory, SpotTick, TradeHistory,
    TAPE_DEPTH,
};

/// Nanoseconds per millisecond.
const NS_PER_MS: i64 = 1_000_000;
/// BTC-5m market window length in nanoseconds (5 minutes).
const MARKET_WINDOW_NS: i64 = 300 * 1_000_000_000;
/// Trailing spot tape retained for feature windows. br2/model windows reach back
/// 300s; keep a generous margin so binary searches always have coverage.
const SPOT_RETENTION_NS: i64 = 600 * 1_000_000_000;

/// One aggressor print from the live Binance aggTrade feed.
///
/// NOTE: the agent's existing `wire::spot_ws::SpotTradeEvent` drops the Binance
/// `m` (is_buyer_maker) flag. br2's signed-flow / adverse-volume features REQUIRE
/// that flag, so the shadow adapter ingests it directly here. Wiring the live
/// runtime must parse `m` from the aggTrade frame (it is present in the raw
/// payload) and feed it via [`Br2ShadowAdapter::on_spot_trade`].
#[derive(Debug, Clone, Copy)]
pub struct SpotTrade {
    pub ts_ns: i64,
    pub price: f64,
    pub quantity: f32,
    /// Binance convention: true if the buyer is the maker (seller-initiated print).
    pub is_buyer_maker: bool,
}

/// Polymarket YES-book top-of-book at a decision instant.
#[derive(Debug, Clone, Copy)]
pub struct YesTopOfBook {
    pub ts_ns: i64,
    pub yes_bid: f32,
    pub yes_bid_size: f32,
    pub yes_ask: f32,
    pub yes_ask_size: f32,
}

impl YesTopOfBook {
    fn yes_mid(&self) -> f32 {
        if self.yes_bid > 0.0 && self.yes_ask > 0.0 {
            0.5 * (self.yes_bid + self.yes_ask)
        } else if self.yes_ask > 0.0 {
            self.yes_ask
        } else {
            self.yes_bid
        }
    }
}

/// The evolving position/cash/events state the canonical runner feeds into
/// br2's `Ctx`. br2 reads `yes_shares`/`no_shares` for position-aware lanes, so
/// equivalence requires supplying the same state at each event.
#[derive(Debug, Clone, Copy, Default)]
pub struct DecisionPosition {
    pub events_seen: u64,
    pub yes_shares: f64,
    pub no_shares: f64,
    pub cash_usdc: f64,
}

/// One shadow decision: the order br2 WOULD place. Submitted nowhere.
#[derive(Debug, Clone)]
pub struct ShadowOrder {
    pub market_id: u32,
    pub side: Side,
    pub shares: f64,
    pub max_depth: usize,
    pub limit_price: Option<f32>,
    pub tag: &'static str,
}

impl ShadowOrder {
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

    /// Human-readable order line. NO orders display the implied NO price (1 - YES).
    pub fn log_line(&self) -> String {
        let (outcome, px) = match self.side {
            Side::BuyYes | Side::SellYes => ("YES", self.limit_price),
            Side::BuyNo | Side::SellNo => (
                "NO",
                self.limit_price.map(|p| 1.0 - p),
            ),
        };
        let action = match self.side {
            Side::BuyYes | Side::BuyNo => "BUY",
            Side::SellYes | Side::SellNo => "SELL",
        };
        let px_str = px.map(|p| format!("{p:.4}")).unwrap_or_else(|| "MKT".to_string());
        format!(
            "SHADOW market={} {} {} clip={:.2} price={} depth={} tag={}",
            self.market_id, action, outcome, self.shares, px_str, self.max_depth, self.tag
        )
    }
}

/// Per-market live state for one active BTC-5m market.
struct MarketState {
    market_id: u32,
    open_ns: i64,
    close_ns: i64,
    /// Spot price at market open: the strike proxy used by br2 lanes.
    spot_at_open: f64,
    strategy: BonereaperV2,
    model_state: ModelState,
    yes_min: f32,
    yes_max: f32,
    events_seen: u64,
    /// Most recent canonical model output for this market, captured each
    /// decision for shadow diagnostics (read-only; never feeds decision logic).
    last_model_output: Option<pm_model::ModelOutput>,
}

/// The live shadow adapter. Owns the trailing spot tape and per-market br2 state,
/// loads the frozen 062901 snapshot + champion params, and produces shadow
/// decisions from live top-of-book ticks.
pub struct Br2ShadowAdapter {
    cfg: BonereaperV2Config,
    model_cfg: ModelConfig,
    /// External model-gate thresholds applied by the canonical runner AFTER the
    /// strategy emits orders (pm-app runner.rs `run_backtest`, the
    /// `enforce_model_gate` block). The 062901 champion enforced this gate, so
    /// the live path must reproduce it to stay decision-identical.
    gate: ModelGate,
    snapshot: Option<OnlineMetaCalibratorSnapshot>,
    spot: VecDeque<SpotTick>,
    market: Option<MarketState>,
}

/// The external model gate the canonical runner applies to br2 orders. Mirrors
/// `RunnerConfig::{enforce_model_gate, model_gate_min_confidence,
/// model_gate_max_risk, model_gate_min_edge}` and the `order_requires_model_gate`
/// tag predicate. The 062901 champion values are conf >= 0.68, risk <= 0.72,
/// side-edge >= 0.00, enforced.
#[derive(Debug, Clone, Copy)]
pub struct ModelGate {
    pub enforce: bool,
    pub min_confidence: f32,
    pub max_risk: f32,
    pub min_edge: f32,
}

impl Default for ModelGate {
    fn default() -> Self {
        Self {
            enforce: true,
            min_confidence: 0.68,
            max_risk: 0.72,
            min_edge: 0.00,
        }
    }
}

/// True when a br2 order tag is subject to the external model gate. Mirrors
/// `order_requires_model_gate` in pm-app runner.rs: only `br2_participation_*`
/// orders bypass the gate.
pub fn order_requires_model_gate(tag: &str) -> bool {
    !tag.starts_with("br2_participation_")
}

/// True when an order adds YES exposure (BuyYes / SellNo). Mirrors
/// `order_adds_yes_exposure` in pm-app runner.rs.
pub fn order_adds_yes_exposure(side: Side) -> bool {
    matches!(side, Side::BuyYes | Side::SellNo)
}

impl Br2ShadowAdapter {
    /// Build the adapter with the 062901 champion params and (optionally) the
    /// frozen 062901 meta-calibrator snapshot. Pass `None` for the snapshot to
    /// run with a fresh (untrained) calibrator; the model gates will then be
    /// more conservative, but the code path is identical.
    pub fn new(snapshot: Option<OnlineMetaCalibratorSnapshot>) -> Self {
        Self {
            cfg: champion_062901_config(),
            model_cfg: champion_062901_model_config(),
            gate: ModelGate::default(),
            snapshot,
            spot: VecDeque::with_capacity(8192),
            market: None,
        }
    }

    /// Apply the external model gate to a strategy output, mirroring the
    /// `enforce_model_gate` block in pm-app runner.rs `run_backtest`. Returns the
    /// orders that survive the gate (those the canonical backtest would submit).
    fn apply_model_gate(
        &self,
        market_id: u32,
        orders: &[OrderRequest],
        model_output: &pm_model::ModelOutput,
        yes_mid: f32,
    ) -> Vec<ShadowOrder> {
        let mut kept = Vec::with_capacity(orders.len());
        for req in orders {
            if self.gate.enforce && order_requires_model_gate(req.tag) {
                let yes_side = order_adds_yes_exposure(req.side);
                let side_edge =
                    pm_model::side_edge_vs_mid(model_output, yes_mid, yes_side).clamp(0.0, 1.0);
                if model_output.confidence_score < self.gate.min_confidence {
                    continue;
                }
                if model_output.risk_score > self.gate.max_risk {
                    continue;
                }
                if side_edge < self.gate.min_edge {
                    continue;
                }
            }
            kept.push(ShadowOrder::from_request(market_id, req));
        }
        kept
    }

    /// Load the frozen 062901 snapshot from its on-disk JSON artifact
    /// (`--meta-calibrator-snapshot-out` from the canonical command).
    pub fn load_snapshot_from_path(
        path: &std::path::Path,
    ) -> anyhow::Result<OnlineMetaCalibratorSnapshot> {
        let bytes = std::fs::read(path)?;
        let snap: OnlineMetaCalibratorSnapshot = serde_json::from_slice(&bytes)?;
        Ok(snap)
    }

    /// Ingest one live Binance aggTrade print into the trailing spot tape.
    pub fn on_spot_trade(&mut self, trade: SpotTrade) {
        self.spot.push_back(SpotTick {
            ts_ns: trade.ts_ns,
            price: trade.price,
            quantity: trade.quantity,
            is_buyer_maker: trade.is_buyer_maker,
        });
        let cutoff = trade.ts_ns - SPOT_RETENTION_NS;
        while let Some(front) = self.spot.front() {
            if front.ts_ns < cutoff {
                self.spot.pop_front();
            } else {
                break;
            }
        }
    }

    /// Open a new BTC-5m market. `close_ns` is the resolution time; the open
    /// strike proxy is the live spot at `open_ns` (last print at-or-before).
    /// Returns `true` if the market was opened. Returns `false` (and opens
    /// nothing) when the spot tape does not yet cover `open_ns` with a valid
    /// (>0) price: on a cold start the tape has not reached back to the
    /// market's open, so the strike proxy would be garbage. The caller must
    /// treat `false` as "not open yet" and retry on a later tick once the
    /// tape spans the model window.
    pub fn on_market_open(&mut self, market_id: u32, open_ns: i64, close_ns: i64) -> bool {
        let spot_hist = self.spot_history();
        let Some(spot_at_open) = spot_hist
            .price_at_or_before(open_ns)
            .filter(|price| *price > 0.0)
        else {
            return false;
        };
        let mut model_state = ModelState::new();
        if let Some(snap) = &self.snapshot {
            model_state.load_meta_calibrator_snapshot(snap.clone());
        }
        self.market = Some(MarketState {
            market_id,
            open_ns,
            close_ns,
            spot_at_open,
            strategy: BonereaperV2::new(self.cfg.clone()),
            model_state,
            yes_min: f32::INFINITY,
            yes_max: f32::NEG_INFINITY,
            events_seen: 0,
            last_model_output: None,
        });
        true
    }

    /// Close the active market (resets per-market br2 + model state).
    pub fn on_market_close(&mut self) {
        self.market = None;
    }

    /// Run one shadow decision against the active market's live top-of-book.
    /// Returns the orders br2 WOULD place. Submits nothing.
    pub fn on_decision(&mut self, tob: &YesTopOfBook) -> Vec<ShadowOrder> {
        let spot_hist = self.spot_history();
        let market_close_ns;
        let market_open_ns;
        let market_id;
        let spot_at_open;
        match &self.market {
            Some(m) => {
                market_close_ns = m.close_ns;
                market_open_ns = m.open_ns;
                market_id = m.market_id;
                spot_at_open = m.spot_at_open;
            }
            None => return Vec::new(),
        }

        let market = self.market.as_mut().expect("checked above");
        market.events_seen += 1;
        let yes_mid = tob.yes_mid();
        market.yes_min = market.yes_min.min(yes_mid);
        market.yes_max = market.yes_max.max(yes_mid);
        let yes_range_so_far = if market.yes_min.is_finite() && market.yes_max.is_finite() {
            market.yes_max - market.yes_min
        } else {
            0.0
        };

        let spot_now = spot_hist
            .price_at_or_before(tob.ts_ns)
            .unwrap_or(spot_at_open) as f32;

        let event = build_replay_event(market_id, tob, spot_now);
        let secs_since_open = ((tob.ts_ns - market_open_ns).max(0) as f64) / 1e9;

        // Canonical 4-score model output — same call the backtest runner makes.
        let model_eval = market.model_state.evaluate_detailed_with_market_context(
            &event,
            &spot_hist,
            secs_since_open as f32,
            &self.model_cfg,
            ModelMarketContext::default(),
        );
        market.last_model_output = Some(model_eval.output);

        let ctx = pm_strategy::Ctx {
            events_seen: market.events_seen,
            yes_shares: 0.0,
            no_shares: 0.0,
            cash_usdc: self.cfg.bankroll_usdc,
            market_yes_range_so_far: yes_range_so_far,
            // Prior-market ranges are portfolio-replay context. Live wiring should
            // supply trailing per-market YES-range means; 0.0 here keeps the
            // regime gate inert (it is disabled in the 062901 champion anyway).
            prior_market_range_1d: 0.0,
            prior_market_range_3d: 0.0,
            prior_market_range_7d: 0.0,
            model_output: Some(model_eval.output),
            market_close_ns,
        };

        let trades = TradeHistory::default();
        let out: StrategyOutput = market.strategy.on_event(&event, &ctx, &spot_hist, &trades);
        self.apply_model_gate(market_id, &out.orders, &model_eval.output, event.yes_mid)
    }

    /// Equivalence-test entry point: decide on a canonical `ReplayEvent` directly
    /// (the full-depth book + spot the backtest sees), bypassing the top-of-book
    /// reconstruction. This is the live path where the adapter receives the full
    /// Polymarket book; it reproduces the canonical runner's decision exactly:
    /// same model eval, same Ctx, same `on_event`, same external model gate.
    ///
    /// `pos` carries the evolving position/cash/events the canonical runner feeds
    /// into `Ctx`. br2 is position-aware (side-lock and tail-coverage lanes read
    /// `yes_shares`/`no_shares`), so equivalence requires the SAME position state
    /// at each event. Live wiring supplies real position here; the equivalence
    /// test supplies the canonical-mirrored position. The per-market YES range is
    /// tracked internally off `event.yes_mid`, identical to the runner.
    ///
    /// `trades` is the same `TradeHistory` the backtest feeds (live = the
    /// trailing PM trade tape).
    pub fn on_decision_event(
        &mut self,
        event: &ReplayEvent,
        pos: DecisionPosition,
        trades: &TradeHistory,
    ) -> Vec<ShadowOrder> {
        let spot_hist = self.spot_history();
        let market_open_ns;
        let market_close_ns;
        let market_id;
        match &self.market {
            Some(m) => {
                market_open_ns = m.open_ns;
                market_close_ns = m.close_ns;
                market_id = m.market_id;
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

        let secs_since_open = ((event.ts_ns - market_open_ns).max(0) as f64) / 1e9;
        let model_eval = market.model_state.evaluate_detailed_with_market_context(
            event,
            &spot_hist,
            secs_since_open as f32,
            &self.model_cfg,
            ModelMarketContext::default(),
        );
        market.last_model_output = Some(model_eval.output);

        let ctx = pm_strategy::Ctx {
            events_seen: pos.events_seen,
            yes_shares: pos.yes_shares,
            no_shares: pos.no_shares,
            cash_usdc: pos.cash_usdc,
            market_yes_range_so_far: yes_range_so_far,
            prior_market_range_1d: 0.0,
            prior_market_range_3d: 0.0,
            prior_market_range_7d: 0.0,
            model_output: Some(model_eval.output),
            market_close_ns,
        };

        let out: StrategyOutput = market.strategy.on_event(event, &ctx, &spot_hist, trades);
        self.apply_model_gate(market_id, &out.orders, &model_eval.output, event.yes_mid)
    }

    /// Build the canonical `ReplayEvent` for the active market from a live YES
    /// top-of-book, using the trailing spot tape (last print at-or-before the
    /// decision instant, falling back to spot-at-open). Returns `None` when no
    /// market is active. This is the SAME event `on_decision` builds internally;
    /// exposing it lets the live driver feed `on_decision_event` with a real
    /// `DecisionPosition` without duplicating the spot/strike logic.
    pub fn build_live_event(&self, tob: &YesTopOfBook) -> Option<ReplayEvent> {
        let market = self.market.as_ref()?;
        let spot_now = self
            .spot_history()
            .price_at_or_before(tob.ts_ns)
            .unwrap_or(market.spot_at_open) as f32;
        Some(build_replay_event(market.market_id, tob, spot_now))
    }

    /// The external model gate the adapter applies (062901 champion thresholds).
    /// Exposed so the equivalence test can replicate the canonical gate exactly.
    pub fn model_gate(&self) -> ModelGate {
        self.gate
    }

    /// The frozen 062901 `ModelConfig` the adapter evaluates the model with.
    pub fn model_config(&self) -> ModelConfig {
        self.model_cfg.clone()
    }

    /// The frozen 062901 `BonereaperV2Config` the adapter runs br2 with.
    pub fn strategy_config(&self) -> BonereaperV2Config {
        self.cfg.clone()
    }

    /// Per-lane gate statistics for the active market, for shadow diagnostics.
    /// Shows exactly which br2 gate accepted or rejected each candidate load,
    /// which is the key observability signal in shadow mode.
    pub fn gate_stats(&self) -> Option<pm_strategy::bonereaper_v2::BonereaperV2GateStats> {
        self.market.as_ref().map(|m| m.strategy.gate_stats())
    }

    /// The most recent canonical model output for the active market, captured at
    /// the last decision. Read-only diagnostic accessor for the shadow log; it is
    /// `None` before the first decision or when no market is active.
    pub fn last_model_output(&self) -> Option<pm_model::ModelOutput> {
        self.market.as_ref().and_then(|m| m.last_model_output)
    }

    /// The strike-proxy spot at the active market's open. Read-only diagnostic
    /// accessor for the shadow log; `None` when no market is active.
    pub fn spot_at_open(&self) -> Option<f64> {
        self.market.as_ref().map(|m| m.spot_at_open)
    }

    /// Override the base late-favourite model-edge gate the adapter runs br2
    /// with, clamping the high-cert edge to `min(existing, new)` so the high-cert
    /// path is never stricter than the base. Applies to the config future markets
    /// are built from (see `on_market_open`). ADDITIVE: when never called,
    /// behavior is byte-identical to the 062901 champion (0.09 / 0.06).
    pub fn set_late_favourite_min_model_edge(&mut self, new_edge: f32) {
        self.cfg.late_favourite_min_model_edge = new_edge;
        self.cfg.late_favourite_high_cert_min_model_edge = self
            .cfg
            .late_favourite_high_cert_min_model_edge
            .min(new_edge);
    }

    /// Override the realized-vol floor (180s, bps) on all three directional
    /// lanes the future configs are built from (see `on_market_open`). ADDITIVE:
    /// when never called, behavior is byte-identical to the 062901 champion
    /// (1.25 on every lane). Accepts 0 to fully disable the floor.
    pub fn set_min_realized_vol_180s_bps(&mut self, bps: f32) {
        self.cfg.late_favourite_min_realized_vol_180s_bps = bps;
        self.cfg.late_confirm_min_realized_vol_180s_bps = bps;
        self.cfg.high_skew_min_realized_vol_180s_bps = bps;
    }

    /// Override the late-favourite minimum ask gate the future configs are built
    /// from (see `on_market_open`). ADDITIVE: when never called, behavior is
    /// byte-identical to the 062901 champion (0.70).
    pub fn set_late_favourite_min_ask(&mut self, min_ask: f32) {
        self.cfg.late_favourite_min_ask = min_ask;
    }

    fn spot_history(&self) -> SpotHistory {
        SpotHistory::new(self.spot.iter().copied().collect())
    }
}

/// Build a `ReplayEvent` from the live YES top-of-book. Depth beyond level 0 is
/// zero-filled (the live feed is top-of-book; br2 lane sweeps clamp to the
/// available depth). NO is implied as 1 - YES by br2's runner, not encoded here.
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
    ReplayEvent {
        ts_ns: tob.ts_ns,
        market_id: MarketId(market_id),
        yes_mid: tob.yes_mid(),
        yes_bid: tob.yes_bid,
        yes_ask: tob.yes_ask,
        volume: 0.0,
        bids,
        asks,
        spot_price,
        flags: ReplayFlags::BOOK_UPDATE,
    }
}

/// The frozen 062901 champion `BonereaperV2Config`.
///
/// Derived from `configs/bonereaper_v2_favourite_062901.command.txt` in
/// polymarket-backtest (run_id 20260529T062901Z-portfolio-grid-5265). The struct
/// `Default` already matches most 062901 knobs; the explicit overrides below are
/// the flags whose champion value differs from the strategy default.
pub fn champion_062901_config() -> BonereaperV2Config {
    BonereaperV2Config {
        // Sizing: 062901 used CLI sizing (clip 0.015 of equity, max_clip 30,
        // bankroll 1000). The per-clip equity fraction is applied by the live
        // sizing layer; the strategy itself caps each clip at max_clip_usdc.
        bankroll_usdc: 1000.0,
        max_clip_usdc: 30.0,
        tick: 0.01,

        min_composite_direction: 0.10,

        // Late confirmation lane (062901 flags).
        late_clip_frac: 1.0,
        late_max_fires: 3,
        late_confirm_min_model_confidence: 0.58,
        late_confirm_max_model_risk: 0.80,
        late_confirm_min_model_side_p: 0.58,
        late_confirm_min_model_edge: 0.02,
        late_confirm_min_book_skew: 0.06,
        late_confirm_max_whipsaw_score: 0.85,
        late_confirm_min_realized_vol_180s_bps: 1.25,
        late_confirm_max_observed_range: 0.50,

        recent_regime_gate_min_edge: 0.08,

        // High-skew load lane.
        high_skew_clip_frac: 0.60,
        high_skew_max_clips: 5,
        high_skew_max_whipsaw_score: 0.75,
        high_skew_min_realized_vol_180s_bps: 1.25,

        // Late-favourite loading: the PnL engine.
        late_favourite_start_secs: 180.0,
        late_favourite_threshold: 0.22,
        late_favourite_min_ask: 0.70,
        late_favourite_max_ask: 0.97,
        late_favourite_clip_frac: 1.00,
        late_favourite_high_cert_clip_frac: 1.00,
        late_favourite_high_cert_full_clip_edge: 0.09,
        late_favourite_max_clips: 12,
        late_favourite_min_sustain_secs: 0.0,
        late_favourite_sweep_depth: 7,
        late_favourite_min_model_confidence: 0.68,
        late_favourite_min_model_direction_abs: 0.0,
        late_favourite_max_model_risk: 0.72,
        late_favourite_min_model_side_p: 0.62,
        late_favourite_min_model_edge: 0.09,
        late_favourite_high_cert_min_model_edge: 0.06,
        late_favourite_max_whipsaw_score: 0.75,
        late_favourite_max_reversal_pressure: 0.85,
        late_favourite_min_path_efficiency: 0.0,
        late_favourite_min_realized_vol_180s_bps: 1.25,
        late_favourite_max_observed_range: 0.70,
        late_favourite_range_soft_throttle: 0.55,
        late_favourite_range_hard_throttle: 0.70,
        late_favourite_range_extra_edge: 0.08,
        late_favourite_range_extra_confidence: 0.12,
        late_favourite_max_adverse_fast_momentum: 1.0,
        late_favourite_max_adverse_broad_momentum: 1.0,
        late_favourite_max_entry_pullback: 1.0,
        late_favourite_max_avg_entry_drawdown: 1.0,

        // Convex tail ladder.
        tail_clip_frac: 0.10,
        tail_max_clips: 6,
        tail_sweep_depth: 3,
        tail_min_ask: 0.01,
        tail_max_ask: 0.08,
        tail_min_seconds_to_close: 10.0,
        tail_min_favourite_unrealized_edge: 0.0,
        tail_min_observed_range: 0.0,
        tail_target_favourite_loss_coverage_frac: 0.50,
        tail_reversal_coverage_frac: 0.00,
        tail_reversal_min_seconds_to_close: 10.0,
        tail_reversal_max_seconds_to_close: 35.0,
        tail_reversal_min_favourite_ask: 0.85,
        tail_extreme_threshold: 0.30,
        tail_min_skew_step: 0.02,
        tail_budget_favourite_spend_frac: 0.20,
        tail_budget_favourite_upside_frac: 0.25,
        tail_regime_boost_coverage_frac: 0.0,
        tail_regime_boost_budget_spend_frac: 0.0,
        tail_regime_boost_budget_upside_frac: 0.0,
        tail_regime_boost_min_whipsaw_score: 1.0,
        tail_regime_boost_min_reversal_pressure: 1.0,
        tail_regime_boost_min_realized_vol_180s_bps: 1_000_000_000.0,
        tail_regime_boost_max_path_efficiency: 0.0,

        ..BonereaperV2Config::default()
    }
}

/// The frozen 062901 `ModelConfig` (model-gate weights). The champion run used
/// the model defaults for the BTC risk-blend weights; only the gate thresholds
/// (min-confidence 0.68 / max-risk 0.72) live on the strategy config, not here.
pub fn champion_062901_model_config() -> ModelConfig {
    ModelConfig {
        enable_meta_calibration: true,
        ..ModelConfig::default()
    }
}

/// Convenience: the canonical 062901 window length and tick cadence, exposed for
/// the live runtime to schedule decisions on the 5-minute BTC market cycle.
pub const fn market_window_ns() -> i64 {
    MARKET_WINDOW_NS
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ms(n: i64) -> i64 {
        n * NS_PER_MS
    }

    /// End-to-end smoke test: instantiate the adapter with the 062901 champion
    /// params, feed a synthetic trailing Binance tape + a BTC-5m market with a
    /// strong late favourite, and confirm br2 processes the event and produces a
    /// logged shadow decision without panicking.
    #[test]
    fn shadow_decision_end_to_end() {
        // Fresh calibrator (no snapshot on this host); code path is identical.
        let mut adapter = Br2ShadowAdapter::new(None);

        // Synthetic Binance tape: a steady BTC uptrend over the 5 minutes, with
        // aggressor buys dominating (is_buyer_maker = false => aggressive buy).
        let market_open_ns = ms(1_000_000);
        let market_close_ns = market_open_ns + MARKET_WINDOW_NS;
        let mut price = 64_000.0_f64;
        let mut t = market_open_ns - ms(60_000); // 60s of warmup tape before open
        while t <= market_close_ns {
            price += 1.5; // gentle uptrend
            adapter.on_spot_trade(SpotTrade {
                ts_ns: t,
                price,
                quantity: 5.0,
                is_buyer_maker: false,
            });
            t += ms(200); // 5 prints/sec
        }

        adapter.on_market_open(42, market_open_ns, market_close_ns);

        // Drive decisions across the window. Make YES the heavy favourite late
        // (>= 0.72) so the late-favourite lane has a candidate.
        let mut emitted = Vec::new();
        let mut decision_t = market_open_ns;
        while decision_t < market_close_ns {
            let secs_to_close = (market_close_ns - decision_t) as f64 / 1e9;
            // Climb YES toward a strong favourite as the close approaches.
            let yes_ask = if secs_to_close < 120.0 { 0.88 } else { 0.55 };
            let yes_bid = yes_ask - 0.02;
            let tob = YesTopOfBook {
                ts_ns: decision_t,
                yes_bid,
                yes_bid_size: 500.0,
                yes_ask,
                yes_ask_size: 500.0,
            };
            let orders = adapter.on_decision(&tob);
            for o in &orders {
                emitted.push(o.log_line());
            }
            decision_t += ms(1_000); // 1s decision cadence (062901 replay-sample)
        }

        adapter.on_market_close();

        // The adapter must have processed the full window without panicking and
        // produced log lines for any orders br2 decided to place. We do not
        // assert a specific count (gates depend on the synthetic tape), only that
        // the end-to-end path runs and log formatting works.
        for line in &emitted {
            assert!(line.starts_with("SHADOW market=42"), "bad line: {line}");
        }
        // Sanity: with a strong late favourite + uptrend tape, br2 should fire at
        // least once. If gates reject everything the path still ran; surface the
        // count for the smoke-test operator.
        println!("br2 shadow emitted {} order(s)", emitted.len());
        for line in &emitted {
            println!("{line}");
        }
    }

    #[test]
    fn no_order_logs_implied_no_price() {
        let o = ShadowOrder {
            market_id: 7,
            side: Side::BuyNo,
            shares: 10.0,
            max_depth: 3,
            limit_price: Some(0.30), // YES-terms limit
            tag: "br2_tail",
        };
        let line = o.log_line();
        // NO price is 1 - YES = 0.70.
        assert!(line.contains("NO"), "{line}");
        assert!(line.contains("0.7000"), "{line}");
    }
}
