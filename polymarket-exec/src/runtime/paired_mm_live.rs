//! Live glue for the calm-regime PAIRED market-making overlay.
//!
//! INC1 was SHADOW-ONLY. INC2 adds a PAPER submission arm behind a hard guard
//! that mirrors br2's paper arm EXACTLY: the paired-MM may convert its two-sided
//! touch quotes into MAKER (post_only LIMIT) order intents and rest them through
//! the runtime's existing tracked-submit path ONLY when
//! `PM_BTC_5M_PAIRED_MM_PAPER_TRADE` is truthy AND the runtime is in paper mode
//! (`config.paper_mode == true`). There is NO real-money arm in this increment:
//! if the paper-trade flag is set while the runtime is NOT in paper mode we WARN
//! and REFUSE, leaving the overlay shadow-only. Real-money submission is
//! IMPOSSIBLE here (there is no live arm and no live submit path).
//!
//! When NOT (paper_mode && armed), behavior is byte-identical to INC1: the
//! overlay decides + logs the two-sided quotes and a simulated pairing/inventory/
//! PnL it WOULD place, and SUBMITS NOTHING.
//!
//! This module is ADDITIVE. It mirrors the structure of
//! [`crate::runtime::br2_live`] (an env-gated, shadow-logging runtime overlay
//! driven by the Binance spot tap + the Polymarket book tick), but implements
//! the VALIDATED two-sided touch-quoting paired-MM the backtest settled on (see
//! `polymarket-backtest/scripts/mm_paired_sim.py` and
//! `.claude/paired_mm_overlay_build_plan.md`). It DECIDES + LOGS the two-sided
//! quotes and the simulated pairing / inventory / PnL it WOULD place. No order
//! submission, no [`OrderIntent`] construction, no execution-path interaction.
//!
//! Behavior (all from the build plan, not re-derived):
//!   - TWO-SIDED TOUCH quoting at best_bid / best_ask, tiny clips (~10-20 sh,
//!     configurable), TOUCH-ONLY (no laddering: deeper rungs don't fill on the
//!     1c spread), continuous requote each tick.
//!   - STRICT PAIRING: track matched Up+Down pairs vs unmatched residual; HARD
//!     residual cap (~5% net skew via a repair band on |yes_long - no_long|);
//!     when one-sided, SKEW to re-pair (pull the leading leg, keep quoting the
//!     missing side), NEVER trade out of a stranded leg. Matched pairs realize
//!     to $1 (redeem). Inventory + PnL simulated internally.
//!   - LATE-PULL: stop quoting in the last ~45-60s (configurable).
//!   - REGIME GATE: quote only when low spot vol + narrow YES range-so-far +
//!     healthy sign-flip + mid in ~0.30-0.70. Computed from a self-contained
//!     trailing spot tape (fed by `on_spot_trade`) exactly as the reference
//!     Python (30s-grid returns: stdev = vol, fraction-of-sign-flips = flips).
//!   - SIMULATED FILL: a conservative pro-rata clip/(clip+depth_ahead) model.
//!     Because the runtime loop surfaces only book SNAPSHOTS (not individual
//!     taker prints), a taker print is inferred from a change in the YES book's
//!     `last_trade_price` between ticks and classified against OUR resting
//!     touch quotes (print <= our bid => taker SELL hits our bid; print >= our
//!     ask => taker BUY lifts our ask). depth_ahead is the resting top-of-book
//!     size at that level. This is the faithful adaptation of the tape model to
//!     the snapshot stream available at the wiring point (see the module-level
//!     note in the report; it is the one deliberate simplification).
//!
//! Gating: OFF by default. [`PairedMmLiveShadow::from_env`] returns `None`
//! unless `PM_BTC_5M_PAIRED_MM_SHADOW` is truthy, so with the flag unset the
//! runtime does NO paired-MM work at all and the existing live path is
//! byte-identical. There is NO paper or live arm in INC1: this overlay can only
//! log.
//!
//! Regime-disjoint wiring (see runtime/runner.rs): the overlay is driven only
//! when br2 is NOT quoting that market. The caller passes `br2_quoting` (true
//! when the br2 driver produced any order for this market this tick); when set,
//! the overlay records the abstention and skips quoting, so the two strategies
//! never double-quote the same market.

use std::collections::HashMap;
use std::collections::VecDeque;

use tracing::{info, warn};

use crate::book::BookState;
use crate::market_context::MarketContextRecord;
use crate::types::{ClientOrderId, InstrumentId, MarketId, OrderIntent};

/// Quote-level tag stamped on every paired-MM maker intent. Distinct prefix so
/// the accounting lane / MM gates never confuse it with another strategy's
/// quotes, and so a reader can attribute paper fills to this overlay.
const MM_QUOTE_TAG: &str = "pairedmm-maker";

/// Minimum spacing between paired-MM shadow decisions for one market. Mirrors
/// br2's 1s book cadence so decisions are comparable and the log is not spammed
/// on every book delta.
const DECISION_CADENCE_MS: u64 = 1_000;

// Defaults transcribed from scripts/mm_paired_sim.py (the validated config).
const DEFAULT_CLIP_SHARES: f64 = 10.0;
const ACTIVE_WIN_SECS: f64 = 300.0; // quote over the last 5 minutes
const REGIME_WARMUP_SECS: f64 = 60.0; // need >=60s of history before quoting
const REGIME_MID_LO: f64 = 0.30;
const REGIME_MID_HI: f64 = 0.70;
const REGIME_RANGE_MAX: f64 = 0.06; // YES mid range-so-far must be <= 6c
const REGIME_SPOT_VOL_MAX: f64 = 0.00012; // max stdev of 30s-grid spot returns
const REGIME_FLIP_MIN: f64 = 0.20; // min fraction of spot-return sign flips
const LATE_PULL_SECS: f64 = 45.0; // pull both legs in the last N seconds
const REPAIR_DELTA_SHARES: f64 = 2.0; // residual-cap band on |yes_long - no_long|
const RESIDUAL_CAP_FRAC: f64 = 0.05; // target residual <= 5% of paired volume (logged)
const TAKER_FEE_FRAC: f64 = 0.0156;
const REBATE_FRAC: f64 = 0.20 * TAKER_FEE_FRAC; // maker rebate ~20% of taker fee, on notional

/// A trailing spot tape entry (price at a wall-clock ms). Kept self-contained so
/// the regime gate is computed exactly like the reference Python and does not
/// depend on the runtime's own (differently-windowed) signal store.
#[derive(Clone, Copy)]
struct SpotSample {
    ts_ms: u64,
    price: f64,
}

/// Simulated paired inventory + realized/marked PnL for one BTC-5m market.
///
/// `yes_long` = YES shares bought on our resting bid; `no_long` = NO shares
/// acquired by selling YES on our resting ask (a YES short == a NO long, the
/// single-mirrored-book identity the backtest uses). Matched pairs redeem to $1.
#[derive(Default, Clone)]
struct SimInventory {
    yes_long: f64,
    no_long: f64,
    yes_long_cost: f64, // $ paid for YES
    no_long_cost: f64,  // $ paid for NO (= 1 - our_yes_ask)
    rebate_usdc: f64,
    n_fills: u64,
    filled_shares: f64,
}

impl SimInventory {
    /// Net skew between the two legs. Drives the strict-pairing repair.
    fn skew(&self) -> f64 {
        self.yes_long - self.no_long
    }

    /// Matched pairs (each redeems to $1) and unmatched residual shares.
    fn paired(&self) -> f64 {
        self.yes_long.min(self.no_long)
    }

    fn residual_shares(&self) -> f64 {
        (self.yes_long - self.paired()) + (self.no_long - self.paired())
    }

    fn residual_frac(&self) -> f64 {
        let total = self.yes_long + self.no_long;
        if total > 0.0 {
            self.residual_shares() / total
        } else {
            0.0
        }
    }

    /// Realized PnL if the market resolved now: matched pairs realize
    /// (1 - pair_cost) each; the unmatched residual is marked to the running
    /// YES mid (NOT traded out, matching the strict-pairing rule). Rebate added
    /// only when `rebate_on`.
    ///
    /// The residual mark is hardened against degenerate/stale books: a garbage
    /// `yes_mid` (NaN, or far outside [0,1] from a transient bad spread) would
    /// otherwise mark the residual at a nonsensical price and blow the log up.
    /// `yes_mid` and the per-share avg costs are clamped to [0,1], and each
    /// per-share residual mark is clamped to [-1, +1] so a residual of N shares
    /// can never contribute more than N (in magnitude) to PnL.
    fn marked_pnl(&self, yes_mid: f64, rebate_on: bool) -> f64 {
        // Degenerate/stale mid (NaN or outside the share-price domain): fall
        // back to a neutral 0.5 rather than marking at a garbage price.
        let mid = if yes_mid.is_finite() {
            yes_mid.clamp(0.0, 1.0)
        } else {
            0.5
        };
        let paired = self.paired();
        let yes_avg = if self.yes_long > 0.0 {
            (self.yes_long_cost / self.yes_long).clamp(0.0, 1.0)
        } else {
            0.0
        };
        let no_avg = if self.no_long > 0.0 {
            (self.no_long_cost / self.no_long).clamp(0.0, 1.0)
        } else {
            0.0
        };
        let mut pnl = 0.0;
        if paired > 0.0 {
            pnl += paired * (1.0 - (yes_avg + no_avg));
        }
        let res_yes = self.yes_long - paired;
        let res_no = self.no_long - paired;
        // Residual marked to mid (YES leg at mid, NO leg at 1 - mid). The
        // per-share mark is bounded to [-1, +1] so |residual PnL| <= shares.
        if res_yes > 0.0 {
            pnl += res_yes * (mid - yes_avg).clamp(-1.0, 1.0);
        }
        if res_no > 0.0 {
            pnl += res_no * ((1.0 - mid) - no_avg).clamp(-1.0, 1.0);
        }
        if rebate_on {
            pnl += self.rebate_usdc;
        }
        pnl
    }
}

/// One resting maker quote leg the paper arm is managing. Tracks the live
/// client_order_id and the price we posted it at so the next tick can decide
/// keep-vs-cancel/replace as the touch moves.
#[derive(Clone)]
struct RestingLeg {
    client_order_id: ClientOrderId,
    price: f64,
}

/// Tracks the runtime-active BTC-5m market the overlay is currently driving.
struct ActiveMarket {
    market_id: MarketId,
    yes_asset_id: String,
    /// NO instrument id (`instrument_ids[1]`), if the market exposes one. The
    /// paper arm rests the NO leg here (BUY NO @ 1 - best_ask).
    no_asset_id: Option<String>,
    close_ms: u64,
    last_decision_ms: u64,
    /// Running min/max of the YES mid since open (range-so-far for the gate).
    mid_min: f64,
    mid_max: f64,
    /// Last YES `last_trade_price` we observed, to detect a fresh taker print.
    last_trade_price: f64,
    /// SHADOW (off-path) simulated inventory. Unused on the paper arm, which
    /// reads the REAL runtime position instead.
    inventory: SimInventory,
    /// Resting YES-bid maker leg (BUY YES @ best_bid), when one is live.
    bid_leg: Option<RestingLeg>,
    /// Resting NO-bid maker leg (BUY NO @ 1 - best_ask), when one is live.
    no_leg: Option<RestingLeg>,
    /// Monotonic counter for unique client_order_ids on this market.
    quote_seq: u64,
}

/// The REAL runtime paper position for the active market, threaded into the
/// paper arm so strict-pairing/residual-cap is computed from genuine fills (not
/// the INC1 simulated estimate). `yes_shares`/`no_shares` are the YES/NO token
/// share counts (`instrument_ids[0]` / `[1]`).
#[derive(Debug, Default, Clone, Copy)]
pub struct PairedPosition {
    pub yes_shares: f64,
    pub no_shares: f64,
}

impl PairedPosition {
    /// Net skew between the two legs (real positions), driving the repair gate.
    fn skew(&self) -> f64 {
        self.yes_shares - self.no_shares
    }
}

/// Per-tick quote/fill decision, logged by [`PairedMmLiveShadow::decide_tick`].
#[derive(Debug, Default, Clone)]
pub struct PairedMmTickResult {
    /// Whether the overlay was quoting (gates all passed, at least one leg live).
    pub quoting: bool,
    /// The bid touch price it would post (None when the bid leg was pulled).
    pub bid_price: Option<f64>,
    /// The ask touch price it would post (None when the ask leg was pulled).
    pub ask_price: Option<f64>,
    /// Maker quote intents to REST through the runtime's tracked-submit path.
    /// Non-empty ONLY when the paper arm is armed (which requires paper_mode), so
    /// the caller never submits anything outside paper mode. Each is a post_only
    /// BUY (YES @ best_bid, or NO @ 1 - best_ask) that rests below the mid.
    pub submit_intents: Vec<OrderIntent>,
    /// Client order ids of resting maker legs to CANCEL this tick (the touch
    /// moved, or the leg was pulled). Paired with `submit_intents` to realize a
    /// cancel/replace. Empty unless the paper arm is armed.
    pub cancel_ids: Vec<ClientOrderId>,
}

/// Runtime-side driver that feeds the paired-MM shadow overlay from live feeds.
/// SUBMITS NOTHING: it only decides + logs.
pub struct PairedMmLiveShadow {
    active: Option<ActiveMarket>,
    /// True when `PM_BTC_5M_PAIRED_MM_PAPER_TRADE` is armed AND the runtime is in
    /// paper mode. The ONLY switch that lets `decide_tick` emit maker submit/
    /// cancel intents. Can never be true outside paper mode (enforced in
    /// `from_env`); there is no real-money arm in this increment, so real
    /// submission is impossible.
    paper_trade_armed: bool,
    clip_shares: f64,
    rebate_on: bool,
    /// Regime-gate thresholds + residual cap, env-overridable (defaults transcribed
    /// from the validated config). Set the per-gate sentinels to disable a gate.
    mid_lo: f64,
    mid_hi: f64,
    range_max: f64,
    spot_vol_max: f64,
    flip_min: f64,
    late_pull_secs: f64,
    repair_delta: f64,
    /// Self-contained trailing spot tape for the regime gate.
    spot: VecDeque<SpotSample>,
    /// One-shot guard so the "br2 owns this market" disjoint note logs sparingly.
    disjoint_skip_markets: HashMap<String, u64>,
}

impl PairedMmLiveShadow {
    /// Construct the overlay IFF `PM_BTC_5M_PAIRED_MM_SHADOW` is truthy. Returns
    /// `None` (no-op) otherwise so the runtime path stays byte-identical.
    ///
    /// `paper_mode` is the runtime's effective paper-mode flag (read from
    /// AppConfig by the caller). It HARD-GATES the paper arm: paper submission
    /// (`PM_BTC_5M_PAIRED_MM_PAPER_TRADE`) requires `paper_mode == true`. If the
    /// flag is set while the runtime is NOT in paper mode we WARN loudly and
    /// leave the arm disabled (shadow logging still runs). There is NO real-money
    /// arm in this increment, so real submission is impossible regardless of any
    /// env flag.
    ///
    /// The two optional knobs are tuning (apply to both shadow and paper):
    ///   - `PM_BTC_5M_PAIRED_MM_CLIP_SHARES` (f64 > 0): touch clip size; default 10.
    ///   - `PM_BTC_5M_PAIRED_MM_REBATE` (truthy): include the maker rebate in the
    ///     simulated PnL (the backtest is positive at rebate=0; rebate is upside).
    ///
    /// The regime gates + residual cap are ALSO env-overridable (defaults are the
    /// validated config, so unset == byte-identical). Each gate is independently
    /// disable-able by setting its sentinel:
    ///   - `PM_BTC_5M_PAIRED_MM_RANGE_MAX` (f64 >= 0; default 0.06): YES mid
    ///     range-so-far cap. Set >= 1.0 to disable (range can never exceed 1).
    ///   - `PM_BTC_5M_PAIRED_MM_MID_LO` / `_MID_HI` (f64 in [0,1]; default 0.30 /
    ///     0.70): mid band. Set 0.0 / 1.0 to disable.
    ///   - `PM_BTC_5M_PAIRED_MM_VOL_MAX` (f64 >= 0; default 0.00012): spot-vol cap.
    ///     Set huge (e.g. 1e9) to disable.
    ///   - `PM_BTC_5M_PAIRED_MM_FLIP_MIN` (f64 >= 0; default 0.20): min sign-flip
    ///     fraction. Set 0.0 to disable.
    ///   - `PM_BTC_5M_PAIRED_MM_RESIDUAL_CAP_SHARES` (f64 > 0; default 2.0): the
    ///     hard repair-band share cap on |yes_long - no_long|.
    ///   - `PM_BTC_5M_PAIRED_MM_RESIDUAL_CAP_FRAC` (f64 >= 0; default 0.05): logged
    ///     target residual fraction.
    ///   - `PM_BTC_5M_PAIRED_MM_LATE_PULL_SECS` (f64 >= 0; default 45): pull both
    ///     legs in the last N seconds.
    pub fn from_env(paper_mode: bool) -> Option<Self> {
        if !env_truthy("PM_BTC_5M_PAIRED_MM_SHADOW") {
            return None;
        }
        // PAPER arm. Mirrors br2's hard guard exactly: armed IFF the flag is
        // truthy AND the runtime is in paper mode. If set in the wrong mode we
        // WARN and refuse (shadow-only). No real-money arm exists this increment.
        let paper_trade_requested = env_truthy("PM_BTC_5M_PAIRED_MM_PAPER_TRADE");
        let paper_trade_armed = paper_trade_requested && paper_mode;
        if paper_trade_requested && !paper_mode {
            warn!(
                target: "paired_mm",
                "PAIRED-MM PAPER-TRADE flag PM_BTC_5M_PAIRED_MM_PAPER_TRADE is set but the runtime \
                 is NOT in paper mode; REFUSING submission. The paired-MM stays SHADOW-ONLY (log \
                 only). There is no real-money arm; real submission is impossible."
            );
        }
        if paper_trade_armed {
            info!(
                target: "paired_mm",
                "PAIRED-MM PAPER-TRADE armed: maker quotes WILL rest through the paper-fill \
                 simulator (paper_mode=true). No real-money submission occurs."
            );
        }
        let clip_shares = env_positive_f64("PM_BTC_5M_PAIRED_MM_CLIP_SHARES").unwrap_or(DEFAULT_CLIP_SHARES);
        let rebate_on = env_truthy("PM_BTC_5M_PAIRED_MM_REBATE");
        let mid_lo = env_nonneg_f64("PM_BTC_5M_PAIRED_MM_MID_LO").unwrap_or(REGIME_MID_LO);
        let mid_hi = env_nonneg_f64("PM_BTC_5M_PAIRED_MM_MID_HI").unwrap_or(REGIME_MID_HI);
        let range_max = env_nonneg_f64("PM_BTC_5M_PAIRED_MM_RANGE_MAX").unwrap_or(REGIME_RANGE_MAX);
        let spot_vol_max = env_nonneg_f64("PM_BTC_5M_PAIRED_MM_VOL_MAX").unwrap_or(REGIME_SPOT_VOL_MAX);
        let flip_min = env_nonneg_f64("PM_BTC_5M_PAIRED_MM_FLIP_MIN").unwrap_or(REGIME_FLIP_MIN);
        let late_pull_secs = env_nonneg_f64("PM_BTC_5M_PAIRED_MM_LATE_PULL_SECS").unwrap_or(LATE_PULL_SECS);
        let repair_delta =
            env_positive_f64("PM_BTC_5M_PAIRED_MM_RESIDUAL_CAP_SHARES").unwrap_or(REPAIR_DELTA_SHARES);
        let residual_cap_frac =
            env_nonneg_f64("PM_BTC_5M_PAIRED_MM_RESIDUAL_CAP_FRAC").unwrap_or(RESIDUAL_CAP_FRAC);
        info!(
            target: "paired_mm",
            clip_shares,
            rebate_on,
            mid_lo,
            mid_hi,
            range_max,
            spot_vol_max,
            flip_min,
            late_pull_secs,
            repair_delta,
            residual_cap_frac,
            "PAIRED-MM SHADOW overlay enabled (logging only; submits nothing). \
             Quotes only when br2 is NOT quoting the market (regime-disjoint)."
        );
        Some(Self {
            active: None,
            paper_trade_armed,
            clip_shares,
            rebate_on,
            mid_lo,
            mid_hi,
            range_max,
            spot_vol_max,
            flip_min,
            late_pull_secs,
            repair_delta,
            spot: VecDeque::new(),
            disjoint_skip_markets: HashMap::new(),
        })
    }

    /// True when the paper arm is armed (paper_mode AND the flag). The caller
    /// uses this only for startup logging; the gate that actually emits intents
    /// lives in `decide_tick` and is the same boolean.
    pub fn paper_trade_armed(&self) -> bool {
        self.paper_trade_armed
    }

    /// Tap one live Binance aggTrade print into the trailing spot tape. Prunes
    /// to the active-window horizon so the gate's vol/flip measure stays bounded.
    pub fn on_spot_trade(&mut self, price: f64, observed_at_ms: u64) {
        if !price.is_finite() || price <= 0.0 {
            return;
        }
        self.spot.push_back(SpotSample { ts_ms: observed_at_ms, price });
        // Keep ~2x the active window so range/vol over the whole quoting window
        // is always covered.
        let horizon_ms = (ACTIVE_WIN_SECS as u64) * 2 * 1_000;
        while let Some(front) = self.spot.front().copied() {
            if observed_at_ms.saturating_sub(front.ts_ms) <= horizon_ms {
                break;
            }
            self.spot.pop_front();
        }
    }

    /// Drive the BTC-5m lifecycle + one paired-MM decision from a YES book
    /// update. `br2_quoting` is true when the br2 driver produced an order for
    /// this market this tick; when set, the overlay abstains (regime-disjoint).
    ///
    /// `pos` is the REAL runtime paper position for this market (the YES and NO
    /// token share counts). On the paper arm the strict-pairing/residual-cap skew
    /// is computed from these real positions (matched pairs are the redeemable
    /// inventory). On the shadow path `pos` is ignored and the internal
    /// `SimInventory` drives the logged simulation, so the off-path is
    /// byte-identical to INC1.
    ///
    /// Returns the quote decision plus, ONLY when the paper arm is armed, the
    /// maker submit/cancel intents to realize a cancel/replace of the resting
    /// quotes. The submit/cancel lists are ALWAYS empty unless
    /// `paper_trade_armed` (which requires paper_mode), so the caller can never
    /// submit anything outside paper mode.
    pub fn decide_tick(
        &mut self,
        market_id: &MarketId,
        record: &MarketContextRecord,
        yes_book: &BookState,
        br2_quoting: bool,
        pos: PairedPosition,
        now_ms: u64,
    ) -> PairedMmTickResult {
        let (Some(open_ms), Some(close_ms)) = (record.event_start_time_ms, record.event_end_time_ms)
        else {
            return PairedMmTickResult::default();
        };
        let Some(yes_asset_id) = record.instrument_ids.first() else {
            return PairedMmTickResult::default();
        };

        let no_asset_id = record.instrument_ids.get(1).cloned();

        // Cancel ids accumulated this tick (closing/rolling a market pulls any of
        // ITS resting legs). Only ever populated on the paper arm.
        let mut cancel_ids: Vec<ClientOrderId> = Vec::new();

        // Expire OUR active market once past ITS close: cancel any resting legs,
        // then log a final realized summary (matched pairs redeem to $1, residual
        // marked to last mid).
        if let Some(active) = &self.active {
            if now_ms > active.close_ms {
                self.drain_resting_legs(&mut cancel_ids);
                self.log_market_close();
                self.active = None;
            }
        }

        // Ignore ticks for markets not currently live; never re-open a resolved
        // window. (Any close-driven cancels above are still returned.)
        if now_ms > close_ms || now_ms < open_ms {
            return PairedMmTickResult { cancel_ids, ..Default::default() };
        }

        // Roll to a new live BTC-5m window if the runtime moved on.
        let is_new_market = match &self.active {
            Some(active) => &active.market_id != market_id,
            None => true,
        };
        if is_new_market {
            if self.active.is_some() {
                self.drain_resting_legs(&mut cancel_ids);
                self.log_market_close();
            }
            info!(
                target: "paired_mm",
                market = %market_id,
                open_ms,
                close_ms,
                yes_asset = %yes_asset_id,
                no_asset = ?no_asset_id,
                paper_armed = self.paper_trade_armed,
                "PAIRED-MM opened BTC-5m market"
            );
            self.active = Some(ActiveMarket {
                market_id: market_id.clone(),
                yes_asset_id: yes_asset_id.clone(),
                no_asset_id: no_asset_id.clone(),
                close_ms,
                last_decision_ms: 0,
                mid_min: f64::INFINITY,
                mid_max: f64::NEG_INFINITY,
                last_trade_price: yes_book.last_trade_price,
                inventory: SimInventory::default(),
                bid_leg: None,
                no_leg: None,
                quote_seq: 0,
            });
        }

        // Only the YES book drives decisions; throttle to the 1s cadence.
        let should_decide = match &self.active {
            Some(active) => {
                yes_book.asset_id == active.yes_asset_id
                    && now_ms.saturating_sub(active.last_decision_ms) >= DECISION_CADENCE_MS
            }
            None => false,
        };
        if !should_decide {
            return PairedMmTickResult { cancel_ids, ..Default::default() };
        }
        // Need a two-sided YES top-of-book.
        if yes_book.best_bid <= 0.0 || yes_book.best_ask <= 0.0 || yes_book.best_ask < yes_book.best_bid {
            return PairedMmTickResult { cancel_ids, ..Default::default() };
        }

        let yes_mid = 0.5 * (yes_book.best_bid + yes_book.best_ask);

        // Update range-so-far + cadence bookkeeping, snapshot fields we need.
        let (mid_min, mid_max, prev_trade_price) = {
            let active = self.active.as_mut().expect("active set above");
            active.mid_min = active.mid_min.min(yes_mid);
            active.mid_max = active.mid_max.max(yes_mid);
            active.last_decision_ms = now_ms;
            (active.mid_min, active.mid_max, active.last_trade_price)
        };

        let secs_to_close = (close_ms as i64 - now_ms as i64) as f64 / 1000.0;

        // Regime-disjoint: never quote a market br2 is quoting. Pull our resting
        // legs (if any) so we never co-quote.
        if br2_quoting {
            self.drain_resting_legs(&mut cancel_ids);
            let last = self.disjoint_skip_markets.get(market_id.as_str()).copied().unwrap_or(0);
            if now_ms.saturating_sub(last) >= 15_000 {
                info!(
                    target: "paired_mm",
                    market = %market_id,
                    secs_to_close,
                    "PAIRED-MM abstaining: br2 is quoting this market (regime-disjoint)"
                );
                self.disjoint_skip_markets.insert(market_id.as_str().to_string(), now_ms);
            }
            return PairedMmTickResult { cancel_ids, ..Default::default() };
        }

        // First, simulate any fill since the last tick from a fresh taker print
        // crossing our PREVIOUS resting touch quotes. (We posted at the prior
        // best_bid/best_ask; approximate that with the current touch since we
        // requote continuously at the touch each tick.) This uses the same
        // pro-rata clip/(clip+depth_ahead) model as the backtest.
        self.simulate_fill(yes_book, prev_trade_price, yes_mid);
        if let Some(active) = self.active.as_mut() {
            active.last_trade_price = yes_book.last_trade_price;
        }

        // Regime quote-gate (evaluated on data SO FAR, no lookahead).
        let warmup_ok = self.spot_history_secs(now_ms) >= REGIME_WARMUP_SECS;
        let in_window = (ACTIVE_WIN_SECS - secs_to_close.max(0.0)) >= 0.0
            && secs_to_close <= ACTIVE_WIN_SECS;
        let mid_ok = (self.mid_lo..=self.mid_hi).contains(&yes_mid);
        let range = if mid_max.is_finite() && mid_min.is_finite() {
            mid_max - mid_min
        } else {
            0.0
        };
        let range_ok = range <= self.range_max;
        let (vol, flips) = self.spot_metrics(now_ms);
        let vol_ok = vol <= self.spot_vol_max;
        let flip_ok = flips >= self.flip_min;

        let regime_ok = warmup_ok && in_window && mid_ok && range_ok && vol_ok && flip_ok;
        if !regime_ok {
            // Throttled via cadence (already 1s); log the gate snapshot so a
            // reader can tell which gate blocked the quote.
            info!(
                target: "paired_mm",
                market = %market_id,
                secs_to_close,
                yes_mid,
                range,
                vol,
                flips,
                warmup_ok,
                mid_ok,
                range_ok,
                vol_ok,
                flip_ok,
                "PAIRED-MM not quoting: regime gate not met"
            );
            // Not quoting this tick: pull any resting legs.
            self.drain_resting_legs(&mut cancel_ids);
            return PairedMmTickResult { cancel_ids, ..Default::default() };
        }

        // Dynamic gates: which legs are live this tick.
        let mut bid_live = true; // resting YES bid (we buy YES)
        let mut ask_live = true; // resting YES ask (we sell YES == buy NO)

        // Late-window pull: pull both.
        if secs_to_close <= self.late_pull_secs {
            bid_live = false;
            ask_live = false;
        }

        // Strict-pairing repair / residual cap: if one leg outran the other past
        // the band, stop adding to the leading leg and only quote the missing
        // side to re-pair. This is the structural residual cap, NOT a trade-out.
        // On the paper arm the skew is the REAL position (genuine fills); on the
        // shadow path it is the internal SimInventory (byte-identical to INC1).
        let skew = if self.paper_trade_armed {
            pos.skew()
        } else {
            self.active.as_ref().map(|a| a.inventory.skew()).unwrap_or(0.0)
        };
        if skew > self.repair_delta {
            bid_live = false;
        } else if skew < -self.repair_delta {
            ask_live = false;
        }

        let bid_price = bid_live.then_some(yes_book.best_bid);
        // The "ask" leg of the pair is acquired as a NO BUY at 1 - best_ask. We
        // surface the YES ask in the result for logging/parity with INC1.
        let ask_price = ask_live.then_some(yes_book.best_ask);
        let quoting = bid_price.is_some() || ask_price.is_some();

        // Paper arm: realize the maker quote lifecycle (cancel/replace) for both
        // legs against the current touch. Empty on the shadow path.
        let mut submit_intents: Vec<OrderIntent> = Vec::new();
        if self.paper_trade_armed {
            self.manage_paper_legs(
                market_id,
                yes_book,
                bid_price,
                ask_price,
                now_ms,
                &mut submit_intents,
                &mut cancel_ids,
            );
        }

        if let Some(active) = self.active.as_ref() {
            let inv = &active.inventory;
            let sim_pnl = inv.marked_pnl(yes_mid, self.rebate_on);
            info!(
                target: "paired_mm",
                market = %market_id,
                secs_to_close,
                yes_bid = yes_book.best_bid,
                yes_ask = yes_book.best_ask,
                yes_mid,
                clip = self.clip_shares,
                quote_bid = ?bid_price,
                quote_ask = ?ask_price,
                skew,
                paper_armed = self.paper_trade_armed,
                real_yes = pos.yes_shares,
                real_no = pos.no_shares,
                n_submit = submit_intents.len(),
                n_cancel = cancel_ids.len(),
                yes_long = inv.yes_long,
                no_long = inv.no_long,
                matched_pairs = inv.paired(),
                residual_shares = inv.residual_shares(),
                residual_frac = inv.residual_frac(),
                n_fills = inv.n_fills,
                filled_shares = inv.filled_shares,
                sim_pnl,
                "PAIRED-MM quote tick"
            );
        }

        PairedMmTickResult { quoting, bid_price, ask_price, submit_intents, cancel_ids }
    }

    /// Realize the maker quote lifecycle for the paper arm: for each leg, compare
    /// the desired post (or pull) against what is currently resting and emit a
    /// CANCEL + SUBMIT (replace) only when the leg should move, a SUBMIT when
    /// newly live, or a CANCEL when pulled. When a leg's target price is
    /// unchanged we keep the resting quote (emit nothing) so we do not churn the
    /// book every tick.
    ///
    /// YES-bid leg: BUY YES @ best_bid on the YES token. NO leg: BUY NO @
    /// 1 - best_ask on the NO token. Both are post_only BUYs that rest strictly
    /// below the mid, so the matched pair costs best_bid + (1 - best_ask) =
    /// 1 - spread < $1 (the captured spread), mirroring the INC1 identity.
    fn manage_paper_legs(
        &mut self,
        market_id: &MarketId,
        yes_book: &BookState,
        bid_price: Option<f64>,
        ask_price: Option<f64>,
        now_ms: u64,
        submit_intents: &mut Vec<OrderIntent>,
        cancel_ids: &mut Vec<ClientOrderId>,
    ) {
        let clip = self.clip_shares;
        let Some(active) = self.active.as_mut() else { return };
        let yes_token = InstrumentId::from(active.yes_asset_id.as_str());
        let no_token = active.no_asset_id.as_ref().map(|id| InstrumentId::from(id.as_str()));

        // YES-bid leg (BUY YES @ best_bid).
        let want_bid = bid_price.filter(|p| p.is_finite() && *p > 0.0);
        match (want_bid, active.bid_leg.clone()) {
            (Some(price), Some(existing)) if (existing.price - price).abs() <= 1e-9 => {
                // Unchanged: keep the resting quote.
            }
            (Some(price), existing) => {
                if let Some(existing) = existing {
                    cancel_ids.push(existing.client_order_id);
                }
                active.quote_seq += 1;
                let coid = Self::leg_coid(market_id, "bidyes", active.quote_seq, now_ms);
                let intent = Self::maker_buy(
                    coid.clone(),
                    market_id,
                    &yes_token,
                    price,
                    clip,
                    now_ms,
                );
                active.bid_leg = Some(RestingLeg { client_order_id: coid, price });
                submit_intents.push(intent);
            }
            (None, Some(existing)) => {
                cancel_ids.push(existing.client_order_id);
                active.bid_leg = None;
            }
            (None, None) => {}
        }

        // NO leg (BUY NO @ 1 - best_ask). Requires a NO token; if the market
        // exposes none we cannot rest this leg (skip; the bid leg still rests).
        let want_no_price = ask_price
            .filter(|p| p.is_finite() && *p > 0.0)
            .map(|ask| 1.0 - ask)
            .filter(|p| p.is_finite() && *p > 0.0);
        if let Some(no_token) = no_token {
            let _ = yes_book; // book is sourced via ask_price; kept for symmetry
            match (want_no_price, active.no_leg.clone()) {
                (Some(price), Some(existing)) if (existing.price - price).abs() <= 1e-9 => {}
                (Some(price), existing) => {
                    if let Some(existing) = existing {
                        cancel_ids.push(existing.client_order_id);
                    }
                    active.quote_seq += 1;
                    let coid = Self::leg_coid(market_id, "buyno", active.quote_seq, now_ms);
                    let intent = Self::maker_buy(
                        coid.clone(),
                        market_id,
                        &no_token,
                        price,
                        clip,
                        now_ms,
                    );
                    active.no_leg = Some(RestingLeg { client_order_id: coid, price });
                    submit_intents.push(intent);
                }
                (None, Some(existing)) => {
                    cancel_ids.push(existing.client_order_id);
                    active.no_leg = None;
                }
                (None, None) => {}
            }
        } else if let Some(existing) = active.no_leg.take() {
            cancel_ids.push(existing.client_order_id);
        }
    }

    /// Cancel any resting maker legs on the active market (e.g. on close, roll,
    /// abstention, or regime exit) and forget them. No-op when no legs rest or
    /// no market is active. Only emits ids; the caller routes the cancels.
    fn drain_resting_legs(&mut self, cancel_ids: &mut Vec<ClientOrderId>) {
        if let Some(active) = self.active.as_mut() {
            if let Some(leg) = active.bid_leg.take() {
                cancel_ids.push(leg.client_order_id);
            }
            if let Some(leg) = active.no_leg.take() {
                cancel_ids.push(leg.client_order_id);
            }
        }
    }

    /// Build a post_only-style maker BUY intent for one resting leg. The intent
    /// is a limit BUY at `price` for `shares`, tagged so the runtime classifies
    /// it as a paired-MM maker quote. It rests below the touch and never crosses.
    fn maker_buy(
        client_order_id: ClientOrderId,
        market_id: &MarketId,
        instrument: &InstrumentId,
        price: f64,
        shares: f64,
        now_ms: u64,
    ) -> OrderIntent {
        let price = price.clamp(0.0, 1.0);
        let mut intent = OrderIntent::new_buy(
            client_order_id,
            market_id.clone(),
            instrument.clone(),
            price,
            shares,
            format!("paired-mm:{MM_QUOTE_TAG}"),
            now_ms,
        );
        intent.quote_level_tag = Some(MM_QUOTE_TAG.to_string());
        intent
    }

    /// Deterministic-ish unique client_order_id for a resting leg.
    fn leg_coid(market_id: &MarketId, leg: &str, seq: u64, now_ms: u64) -> ClientOrderId {
        ClientOrderId::from(format!(
            "pairedmm-paper:{}:{}:{}:{}",
            market_id.as_str(),
            leg,
            seq,
            now_ms
        ))
    }

    /// Conservative simulated fill from a fresh taker print crossing our resting
    /// touch quotes. A change in `last_trade_price` since the previous tick is
    /// treated as one taker print of one clip's worth of taker size; classified
    /// against our resting touch and filled pro-rata by clip/(clip+depth_ahead).
    ///
    /// RESTING-ONLY (maker, capture-the-spread): a leg can ONLY be booked at its
    /// own maker touch (YES bought at `best_bid`, NO at `1 - best_ask`), and ONLY
    /// when the taker print strictly crosses THAT resting side. We never book a
    /// fill at a touch that moved against us between snapshots, so each leg's cost
    /// is always a genuine maker price strictly below the mid. A matched pair then
    /// costs `best_bid + (1 - best_ask) = 1 - spread < $1` (the captured spread).
    /// Re-pairing a one-sided leg is the same resting model (we keep quoting the
    /// missing side and only fill on a taker cross); if no cross arrives the
    /// residual is held (hard-capped) to resolution, never crossed/lifted.
    fn simulate_fill(&mut self, yes_book: &BookState, prev_trade_price: f64, yes_mid: f64) {
        let print_price = yes_book.last_trade_price;
        // No fresh print, or a non-finite/zero print: nothing to simulate.
        if !print_price.is_finite() || print_price <= 0.0 {
            return;
        }
        if (print_price - prev_trade_price).abs() <= 1e-9 {
            return; // last_trade_price unchanged => no new print this tick
        }
        let bid = yes_book.best_bid;
        let ask = yes_book.best_ask;
        if !(bid > 0.0 && ask > 0.0 && ask > bid) {
            return; // need a clean two-sided touch to define maker prices
        }

        let clip = self.clip_shares;
        let repair_delta = self.repair_delta;
        let Some(active) = self.active.as_mut() else { return };
        let inv = &mut active.inventory;

        // Taker SELL hits our resting YES bid: print at/below best_bid. We buy
        // YES at the MAKER touch (best_bid). depth_ahead = resting bid size.
        // RESTING-ONLY guard: best_bid is strictly below the mid, so this leg can
        // never be booked above the mid; the pair cost stays < $1.
        if print_price <= bid + 1e-9 {
            let ahead = yes_book.best_bid_size.max(0.0);
            let frac = clip / (clip + ahead);
            let mut qty = clip * frac;
            // Don't let this fill push yes ahead of no past the repair band.
            qty = qty.min((inv.no_long + repair_delta - inv.yes_long).max(0.0));
            if qty > 1e-9 {
                inv.yes_long += qty;
                inv.yes_long_cost += qty * bid;
                inv.filled_shares += qty;
                inv.n_fills += 1;
                inv.rebate_usdc += qty * bid * REBATE_FRAC;
            }
            return;
        }

        // Taker BUY lifts our resting YES ask: print at/above best_ask. We sell
        // YES == buy NO at the MAKER touch (1 - best_ask). depth_ahead = resting
        // ask size. RESTING-ONLY guard: 1 - best_ask is strictly below the mid.
        if print_price >= ask - 1e-9 {
            let ahead = yes_book.best_ask_size.max(0.0);
            let frac = clip / (clip + ahead);
            let mut qty = clip * frac;
            qty = qty.min((inv.yes_long + repair_delta - inv.no_long).max(0.0));
            if qty > 1e-9 {
                let no_price = 1.0 - ask;
                inv.no_long += qty;
                inv.no_long_cost += qty * no_price;
                inv.filled_shares += qty;
                inv.n_fills += 1;
                inv.rebate_usdc += qty * no_price * REBATE_FRAC;
            }
        }
        // A print STRICTLY INSIDE the spread (bid < print < ask) crosses neither
        // resting side: it is NOT our fill (it would imply paying the spread to
        // re-pair). We book nothing and hold, matching the resting-only rule.
        let _ = yes_mid; // mid used only by the marked-PnL log, not by the fill model
    }

    /// Log a final realized summary for the active market at close. Matched
    /// pairs redeem to $1; residual is marked to the last observed mid (never
    /// traded out). Pure logging.
    fn log_market_close(&self) {
        let Some(active) = self.active.as_ref() else { return };
        let inv = &active.inventory;
        let last_mid = if active.mid_max.is_finite() && active.mid_min.is_finite() {
            0.5 * (active.mid_min + active.mid_max)
        } else {
            0.5
        };
        info!(
            target: "paired_mm",
            market = %active.market_id,
            yes_long = inv.yes_long,
            no_long = inv.no_long,
            matched_pairs = inv.paired(),
            residual_shares = inv.residual_shares(),
            residual_frac = inv.residual_frac(),
            n_fills = inv.n_fills,
            filled_shares = inv.filled_shares,
            sim_pnl = inv.marked_pnl(last_mid, self.rebate_on),
            "PAIRED-MM SHADOW closed BTC-5m market (final simulated paired PnL)"
        );
    }

    /// Wall-clock span (seconds) covered by the retained spot tape.
    fn spot_history_secs(&self, now_ms: u64) -> f64 {
        match self.spot.front() {
            Some(front) => now_ms.saturating_sub(front.ts_ms) as f64 / 1000.0,
            None => 0.0,
        }
    }

    /// Stdev of 30s-grid spot returns and the sign-flip fraction over the active
    /// window, matching `spot_metrics` in the reference Python. The Python grids
    /// at 5s; here the runtime spot tape is ~1Hz, so we grid at 5s as well to
    /// keep the vol measure discriminating (raw per-tick stdev is too granular).
    fn spot_metrics(&self, now_ms: u64) -> (f64, f64) {
        let lo_ms = now_ms.saturating_sub(ACTIVE_WIN_SECS as u64 * 1_000);
        let seg: Vec<SpotSample> = self
            .spot
            .iter()
            .copied()
            .filter(|s| s.ts_ms >= lo_ms && s.ts_ms <= now_ms && s.price > 0.0)
            .collect();
        if seg.len() < 10 {
            return (0.0, 0.0);
        }
        let start = seg.first().unwrap().ts_ms;
        let end = seg.last().unwrap().ts_ms;
        if end <= start {
            return (0.0, 0.0);
        }
        // Build a 5s grid; sample the last price at or before each grid point.
        let step_ms = 5_000u64;
        let mut grid_prices: Vec<f64> = Vec::new();
        let mut g = start;
        let mut idx = 0usize;
        while g < end {
            while idx + 1 < seg.len() && seg[idx + 1].ts_ms <= g {
                idx += 1;
            }
            grid_prices.push(seg[idx].price);
            g += step_ms;
        }
        if grid_prices.len() < 8 {
            return (0.0, 0.0);
        }
        let mut rets: Vec<f64> = Vec::with_capacity(grid_prices.len() - 1);
        for w in grid_prices.windows(2) {
            if w[0] > 0.0 {
                rets.push((w[1] - w[0]) / w[0]);
            }
        }
        if rets.len() < 2 {
            return (0.0, 0.0);
        }
        let mean = rets.iter().sum::<f64>() / rets.len() as f64;
        let var = rets.iter().map(|r| (r - mean) * (r - mean)).sum::<f64>() / rets.len() as f64;
        let vol = var.sqrt();
        // Sign-flip fraction over nonzero returns.
        let signs: Vec<f64> = rets.iter().map(|r| r.signum()).filter(|s| *s != 0.0).collect();
        let flips = if signs.len() > 1 {
            let mut flip = 0usize;
            for w in signs.windows(2) {
                if w[0] != w[1] {
                    flip += 1;
                }
            }
            flip as f64 / (signs.len() - 1) as f64
        } else {
            0.0
        };
        (vol, flips)
    }
}

fn env_truthy(name: &str) -> bool {
    std::env::var(name)
        .ok()
        .map(|v| matches!(v.trim().to_ascii_lowercase().as_str(), "true" | "1" | "yes"))
        .unwrap_or(false)
}

fn env_positive_f64(name: &str) -> Option<f64> {
    std::env::var(name)
        .ok()
        .and_then(|v| v.trim().parse::<f64>().ok())
        .filter(|v| v.is_finite() && *v > 0.0)
}

/// Parse an env var as a finite, non-negative f64. Returns `None` if unset,
/// unparseable, NaN/inf, or negative. Used for the regime-gate threshold
/// overrides, where 0.0 is a valid disabling sentinel (e.g. mid_lo, flip_min).
fn env_nonneg_f64(name: &str) -> Option<f64> {
    std::env::var(name)
        .ok()
        .and_then(|v| v.trim().parse::<f64>().ok())
        .filter(|v| v.is_finite() && *v >= 0.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn book(bid: f64, bid_sz: f64, ask: f64, ask_sz: f64, last_trade: f64) -> BookState {
        BookState::from_top_of_book("yes", bid, bid_sz, ask, ask_sz, last_trade, 0)
    }

    #[test]
    fn pro_rata_fill_haircut_matches_clip_over_clip_plus_ahead() {
        let mut inv = SimInventory::default();
        // clip 10, 200 ahead => frac = 10/210 => qty = 10 * 10/210 ~= 0.476
        let clip = 10.0;
        let ahead = 200.0;
        let frac = clip / (clip + ahead);
        let qty = clip * frac;
        inv.yes_long += qty;
        assert!((inv.yes_long - 0.47619).abs() < 1e-4);
    }

    #[test]
    fn matched_pairs_redeem_to_one_residual_marked_to_mid() {
        let mut inv = SimInventory::default();
        // 5 YES @ 0.48, 5 NO @ 0.49 => fully paired, pair_cost 0.97 => 5*0.03=0.15
        inv.yes_long = 5.0;
        inv.yes_long_cost = 5.0 * 0.48;
        inv.no_long = 5.0;
        inv.no_long_cost = 5.0 * 0.49;
        let pnl = inv.marked_pnl(0.5, false);
        assert!((pnl - 0.15).abs() < 1e-9, "pnl={pnl}");
        assert_eq!(inv.paired(), 5.0);
        assert_eq!(inv.residual_shares(), 0.0);
    }

    #[test]
    fn residual_frac_reflects_unmatched_skew() {
        let mut inv = SimInventory::default();
        inv.yes_long = 12.0;
        inv.no_long = 10.0;
        assert_eq!(inv.paired(), 10.0);
        assert_eq!(inv.residual_shares(), 2.0);
        // residual / total = 2 / 22
        assert!((inv.residual_frac() - (2.0 / 22.0)).abs() < 1e-9);
    }

    #[test]
    fn simulate_fill_buys_yes_on_taker_sell_hitting_our_bid() {
        let mut mm = PairedMmLiveShadow {
            active: Some(ActiveMarket {
                market_id: MarketId::from("m"),
                yes_asset_id: "yes".to_string(),
                close_ms: 10_000,
                last_decision_ms: 0,
                mid_min: 0.5,
                mid_max: 0.5,
                no_asset_id: None,
                last_trade_price: 0.0,
                inventory: SimInventory::default(),
                bid_leg: None,
                no_leg: None,
                quote_seq: 0,
            }),
            paper_trade_armed: false,
            clip_shares: 10.0,
            rebate_on: false,
            mid_lo: REGIME_MID_LO,
            mid_hi: REGIME_MID_HI,
            range_max: REGIME_RANGE_MAX,
            spot_vol_max: REGIME_SPOT_VOL_MAX,
            flip_min: REGIME_FLIP_MIN,
            late_pull_secs: LATE_PULL_SECS,
            repair_delta: REPAIR_DELTA_SHARES,
            spot: VecDeque::new(),
            disjoint_skip_markets: HashMap::new(),
        };
        // Fresh print at 0.48 == our resting bid; 0 depth ahead => full clip.
        let b = book(0.48, 0.0, 0.50, 100.0, 0.48);
        mm.simulate_fill(&b, 0.0, 0.49);
        let inv = &mm.active.as_ref().unwrap().inventory;
        assert!(inv.yes_long > 0.0);
        assert_eq!(inv.no_long, 0.0);
    }

    #[test]
    fn no_fill_when_last_trade_price_unchanged() {
        let mut mm = PairedMmLiveShadow {
            active: Some(ActiveMarket {
                market_id: MarketId::from("m"),
                yes_asset_id: "yes".to_string(),
                close_ms: 10_000,
                last_decision_ms: 0,
                mid_min: 0.5,
                mid_max: 0.5,
                no_asset_id: None,
                last_trade_price: 0.48,
                inventory: SimInventory::default(),
                bid_leg: None,
                no_leg: None,
                quote_seq: 0,
            }),
            paper_trade_armed: false,
            clip_shares: 10.0,
            rebate_on: false,
            mid_lo: REGIME_MID_LO,
            mid_hi: REGIME_MID_HI,
            range_max: REGIME_RANGE_MAX,
            spot_vol_max: REGIME_SPOT_VOL_MAX,
            flip_min: REGIME_FLIP_MIN,
            late_pull_secs: LATE_PULL_SECS,
            repair_delta: REPAIR_DELTA_SHARES,
            spot: VecDeque::new(),
            disjoint_skip_markets: HashMap::new(),
        };
        let b = book(0.48, 0.0, 0.50, 100.0, 0.48);
        mm.simulate_fill(&b, 0.48, 0.49);
        let inv = &mm.active.as_ref().unwrap().inventory;
        assert_eq!(inv.yes_long, 0.0);
        assert_eq!(inv.no_long, 0.0);
    }

    fn shadow_with_inv(last_trade: f64, inv: SimInventory) -> PairedMmLiveShadow {
        PairedMmLiveShadow {
            active: Some(ActiveMarket {
                market_id: MarketId::from("m"),
                yes_asset_id: "yes".to_string(),
                close_ms: 10_000,
                last_decision_ms: 0,
                mid_min: 0.5,
                mid_max: 0.5,
                no_asset_id: None,
                last_trade_price: last_trade,
                inventory: inv,
                bid_leg: None,
                no_leg: None,
                quote_seq: 0,
            }),
            paper_trade_armed: false,
            clip_shares: 10.0,
            rebate_on: false,
            mid_lo: REGIME_MID_LO,
            mid_hi: REGIME_MID_HI,
            range_max: REGIME_RANGE_MAX,
            spot_vol_max: REGIME_SPOT_VOL_MAX,
            flip_min: REGIME_FLIP_MIN,
            late_pull_secs: LATE_PULL_SECS,
            repair_delta: REPAIR_DELTA_SHARES,
            spot: VecDeque::new(),
            disjoint_skip_markets: HashMap::new(),
        }
    }

    #[test]
    fn resting_pair_costs_below_one_on_clean_oscillation() {
        // Clean 1c market: bid 0.49 / ask 0.50. A taker SELL prints at 0.49
        // (hits our resting bid) then a taker BUY prints at 0.50 (lifts our
        // resting ask). Both legs book at their MAKER touch, so the matched pair
        // costs bid + (1 - ask) = 0.49 + 0.50 = 0.99 < $1 (the captured 1c spread).
        let mut mm = shadow_with_inv(0.0, SimInventory::default());
        // depth 0 ahead => full clip on each side so the legs pair exactly.
        mm.simulate_fill(&book(0.49, 0.0, 0.50, 0.0, 0.49), 0.0, 0.495);
        mm.simulate_fill(&book(0.49, 0.0, 0.50, 0.0, 0.50), 0.49, 0.495);
        let inv = &mm.active.as_ref().unwrap().inventory;
        let paired = inv.paired();
        assert!(paired > 0.0, "expected a matched pair, got {paired}");
        let pair_cost = (inv.yes_long_cost / inv.yes_long) + (inv.no_long_cost / inv.no_long);
        assert!((pair_cost - 0.99).abs() < 1e-9, "pair_cost={pair_cost} (must be 1 - spread)");
        // Realized PnL on the pair is positive (the captured spread).
        assert!(inv.marked_pnl(0.495, false) > 0.0);
    }

    #[test]
    fn mid_spread_print_books_nothing_never_crosses_to_repair() {
        // One-sided (yes ahead of no past the repair band): the ask leg is the
        // re-pairing side. A print STRICTLY INSIDE the spread (0.495, between
        // bid 0.49 and ask 0.50) crosses neither resting side. Re-pairing must
        // NOT lift/cross to fill it: nothing is booked, the residual is held.
        let mut inv = SimInventory::default();
        inv.yes_long = 5.0;
        inv.yes_long_cost = 5.0 * 0.49;
        let mut mm = shadow_with_inv(0.0, inv);
        mm.simulate_fill(&book(0.49, 0.0, 0.50, 0.0, 0.495), 0.0, 0.495);
        let after = &mm.active.as_ref().unwrap().inventory;
        assert_eq!(after.no_long, 0.0, "mid-spread print must not re-pair by crossing");
        assert_eq!(after.yes_long, 5.0);
    }

    #[test]
    fn locked_book_books_nothing() {
        // ask == bid (locked, zero spread): a "pair" here would cost exactly $1
        // with no edge. The resting-only model must reject it (no spread to capture).
        let mut mm = shadow_with_inv(0.0, SimInventory::default());
        mm.simulate_fill(&book(0.50, 0.0, 0.50, 0.0, 0.50), 0.0, 0.50);
        let inv = &mm.active.as_ref().unwrap().inventory;
        assert_eq!(inv.yes_long, 0.0);
        assert_eq!(inv.no_long, 0.0);
    }

    #[test]
    fn from_env_is_none_when_flag_unset() {
        std::env::remove_var("PM_BTC_5M_PAIRED_MM_SHADOW");
        assert!(PairedMmLiveShadow::from_env(true).is_none());
        assert!(PairedMmLiveShadow::from_env(false).is_none());
    }

    #[test]
    fn degenerate_mid_cannot_blow_up_residual_pnl() {
        // Repro of the observed live tick: 0.61-share NO residual, no matched
        // pair, marked against a degenerate yes_mid (~16.1 from a transient bad
        // spread). The old math produced sim_pnl=-9.54 (|pnl| >> shares). The
        // mark must now stay bounded by the residual share count.
        let mut inv = SimInventory::default();
        inv.no_long = 0.61;
        inv.no_long_cost = 0.61 * 0.49;
        let pnl = inv.marked_pnl(16.147_093_442_622_953, false);
        assert!(
            pnl.abs() <= inv.residual_shares() + 1e-9,
            "degenerate mid blew up residual PnL: pnl={pnl}, shares={}",
            inv.residual_shares()
        );
    }

    #[test]
    fn residual_pnl_bounded_by_shares_across_pathological_inputs() {
        // |residual PnL| can never exceed the residual share count, for any
        // mid (incl. NaN / out-of-domain) and any (even garbage) cost basis.
        let mids = [
            f64::NAN,
            f64::INFINITY,
            f64::NEG_INFINITY,
            -50.0,
            0.0,
            0.5,
            1.0,
            999.0,
        ];
        let costs = [-10.0, 0.0, 0.25, 0.5, 1.0, 50.0];
        for side_is_yes in [true, false] {
            for n in [0.61_f64, 1.0, 7.5, 100.0] {
                for &cost_per in &costs {
                    for &mid in &mids {
                        let mut inv = SimInventory::default();
                        if side_is_yes {
                            inv.yes_long = n;
                            inv.yes_long_cost = n * cost_per;
                        } else {
                            inv.no_long = n;
                            inv.no_long_cost = n * cost_per;
                        }
                        let pnl = inv.marked_pnl(mid, false);
                        assert!(
                            pnl.is_finite() && pnl.abs() <= n + 1e-9,
                            "residual PnL exceeded shares: pnl={pnl}, n={n}, \
                             cost_per={cost_per}, mid={mid}, yes={side_is_yes}"
                        );
                    }
                }
            }
        }
    }

    fn paper_shadow_with_market(no_asset: Option<&str>) -> PairedMmLiveShadow {
        let mut mm = shadow_with_inv(0.0, SimInventory::default());
        mm.paper_trade_armed = true;
        if let Some(active) = mm.active.as_mut() {
            active.no_asset_id = no_asset.map(|s| s.to_string());
        }
        mm
    }

    #[test]
    fn from_env_paper_arm_refused_outside_paper_mode() {
        // The hard guard: PM_BTC_5M_PAIRED_MM_PAPER_TRADE set but paper_mode=false
        // MUST NOT arm. With the shadow flag on, the overlay still constructs but
        // stays shadow-only (no paper arm). Set the SHADOW flag immediately before
        // each from_env call so a sibling env-mutating test cannot race it away.
        std::env::set_var("PM_BTC_5M_PAIRED_MM_PAPER_TRADE", "true");
        std::env::set_var("PM_BTC_5M_PAIRED_MM_SHADOW", "true");
        let refused = PairedMmLiveShadow::from_env(false);
        if let Some(refused) = refused {
            assert!(!refused.paper_trade_armed(), "paper arm must be refused when !paper_mode");
        }
        std::env::set_var("PM_BTC_5M_PAIRED_MM_SHADOW", "true");
        let armed = PairedMmLiveShadow::from_env(true);
        if let Some(armed) = armed {
            assert!(armed.paper_trade_armed(), "paper arm must arm under paper_mode");
        }
        std::env::remove_var("PM_BTC_5M_PAIRED_MM_PAPER_TRADE");
        std::env::remove_var("PM_BTC_5M_PAIRED_MM_SHADOW");
    }

    #[test]
    fn manage_paper_legs_submits_both_maker_buys_first_tick() {
        // bid 0.49 / ask 0.50, NO token present => rest BUY YES @0.49 and
        // BUY NO @ 1-0.50 = 0.50. Two post_only maker buys.
        let mut mm = paper_shadow_with_market(Some("no"));
        let b = book(0.49, 5.0, 0.50, 5.0, 0.49);
        let mut submits = Vec::new();
        let mut cancels = Vec::new();
        mm.manage_paper_legs(
            &MarketId::from("m"),
            &b,
            Some(0.49),
            Some(0.50),
            1_000,
            &mut submits,
            &mut cancels,
        );
        assert_eq!(submits.len(), 2, "both legs should post on first tick");
        assert!(cancels.is_empty(), "nothing resting yet");
        let yes_leg = submits.iter().find(|i| i.instrument_id.as_str() == "yes").unwrap();
        let no_leg = submits.iter().find(|i| i.instrument_id.as_str() == "no").unwrap();
        assert_eq!(yes_leg.side, crate::types::TradeSide::Buy);
        assert!((yes_leg.limit_price - 0.49).abs() < 1e-9);
        assert_eq!(no_leg.side, crate::types::TradeSide::Buy);
        assert!((no_leg.limit_price - 0.50).abs() < 1e-9, "NO @ 1-ask");
        assert_eq!(yes_leg.quote_level_tag.as_deref(), Some(MM_QUOTE_TAG));
    }

    #[test]
    fn manage_paper_legs_keeps_unchanged_touch_and_replaces_on_move() {
        let mut mm = paper_shadow_with_market(Some("no"));
        let market = MarketId::from("m");
        let b = book(0.49, 5.0, 0.50, 5.0, 0.49);
        let mut submits = Vec::new();
        let mut cancels = Vec::new();
        mm.manage_paper_legs(&market, &b, Some(0.49), Some(0.50), 1_000, &mut submits, &mut cancels);
        assert_eq!(submits.len(), 2);

        // Same touch next tick: keep, no churn.
        submits.clear();
        cancels.clear();
        mm.manage_paper_legs(&market, &b, Some(0.49), Some(0.50), 2_000, &mut submits, &mut cancels);
        assert!(submits.is_empty() && cancels.is_empty(), "unchanged touch must not churn");

        // Touch moves on the bid leg: cancel old + submit new for that leg only.
        submits.clear();
        cancels.clear();
        mm.manage_paper_legs(&market, &b, Some(0.48), Some(0.50), 3_000, &mut submits, &mut cancels);
        assert_eq!(submits.len(), 1, "only the moved leg replaces");
        assert_eq!(cancels.len(), 1, "old bid leg cancelled");
        assert!((submits[0].limit_price - 0.48).abs() < 1e-9);
    }

    #[test]
    fn manage_paper_legs_pulls_leg_when_target_none() {
        let mut mm = paper_shadow_with_market(Some("no"));
        let market = MarketId::from("m");
        let b = book(0.49, 5.0, 0.50, 5.0, 0.49);
        let mut submits = Vec::new();
        let mut cancels = Vec::new();
        mm.manage_paper_legs(&market, &b, Some(0.49), Some(0.50), 1_000, &mut submits, &mut cancels);
        assert_eq!(submits.len(), 2);

        // Bid leg pulled (e.g. late-pull or repair skew) => cancel it, keep NO.
        submits.clear();
        cancels.clear();
        mm.manage_paper_legs(&market, &b, None, Some(0.50), 2_000, &mut submits, &mut cancels);
        assert!(submits.is_empty(), "no new posts when pulling");
        assert_eq!(cancels.len(), 1, "pulled bid leg cancelled");
        assert!(mm.active.as_ref().unwrap().bid_leg.is_none());
        assert!(mm.active.as_ref().unwrap().no_leg.is_some());
    }

    #[test]
    fn drain_resting_legs_cancels_all_open_legs() {
        let mut mm = paper_shadow_with_market(Some("no"));
        let market = MarketId::from("m");
        let b = book(0.49, 5.0, 0.50, 5.0, 0.49);
        let mut submits = Vec::new();
        let mut cancels = Vec::new();
        mm.manage_paper_legs(&market, &b, Some(0.49), Some(0.50), 1_000, &mut submits, &mut cancels);
        cancels.clear();
        mm.drain_resting_legs(&mut cancels);
        assert_eq!(cancels.len(), 2, "both resting legs pulled on drain");
        assert!(mm.active.as_ref().unwrap().bid_leg.is_none());
        assert!(mm.active.as_ref().unwrap().no_leg.is_none());
    }

    #[test]
    fn manage_paper_legs_skips_no_leg_without_no_token() {
        // No NO token: only the YES bid leg can rest.
        let mut mm = paper_shadow_with_market(None);
        let b = book(0.49, 5.0, 0.50, 5.0, 0.49);
        let mut submits = Vec::new();
        let mut cancels = Vec::new();
        mm.manage_paper_legs(
            &MarketId::from("m"),
            &b,
            Some(0.49),
            Some(0.50),
            1_000,
            &mut submits,
            &mut cancels,
        );
        assert_eq!(submits.len(), 1, "only YES leg without a NO token");
        assert_eq!(submits[0].instrument_id.as_str(), "yes");
    }
}
