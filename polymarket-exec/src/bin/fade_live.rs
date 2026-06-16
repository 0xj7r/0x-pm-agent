//! **DEPRECATED — DO NOT USE FOR LIVE TRADING.**
//!
//! This binary reimplements the fade input pipeline (`FadeCore`) separately from
//! the validated `pm-shadow` engine. It diverged from `shadow-final` and lost
//! ~$700 overnight (2026-06-15 → 2026-06-16) while the shared engine made money
//! on identical config/feeds. See `docs/postmortem-2026-06-16-fade-live-divergence.md`.
//!
//! Use `shadow_live` instead — it runs the EXACT `pm-shadow` engine with
//! `frozen_shadow_final_args`. Live execution will attach to
//! `run_shadow_with_sink` after trade-by-trade decision parity is proven (P4).
//!
//! This binary is HALTED by default. Set `PM_FADE_ALLOW_DEPRECATED=1` only for
//! local debugging of the execution adapter — never for real money.
//!
//! ---
//!
//! Lean LIVE runtime for the exogenous-fade strategy (BTC-5m).
//!
//! Runs ONLY the fade: no br2, no market-making. Every WHETHER/HOW-to-enter
//! decision flows through the shared SSOT `pm_alpha::decide_entry`, so this
//! binary makes byte-identical decisions to the backtest and the pm-app
//! shadow. No gate is reimplemented here.
//!
//! SAFE BY DEFAULT. With no env set, the binary logs a WouldEnter line for
//! every qualifying tick and submits NOTHING. Real-money submission is the
//! DORMANT arm: it takes effect only when EVERY precondition in
//! [`LiveArm::from_env`] holds (live-trade flag truthy, NOT paper mode, a
//! kill-switch path configured, and both notional caps > 0). This mirrors
//! `runtime::br2_live::Br2LiveShadow::from_env` exactly. Per tick, if the
//! kill-switch file exists, the binary submits nothing and disarms.
//!
//! THREE arms, exactly one active: shadow-only (default, log only), PAPER
//! (`PM_FADE_PAPER_TRADE=1` with paper mode), and LIVE (full real-money arm).
//! Paper exercises the FULL order machinery (intent build, arming gate, caps,
//! kill-switch) but routes each fill to a LOCAL simulator: no signer, no
//! adapter, no network, no money. Paper lots settle locally against the spot
//! tape (spot-at-close vs the spot-at-open strike the belief uses). Paper and
//! live are MUTUALLY EXCLUSIVE (paper needs paper mode, live needs !paper mode).
//!
//! Feeds (all read-only consumers of public endpoints; the only outbound
//! payloads are websocket subscriptions and pings) run as tokio tasks and
//! push into a single shared [`FadeCore`] behind a mutex:
//!   - Binance BTCUSDT spot trade tape -> belief vol + strike proxy.
//!   - Binance BTCUSDT perp aggTrade tape -> price-level blend + basis.
//!   - Gamma discovery -> the active btc-updown-5m window (open/close/tokens).
//!   - Polymarket market websocket -> the YES/NO order books (best asks).
//!
//! The decision loop fires at ~1s cadence, builds the EXACT same inputs the
//! shadow builds (SpotHistory from the trade VecDeque, PerpState{trades,
//! oi:[], funding:[]}, ExoState, model.evaluate), then calls `decide_entry`.
//! On `EntryAction::Enter`, when armed and the kill-switch is clear, it
//! submits a marketable taker BUY of `side` token at the decision's
//! `marketable_limit_price`, sized `target_notional / limit`, tagged
//! `exo-fade-taker`. hold_to_redemption => never sell (the SSOT's frozen
//! config has exit_after_s = 0, so this is always a hold lane).

use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result};
use tokio::sync::watch;
use tracing::{info, warn};

use pm_alpha::harness::Side;
use pm_alpha::{
    decide_entry, frozen_fade_decide_config, AlphaModel, AlphaModelConfig, DecideConfig,
    DecisionInputs, EntryAction, EntryState, ExoState, MarketMeta, PerpState, Token, VolEstimator,
};
use pm_types::{SpotHistory, SpotTick};

use polymarket_exec::types::{ClientOrderId, InstrumentId, MarketId, TradeSide};
use polymarket_exec::wire::execution_adapter::{
    ClobProtocolVersion, ExecutionAdapter, PolymarketConfig, PolymarketCredentials,
    PolymarketExecutionAdapter, PolymarketL1Credentials, PolymarketSignatureType,
    RedeemPositionsRequest, SubmitOrderRequest, TimeInForce,
};

/// Spot ticks retained in the rolling buffer. vol3600 needs >= 1h; keep
/// generous headroom so the warm-up gate clears after a restart.
const SPOT_KEEP_SECS: i64 = 7_800;
/// The frozen model vol lookback (seconds). The warm-up gate stands the
/// strategy down until the spot tape spans this.
const VOL_LOOKBACK_S: u32 = 3_600;
/// Frozen live clip notional default (USDC). Override via PM_FADE_CLIP_USD.
const DEFAULT_CLIP_USD: f64 = 15.0;
/// Max entries per market (re-arm ladder). The SSOT's rearm/cooldown gate
/// governs WHEN; this caps the count, mirroring shadow's max_clips.
const MAX_CLIPS: u32 = 2;
/// Decision cadence (ms). The SSOT was validated on a 1s sampling cadence.
const DECIDE_CADENCE_MS: i64 = 1_000;
/// btc-updown-5m slug prefix for Gamma discovery.
const SLUG_PREFIX: &str = "btc-updown-5m-";
/// Safe margin (seconds) after a window's close before we attempt redemption.
/// Lets on-chain resolution settle; the relayer/CTF rejects redeeming an
/// unresolved market anyway, so this is conservative, not load-bearing.
const REDEEM_MARGIN_S: i64 = 60;

fn now_unix_ns() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as i64)
        .unwrap_or(0)
}

fn now_unix_ms() -> i64 {
    now_unix_ns() / 1_000_000
}

fn decide_cfg(notional_usdc: f64) -> DecideConfig {
    frozen_fade_decide_config(notional_usdc)
}

// Frozen model config (vol3600 realized, perp blend 0.75, momentum off).
fn model() -> AlphaModel {
    AlphaModel {
        cfg: AlphaModelConfig {
            vol_lookback_s: VOL_LOOKBACK_S,
            vol_sample_dt_s: 1,
            vol_estimator: VolEstimator::Realized,
            perp_price_weight: 0.75,
            momentum_lookback_s: 0,
            momentum_weight: 1.0,
            xasset_weight: 0.0,
            ..AlphaModelConfig::default()
        },
        calibrator: None,
        dir_model: None,
    }
}

// Order-book ladder (mirrors pm_app::shadow::Ladder; integer price keys so
// f64 never acts as a map key). We only need best_ask + mid for the fade.
#[derive(Debug, Default, Clone)]
struct Ladder {
    bids: BTreeMap<i64, f64>,
    asks: BTreeMap<i64, f64>,
}

fn price_key(price: f64) -> i64 {
    (price * 100_000.0).round() as i64
}
fn key_price(key: i64) -> f64 {
    key as f64 / 100_000.0
}

impl Ladder {
    fn best_ask(&self) -> Option<f64> {
        self.asks.iter().next().map(|(k, _)| key_price(*k))
    }
    fn best_bid(&self) -> Option<f64> {
        self.bids.iter().next_back().map(|(k, _)| key_price(*k))
    }
    fn mid(&self) -> Option<f64> {
        Some((self.best_bid()? + self.best_ask()?) / 2.0)
    }

    /// Volume-weighted average price to fill `notional` USDC by walking the
    /// asks up to `cap_price` at full depth. Mirrors the backtest fill()
    /// (depth_capture_frac = 1.0): the marketable order sweeps level by level
    /// to the limit, so sizing shares off this VWAP (not the touch) makes the
    /// deployed notional land near the clip instead of overrunning. Returns the
    /// VWAP of whatever fills within the cap (partial if the book is thin), or
    /// None if no asks are at or below the cap.
    fn vwap_for_notional(&self, notional: f64, cap_price: f64) -> Option<f64> {
        if !(notional > 0.0) {
            return None;
        }
        let mut remaining = notional;
        let mut shares = 0.0;
        for (k, &size) in self.asks.iter() {
            let price = key_price(*k);
            if price > cap_price + 1e-9 {
                break;
            }
            if size <= 0.0 {
                continue;
            }
            let take_notional = remaining.min(price * size);
            shares += take_notional / price;
            remaining -= take_notional;
            if remaining <= 1e-9 {
                break;
            }
        }
        let filled = notional - remaining;
        (shares > 0.0 && filled > 0.0).then_some(filled / shares)
    }
}

/// One discovered btc-updown-5m window + its per-market entry state. The
/// `EntryState` (armed + cooldown clock) is owned here and threaded into the
/// SSOT; `n_clips` enforces the per-market clip cap (the SSOT's rearm/cooldown
/// decides WHEN, this caps the count).
#[derive(Debug, Clone)]
struct MarketWindow {
    slug: String,
    open_ts_s: i64,
    close_ts_s: i64,
    up_token: String,
    down_token: String,
    /// CTF binary index set for each outcome token: 1 << (position in
    /// clobTokenIds). For these 2-token markets one side is 1, the other 2.
    /// Redeeming the wrong set claims nothing, so we track per-side.
    up_index_set: u64,
    down_index_set: u64,
    /// CTF condition id (for redemption); None until Gamma supplies it.
    condition_id: Option<String>,
    entry: EntryState,
    n_clips: u32,
    /// Held lots awaiting settlement (price, shares, side). Hold-to-redemption
    /// never sells; real lots settle at on-chain resolution (`redeem_sweep`),
    /// paper lots settle LOCALLY against the spot tape (`paper_settle_sweep`).
    held: Vec<HeldLot>,
}

#[derive(Debug, Clone, Copy)]
struct HeldLot {
    side: Side,
    avg_price: f64,
    shares: f64,
    /// Paper lots settle LOCALLY against the spot tape (no on-chain redeem) and
    /// never appear in `redeem_candidates`; real lots ride to on-chain redemption.
    is_paper: bool,
}

/// A real on-chain position awaiting post-close redemption, persisted to the
/// redeem ledger so a restart never orphans an open lot (held lots are otherwise
/// in-memory only). `index_sets` are the CTF binary index sets actually held for
/// this market (Side::Yes -> up_index_set, Side::No -> down_index_set), deduped:
/// same-side clips -> 1 set, a clip-2 side-flip -> both.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
struct PendingRedeem {
    condition_id: String,
    slug: String,
    close_ns: i64,
    index_sets: Vec<u64>,
}

/// All decision/bookkeeping state. Pure w.r.t. I/O: feeds push events in, the
/// runner polls `decide` out at 1s cadence.
struct FadeCore {
    model: AlphaModel,
    cfg: DecideConfig,
    spot: VecDeque<SpotTick>,
    perp_buf: VecDeque<SpotTick>,
    books: HashMap<String, Ladder>,
    markets: HashMap<String, MarketWindow>,
    /// CTF condition ids already redeemed. Idempotency guard: a market is never
    /// redeemed twice even if its window lingers across several sweeps.
    redeemed: HashSet<String>,
    /// Real positions awaiting on-chain redemption, persisted to the redeem
    /// ledger so a restart resumes (not re-derives) open redemptions. The REAL
    /// redemption source of truth; paper lots are NOT tracked here.
    pending_redeems: Vec<PendingRedeem>,
    /// Path to the redeem ledger JSON (PM_FADE_REDEEM_LEDGER_PATH, default
    /// "fade_redeem_ledger.json"). `pending_redeems` is mirrored here on change.
    redeem_ledger_path: PathBuf,
    /// Running fee-net paper P&L (USDC) across all locally-settled paper lots.
    /// Live lots never touch this; it is bookkeeping for the paper arm only.
    paper_pnl_total: f64,
    /// Count of paper lots settled (denominator for the running paper P&L log).
    paper_lots_settled: u64,
}

impl FadeCore {
    fn new(cfg: DecideConfig) -> Self {
        let redeem_ledger_path = PathBuf::from(
            std::env::var("PM_FADE_REDEEM_LEDGER_PATH")
                .unwrap_or_else(|_| "fade_redeem_ledger.json".to_string()),
        );
        // Resume any redemptions left pending by a prior run so a restart never
        // orphans an open position. Missing/corrupt ledger -> empty (safe).
        let pending_redeems = Self::load_pending(&redeem_ledger_path);
        Self {
            model: model(),
            cfg,
            spot: VecDeque::new(),
            perp_buf: VecDeque::new(),
            books: HashMap::new(),
            markets: HashMap::new(),
            redeemed: HashSet::new(),
            pending_redeems,
            redeem_ledger_path,
            paper_pnl_total: 0.0,
            paper_lots_settled: 0,
        }
    }

    /// Read+parse the redeem ledger. Best-effort: a missing file or parse error
    /// yields an empty Vec (a fresh run, or a hand-edited/corrupt ledger we will
    /// not crash on). Real positions still redeem via `redeemed` idempotency.
    fn load_pending(path: &std::path::Path) -> Vec<PendingRedeem> {
        match std::fs::read_to_string(path) {
            Ok(raw) => serde_json::from_str(&raw).unwrap_or_else(|error| {
                warn!(
                    target: "fade_live",
                    path = %path.display(),
                    error = %error,
                    "redeem ledger parse failed; starting with empty pending set"
                );
                Vec::new()
            }),
            Err(_) => Vec::new(),
        }
    }

    /// Mirror `pending_redeems` to the ledger. Best-effort: an IO error is
    /// logged (never panics) so a transient disk fault cannot crash the live
    /// agent; the in-memory set still drives redemption this run.
    fn persist_pending(&self) {
        match serde_json::to_string(&self.pending_redeems) {
            Ok(json) => {
                if let Err(error) = std::fs::write(&self.redeem_ledger_path, json) {
                    warn!(
                        target: "fade_live",
                        path = %self.redeem_ledger_path.display(),
                        error = %error,
                        "redeem ledger write failed; pending set held in memory only"
                    );
                }
            }
            Err(error) => warn!(
                target: "fade_live",
                error = %error,
                "redeem ledger serialize failed; skipping persist"
            ),
        }
    }

    fn push_spot(&mut self, exchange_ms: i64, price: f64, quantity: f64, is_buyer_maker: bool) {
        if !(price.is_finite() && price > 0.0) {
            return;
        }
        self.spot.push_back(SpotTick {
            ts_ns: exchange_ms * 1_000_000,
            price,
            quantity: quantity as f32,
            is_buyer_maker,
        });
        let cutoff_ns = (exchange_ms - SPOT_KEEP_SECS * 1_000) * 1_000_000;
        while self.spot.front().is_some_and(|t| t.ts_ns < cutoff_ns) {
            self.spot.pop_front();
        }
    }

    fn push_perp(&mut self, exchange_ms: i64, price: f64, quantity: f64) {
        if !(price.is_finite() && price > 0.0) {
            return;
        }
        self.perp_buf.push_back(SpotTick {
            ts_ns: exchange_ms * 1_000_000,
            price,
            quantity: quantity as f32,
            is_buyer_maker: false,
        });
        let cutoff_ns = (exchange_ms - SPOT_KEEP_SECS * 1_000) * 1_000_000;
        while self.perp_buf.front().is_some_and(|t| t.ts_ns < cutoff_ns) {
            self.perp_buf.pop_front();
        }
    }

    fn apply_book_snapshot(&mut self, token: &str, bids: &[(f64, f64)], asks: &[(f64, f64)]) {
        let ladder = self.books.entry(token.to_string()).or_default();
        ladder.bids = bids
            .iter()
            .filter(|(p, s)| *p > 0.0 && *p < 1.0 && *s > 0.0)
            .map(|(p, s)| (price_key(*p), *s))
            .collect();
        ladder.asks = asks
            .iter()
            .filter(|(p, s)| *p > 0.0 && *p < 1.0 && *s > 0.0)
            .map(|(p, s)| (price_key(*p), *s))
            .collect();
    }

    fn apply_price_change(&mut self, token: &str, is_buy_side: bool, price: f64, size: f64) {
        if !(price > 0.0 && price < 1.0 && size.is_finite() && size >= 0.0) {
            return;
        }
        let ladder = self.books.entry(token.to_string()).or_default();
        let side = if is_buy_side { &mut ladder.bids } else { &mut ladder.asks };
        if size <= 0.0 {
            side.remove(&price_key(price));
        } else {
            side.insert(price_key(price), size);
        }
    }

    fn upsert_market(&mut self, market: MarketWindow) {
        match self.markets.get_mut(&market.slug) {
            Some(existing) => {
                if existing.condition_id.is_none() {
                    existing.condition_id = market.condition_id;
                }
            }
            None => {
                self.markets.insert(market.slug.clone(), market);
            }
        }
    }

    /// Drop windows that closed more than 10 minutes ago, plus their books.
    fn prune(&mut self, now_ns: i64) {
        let cutoff_s = now_ns / 1_000_000_000 - 600;
        let dead: Vec<String> = self
            .markets
            .values()
            .filter(|m| m.close_ts_s < cutoff_s)
            .map(|m| m.slug.clone())
            .collect();
        for slug in dead {
            if let Some(m) = self.markets.remove(&slug) {
                self.books.remove(&m.up_token);
                self.books.remove(&m.down_token);
            }
        }
    }

    fn subscribed_tokens(&self) -> Vec<String> {
        let mut tokens: Vec<String> = self
            .markets
            .values()
            .flat_map(|m| [m.up_token.clone(), m.down_token.clone()])
            .collect();
        tokens.sort();
        tokens.dedup();
        tokens
    }

    fn spot_history(&self) -> SpotHistory {
        SpotHistory::new(self.spot.iter().copied().collect())
    }

    fn perp_state(&self) -> Option<PerpState> {
        if self.cfg.notional_usdc == 0.0 || self.perp_buf.is_empty() {
            return None;
        }
        Some(PerpState {
            trades: SpotHistory::new(self.perp_buf.iter().copied().collect()),
            oi: Vec::new(),
            funding: Vec::new(),
        })
    }

    /// True once BOTH the spot and perp buffers span the model vol lookback.
    /// Below this the belief runs on a truncated window and produces off-model
    /// output the full-history backtest would never hold. Perp is gated too (a
    /// divergence from shadow, which never restarts so its perp tape is always
    /// warm): the live config blends perp price at 0.75, so trading before the
    /// perp tape spans the lookback runs a spot-only belief, not the validated
    /// one. Perp is pre-warmed from klines on startup so this normally clears
    /// immediately; if that fetch fails we stand down until perp builds live.
    fn warmed_up(&self) -> bool {
        let spans = |buf: &VecDeque<SpotTick>| {
            matches!(
                (buf.front(), buf.back()),
                (Some(first), Some(last))
                    if last.ts_ns - first.ts_ns >= VOL_LOOKBACK_S as i64 * 1_000_000_000
            )
        };
        spans(&self.spot) && spans(&self.perp_buf)
    }

    /// One decision pass over all active windows. Returns the `Enter`
    /// decisions (the caller submits + records). Mirrors the shadow's `decide`
    /// but defers every gate to `decide_entry`.
    fn decide(&mut self, now_ns: i64) -> Vec<EnterIntent> {
        if !self.warmed_up() {
            return Vec::new();
        }
        let spot = self.spot_history();
        let perp = self.perp_state();
        let cfg = self.cfg;
        let mut out = Vec::new();

        for m in self.markets.values_mut() {
            let open_ns = m.open_ts_s * 1_000_000_000;
            let close_ns = m.close_ts_s * 1_000_000_000;
            if now_ns < open_ns || now_ns >= close_ns {
                continue;
            }
            // Clip exhaustion: skip the decide entirely (shadow semantics).
            if m.n_clips >= MAX_CLIPS {
                continue;
            }
            // Strike = binance spot at-or-before open (same price basis as the
            // belief state). NO gamma fallback: if the tape can't cover the
            // open, stand down rather than mix a USD-index strike into a
            // USDT-basis belief (the phantom-edge bug shadow.rs documents).
            let Some(strike) = spot.price_at_or_before(open_ns) else {
                continue;
            };
            // Need both tokens' best ask; skip one-sided / locked books.
            let (Some(up_ask), Some(down_ask)) = (
                self.books.get(&m.up_token).and_then(Ladder::best_ask),
                self.books.get(&m.down_token).and_then(Ladder::best_ask),
            ) else {
                continue;
            };
            let up_mid = self.books.get(&m.up_token).and_then(Ladder::mid).unwrap_or(up_ask);

            let Some(token) = Token::from_slug(&m.slug) else {
                continue;
            };
            let state = ExoState {
                spot: &spot,
                perp: perp.as_ref(),
                ref_spot: None,
                market: MarketMeta {
                    token,
                    window_secs: (m.close_ts_s - m.open_ts_s).max(1) as u32,
                    open_ts_ns: open_ns,
                    close_ts_ns: close_ns,
                    strike,
                },
                now_ns,
            };
            let Some(ev) = self.model.evaluate(&state, false) else {
                continue;
            };

            // Basis momentum (bps): basis_frac(now) - basis_frac(now - 60s),
            // scaled to bps. None when the perp tape can't cover either point.
            let basis_mom_60s_bps = match perp.as_ref() {
                Some(p) => {
                    let now_b = p.basis_frac(&spot, now_ns);
                    let prev_b = p.basis_frac(&spot, now_ns - 60 * 1_000_000_000);
                    match (now_b, prev_b) {
                        (Some(a), Some(b)) => (a - b) * 1e4,
                        _ => 0.0,
                    }
                }
                None => 0.0,
            };

            let inputs = DecisionInputs {
                p_exo: ev.p,
                dir_p_up: None,
                dir_model_active: false,
                yes_ask: up_ask,
                no_buy: down_ask,
                mid: up_mid,
                sigma_bar_bps: ev.raw.sigma_bar_bps,
                basis_mom_60s_bps,
            };

            let (decision, delta) = decide_entry(&inputs, now_ns, close_ns, &m.entry, &cfg);

            // Apply the Rearm-path arming immediately (the SSOT contract: the
            // caller applies `set_armed` on Rearm; the rest after a completed
            // entry). On Enter we defer the cooldown/arming/clip mutation until
            // AFTER submit, exactly like the shadow.
            if let Some(armed) = delta.set_armed {
                if decision.action != EntryAction::Enter {
                    m.entry.armed = armed;
                }
            }

            if decision.action == EntryAction::Enter {
                let (token_id, side_str) = match decision.side {
                    Side::Yes => (m.up_token.clone(), "up"),
                    Side::No => (m.down_token.clone(), "down"),
                };
                out.push(EnterIntent {
                    slug: m.slug.clone(),
                    side: decision.side,
                    side_str,
                    token_id,
                    limit_price: decision.marketable_limit_price,
                    fill_ask: match decision.side {
                        Side::Yes => up_ask,
                        Side::No => down_ask,
                    },
                    target_notional: decision.target_notional,
                    hold_to_redemption: decision.hold_to_redemption,
                    p_exo: ev.p,
                    edge: match decision.side {
                        Side::Yes => ev.p - up_ask,
                        Side::No => (1.0 - ev.p) - down_ask,
                    },
                    sigma_bar_bps: ev.raw.sigma_bar_bps,
                    strike,
                    now_ns,
                    delta_set_armed: delta.set_armed,
                    delta_next_entry_ns: delta.set_next_entry_ns,
                    delta_inc_clips: delta.inc_clips,
                });
            }
        }
        out
    }

    /// Commit a completed entry's state delta + record the held lot. Called
    /// AFTER a (real or shadow) submit, so the SSOT state stays consistent
    /// regardless of arming (the shadow always advances per-market state).
    fn commit_entry(
        &mut self,
        slug: &str,
        side: Side,
        avg_price: f64,
        shares: f64,
        is_paper: bool,
        set_armed: Option<bool>,
        next_entry_ns: Option<i64>,
        inc_clips: bool,
    ) {
        let Some(m) = self.markets.get_mut(slug) else {
            return;
        };
        if let Some(a) = set_armed {
            m.entry.armed = a;
        }
        if let Some(ns) = next_entry_ns {
            m.entry.next_entry_ns = ns;
        }
        if inc_clips {
            m.n_clips = m.n_clips.saturating_add(1);
        }
        if shares > 0.0 {
            m.held.push(HeldLot { side, avg_price, shares, is_paper });
        }
        // Persist REAL positions to the redeem ledger so a restart resumes the
        // open redemption (paper lots settle locally and are never tracked here).
        if shares > 0.0 && !is_paper {
            let index_set = match side {
                Side::Yes => m.up_index_set,
                Side::No => m.down_index_set,
            };
            if let Some(condition_id) = m.condition_id.clone() {
                let close_ns = m.close_ts_s * 1_000_000_000;
                let slug = m.slug.clone();
                if let Some(pr) = self
                    .pending_redeems
                    .iter_mut()
                    .find(|pr| pr.condition_id == condition_id)
                {
                    if !pr.index_sets.contains(&index_set) {
                        pr.index_sets.push(index_set);
                    }
                } else {
                    self.pending_redeems.push(PendingRedeem {
                        condition_id,
                        slug,
                        close_ns,
                        index_sets: vec![index_set],
                    });
                }
                self.persist_pending();
            } else {
                // No condition id yet means we cannot redeem on-chain; the lot is
                // still held in-memory and a later Gamma update supplies the id.
                warn!(
                    target: "fade_live",
                    slug = %m.slug,
                    "real lot committed before condition id known; not yet ledgered"
                );
            }
        }
    }

    /// Collect redemption candidates from the persisted `pending_redeems`: each
    /// entry whose market closed at least `REDEEM_MARGIN_S` ago (resolution had
    /// time to settle) and is not already in the `redeemed` idempotency set.
    /// Returns the full `PendingRedeem` so the caller has the held index sets;
    /// state changes only via `mark_redeemed` after a successful submit.
    fn redeem_candidates(&self, now_ns: i64) -> Vec<PendingRedeem> {
        let margin_ns = REDEEM_MARGIN_S * 1_000_000_000;
        self.pending_redeems
            .iter()
            .filter(|pr| now_ns >= pr.close_ns + margin_ns)
            .filter(|pr| !self.redeemed.contains(&pr.condition_id))
            .cloned()
            .collect()
    }

    /// Mark a pending redemption complete: record the condition id (idempotency),
    /// remove it from `pending_redeems`, clear any matching in-memory real lots,
    /// and persist the trimmed ledger so a later sweep never resubmits.
    fn mark_redeemed(&mut self, slug: &str, condition_id: &str) {
        self.redeemed.insert(condition_id.to_string());
        self.pending_redeems.retain(|pr| pr.condition_id != condition_id);
        if let Some(m) = self.markets.get_mut(slug) {
            // Only real lots redeem on-chain; paper lots settle locally.
            m.held.retain(|l| l.is_paper);
        }
        self.persist_pending();
    }

    /// Locally settle paper lots whose market closed at least `REDEEM_MARGIN_S`
    /// ago. Outcome comes from the SAME Binance spot tape the belief uses: a
    /// Yes/up lot WON if spot-at-close > strike (the spot-at-open the belief
    /// already adopted), No/down inverse. Fee-net P&L per lot replicates the
    /// backtest's curve fee (0.07 * p * (1-p) * shares) on the single taker
    /// entry leg, hold-to-redemption (no exit leg). Returns the per-lot
    /// (slug, side, won, lot_pnl) records the caller logs; the running total +
    /// count are accumulated here. Idempotent: settled lots are removed.
    fn paper_settle_sweep(&mut self, now_ns: i64) -> Vec<PaperSettlement> {
        let cutoff_s = now_ns / 1_000_000_000 - REDEEM_MARGIN_S;
        let spot = self.spot_history();
        let mut out = Vec::new();
        for m in self.markets.values_mut() {
            if m.close_ts_s > cutoff_s || !m.held.iter().any(|l| l.is_paper) {
                continue;
            }
            let close_ns = m.close_ts_s * 1_000_000_000;
            let open_ns = m.open_ts_s * 1_000_000_000;
            // Self-consistent with the belief: strike = spot-at-open, outcome =
            // spot-at-close, both from the local tape. If either is unavailable
            // (tape gap), leave the lot for a later sweep rather than guess.
            let (Some(strike), Some(close_px)) = (
                spot.price_at_or_before(open_ns),
                spot.price_at_or_before(close_ns),
            ) else {
                continue;
            };
            let mut keep = Vec::new();
            for lot in m.held.drain(..) {
                if !lot.is_paper {
                    keep.push(lot);
                    continue;
                }
                let won = match lot.side {
                    Side::Yes => close_px > strike,
                    Side::No => close_px <= strike,
                };
                let payout = if won { 1.0 } else { 0.0 };
                let gross = lot.shares * (payout - lot.avg_price);
                let entry_fee = 0.07 * lot.avg_price * (1.0 - lot.avg_price) * lot.shares;
                let lot_pnl = gross - entry_fee;
                self.paper_pnl_total += lot_pnl;
                self.paper_lots_settled += 1;
                out.push(PaperSettlement {
                    slug: m.slug.clone(),
                    side: lot.side,
                    won,
                    lot_pnl,
                });
            }
            m.held = keep;
        }
        out
    }
}

/// One locally-settled paper lot's outcome (logged by the caller).
struct PaperSettlement {
    slug: String,
    side: Side,
    won: bool,
    lot_pnl: f64,
}

/// A qualifying entry the loop should log (always) and submit (when armed).
#[derive(Debug, Clone)]
struct EnterIntent {
    slug: String,
    side: Side,
    side_str: &'static str,
    token_id: String,
    limit_price: f64,
    /// Best ask of the bought side at decision (the touch). A marketable taker
    /// fills here, not at `limit_price` (the protective cap). Mirrors the
    /// backtest fill() entry_cost = side ask in replay.rs.
    fill_ask: f64,
    target_notional: f64,
    hold_to_redemption: bool,
    p_exo: f64,
    edge: f64,
    sigma_bar_bps: f64,
    strike: f64,
    now_ns: i64,
    delta_set_armed: Option<bool>,
    delta_next_entry_ns: Option<i64>,
    delta_inc_clips: bool,
}

/// What a qualifying entry does this tick: a real adapter submit, a local paper
/// fill, or nothing (shadow-only / capped-out / kill-switch). Exactly one path.
#[derive(Debug, Clone, Copy)]
enum FillAction {
    None,
    Live,
    Paper,
}

// SAFE-BY-DEFAULT real-money arm. Mirrors br2_live::Br2LiveShadow::from_env.

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

/// The dormant real-money arm + its belt-and-suspenders notional caps.
struct LiveArm {
    /// True only when EVERY precondition held at construction. The ONLY switch
    /// that lets the loop submit a real order.
    live_trade_armed: bool,
    /// Paper-trading arm: exercises the FULL order machinery (intent build,
    /// caps, kill-switch) but routes to a LOCAL fill simulator, never the
    /// adapter. MUTUALLY EXCLUSIVE with `live_trade_armed` (paper requires
    /// paper_mode, live requires !paper_mode), so both can never be true.
    paper_trade_armed: bool,
    /// Per-order notional cap (USD). Clips the clip down to fit.
    max_order_notional_usd: f64,
    /// Per-market cumulative submitted-notional cap (USD). Refuses beyond it.
    max_market_notional_usd: f64,
    /// Required kill-switch path. Per tick, if this file EXISTS we submit
    /// nothing and disarm (operator's emergency stop: `touch <path>`).
    kill_switch_path: Option<PathBuf>,
    /// Running submitted notional per market slug, for the cumulative cap.
    submitted_by_market: HashMap<String, f64>,
}

impl LiveArm {
    /// DORMANT by default. Arming requires ALL:
    ///   PM_FADE_LIVE_TRADE truthy AND !paper_mode AND
    ///   PM_FADE_LIVE_KILL_SWITCH_PATH set AND
    ///   PM_FADE_MAX_ORDER_NOTIONAL_USD > 0 AND
    ///   PM_FADE_MAX_MARKET_NOTIONAL_USD > 0.
    /// Any failure REFUSES: warn loudly, stay shadow-only.
    fn from_env() -> Self {
        let live_trade_requested = env_truthy("PM_FADE_LIVE_TRADE");
        // Paper mode is the safe default: only treat the runtime as live when
        // explicitly told it is NOT paper. PM_FADE_PAPER_MODE defaults true.
        let paper_mode = std::env::var("PM_FADE_PAPER_MODE")
            .ok()
            .map(|v| !matches!(v.trim().to_ascii_lowercase().as_str(), "false" | "0" | "no"))
            .unwrap_or(true);
        let kill_switch_path = std::env::var("PM_FADE_LIVE_KILL_SWITCH_PATH")
            .ok()
            .map(|v| v.trim().to_string())
            .filter(|v| !v.is_empty())
            .map(PathBuf::from);
        let max_order_notional_usd = env_positive_f64("PM_FADE_MAX_ORDER_NOTIONAL_USD");
        let max_market_notional_usd = env_positive_f64("PM_FADE_MAX_MARKET_NOTIONAL_USD");

        let preconditions_ok = !paper_mode
            && kill_switch_path.is_some()
            && max_order_notional_usd.is_some()
            && max_market_notional_usd.is_some();
        let live_trade_armed = live_trade_requested && preconditions_ok;

        // Paper arm: explicit opt-in AND paper_mode (the safe default). Because
        // live_trade_armed requires !paper_mode and paper requires paper_mode,
        // the two are MUTUALLY EXCLUSIVE by construction; assert it so a future
        // edit to the precondition logic can never arm both at once.
        let paper_trade_armed = env_truthy("PM_FADE_PAPER_TRADE") && paper_mode;
        assert!(
            !(live_trade_armed && paper_trade_armed),
            "live and paper arms are mutually exclusive"
        );

        if live_trade_requested && !live_trade_armed {
            warn!(
                target: "fade_live",
                paper_mode,
                kill_switch_configured = kill_switch_path.is_some(),
                max_order_notional_usd = ?max_order_notional_usd,
                max_market_notional_usd = ?max_market_notional_usd,
                "PM_FADE_LIVE_TRADE is set but a precondition is missing; REFUSING real-money \
                 submission. Required: !paper_mode (PM_FADE_PAPER_MODE=false) AND \
                 PM_FADE_LIVE_KILL_SWITCH_PATH non-empty AND PM_FADE_MAX_ORDER_NOTIONAL_USD>0 AND \
                 PM_FADE_MAX_MARKET_NOTIONAL_USD>0. fade stays SHADOW-ONLY (log only). No real \
                 orders will be placed."
            );
        }
        if live_trade_armed {
            warn!(
                target: "fade_live",
                max_order_notional_usd = max_order_notional_usd.unwrap_or(0.0),
                max_market_notional_usd = max_market_notional_usd.unwrap_or(0.0),
                kill_switch = ?kill_switch_path,
                "FADE REAL-MONEY submission ARMED. Real orders WILL be placed."
            );
        }

        // Mode banner: exactly one of LIVE / paper / shadow-only is active.
        let mode = if live_trade_armed {
            "LIVE"
        } else if paper_trade_armed {
            "paper"
        } else {
            "shadow-only"
        };
        info!(
            target: "fade_live",
            mode,
            live_trade_armed,
            paper_trade_armed,
            "fade arm mode resolved (LIVE=real orders, paper=local fills no signer, \
             shadow-only=log only)"
        );

        Self {
            live_trade_armed,
            paper_trade_armed,
            max_order_notional_usd: max_order_notional_usd.unwrap_or(0.0),
            max_market_notional_usd: max_market_notional_usd.unwrap_or(0.0),
            kill_switch_path,
            submitted_by_market: HashMap::new(),
        }
    }

    /// Per-tick kill-switch check: if the configured file exists, the loop must
    /// submit nothing and disarm. Returns true when submission is BLOCKED.
    fn kill_switch_tripped(&self) -> bool {
        self.kill_switch_path
            .as_ref()
            .map(|p| p.exists())
            .unwrap_or(false)
    }

    /// Clip a candidate notional to fit BOTH the per-order cap and the
    /// per-market remaining headroom. Returns the allowed notional (0 when the
    /// market is exhausted). Records nothing; call `record_submitted` after a
    /// real submit succeeds.
    fn cap_notional(&self, slug: &str, candidate: f64) -> f64 {
        let already = *self.submitted_by_market.get(slug).unwrap_or(&0.0);
        let market_headroom = (self.max_market_notional_usd - already).max(0.0);
        candidate.min(self.max_order_notional_usd).min(market_headroom)
    }

    fn record_submitted(&mut self, slug: &str, notional: f64) {
        *self.submitted_by_market.entry(slug.to_string()).or_insert(0.0) += notional;
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let _ = dotenvy::dotenv();

    if std::env::var("PM_FADE_ALLOW_DEPRECATED").ok().as_deref() != Some("1") {
        anyhow::bail!(
            "fade_live is HALTED: its reimplemented FadeCore diverged from the validated \
             pm-shadow engine and lost ~$700 overnight while shadow-final made money on \
             identical config (see docs/postmortem-2026-06-16-fade-live-divergence.md). \
             Use `cargo run -p polymarket-exec --bin shadow_live` instead. \
             Set PM_FADE_ALLOW_DEPRECATED=1 only for local adapter debugging — never real money."
        );
    }

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .json()
        .init();

    let clip_usd = env_positive_f64("PM_FADE_CLIP_USD").unwrap_or(DEFAULT_CLIP_USD);
    let cfg = decide_cfg(clip_usd);
    let arm = Arc::new(Mutex::new(LiveArm::from_env()));
    let (live_armed_at_start, paper_armed_at_start) = {
        let a = arm.lock().expect("arm poisoned");
        (a.live_trade_armed, a.paper_trade_armed)
    };

    info!(
        target: "fade_live",
        clip_usd,
        live_trade_armed = live_armed_at_start,
        paper_trade_armed = paper_armed_at_start,
        "fade_live starting (SAFE BY DEFAULT: shadow-only unless armed)"
    );

    // Connect the live execution adapter ONLY when LIVE-armed. Shadow-only AND
    // paper both leave this None: paper never constructs a signer / CLOB client,
    // so there is NO code path by which paper could place a real order.
    let adapter: Option<Arc<PolymarketExecutionAdapter>> = if live_armed_at_start {
        Some(Arc::new(connect_live_adapter().await?))
    } else {
        None
    };

    let core = Arc::new(Mutex::new(FadeCore::new(cfg)));

    // FadeCore::new already loaded the redeem ledger; report any pre-restart
    // pending redemptions the sweep will now resume.
    {
        let c = core.lock().expect("fade core poisoned");
        if !c.pending_redeems.is_empty() {
            info!(
                target: "fade_live",
                pending_redeems = c.pending_redeems.len(),
                ledger = %c.redeem_ledger_path.display(),
                "resumed pending redemptions from ledger"
            );
        }
    }

    // Pre-warm the spot vol buffer from Binance 1m klines so the strategy clears
    // the vol-lookback gate immediately instead of accumulating ~1h of live
    // tape. Best-effort: on failure, fall back to the natural live warm-up.
    match bootstrap_spot_from_klines(&core).await {
        Ok(n) => {
            let warm = core.lock().expect("fade core poisoned").warmed_up();
            info!(target: "fade_live", klines = n, warmed_up = warm,
                "spot buffer pre-warmed from Binance klines");
        }
        Err(e) => warn!(target: "fade_live", error = %e,
            "spot klines pre-warm failed; falling back to live warm-up (~1h)"),
    }

    // Pre-warm perp the same way: the model weights perp price at 0.75, so
    // without this the belief runs spot-only until the live perp tape spans the
    // lookback. Best-effort; warmed_up() gates on perp too, so a failed fetch
    // stands the strategy down rather than trading a degraded belief.
    match bootstrap_perp_from_klines(&core).await {
        Ok(n) => {
            let warm = core.lock().expect("fade core poisoned").warmed_up();
            info!(target: "fade_live", klines = n, warmed_up = warm,
                "perp buffer pre-warmed from Binance futures klines");
        }
        Err(e) => warn!(target: "fade_live", error = %e,
            "perp klines pre-warm failed; falling back to live warm-up (~1h)"),
    }

    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let (assets_tx, assets_rx) = watch::channel(Vec::<String>::new());

    let spot_task = tokio::spawn(feeds::binance_spot_feed(core.clone(), shutdown_rx.clone()));
    let perp_task = tokio::spawn(feeds::binance_perp_feed(core.clone(), shutdown_rx.clone()));
    let discovery_task = tokio::spawn(feeds::gamma_discovery_feed(
        core.clone(),
        assets_tx,
        shutdown_rx.clone(),
    ));
    let book_task = tokio::spawn(feeds::polymarket_book_feed(
        core.clone(),
        assets_rx,
        shutdown_rx.clone(),
    ));

    let mut tick = tokio::time::interval(Duration::from_millis(DECIDE_CADENCE_MS as u64));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut next_decide_ns = 0i64;

    loop {
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {
                info!(target: "fade_live", "SIGINT: shutting down");
                break;
            }
            _ = tick.tick() => {
                let now_ns = now_unix_ns();
                if now_ns < next_decide_ns {
                    continue;
                }
                next_decide_ns = now_ns + DECIDE_CADENCE_MS * 1_000_000;

                let intents = {
                    let mut core = core.lock().expect("fade core poisoned");
                    core.prune(now_ns);
                    core.decide(now_ns)
                };

                for intent in intents {
                    handle_enter(&core, &arm, adapter.as_deref(), intent).await;
                }

                // Post-close redemption sweep. Gated (armed + kill-switch) and
                // idempotent; a no-op in shadow/paper. Runs every tick but only
                // acts on markets past close + REDEEM_MARGIN_S with REAL lots.
                redeem_sweep(&core, &arm, adapter.as_deref(), now_ns).await;

                // Paper-settle sweep. Locally resolves paper lots against the
                // spot tape (no on-chain redeem); a no-op when no paper lots
                // are held. Idempotent: settled lots are removed.
                paper_settle_sweep(&core, now_ns);
            }
        }
    }

    let _ = shutdown_tx.send(true);
    let _ = tokio::time::timeout(
        Duration::from_secs(3),
        futures_util::future::join_all([spot_task, perp_task, discovery_task, book_task]),
    )
    .await;
    Ok(())
}

/// Always log a WouldEnter line (shadow-style). Then route by arm: LIVE submits
/// a marketable taker BUY through the adapter; PAPER builds the identical
/// SubmitOrderRequest but fills it in a LOCAL sim (no adapter, no signer);
/// shadow-only does neither. The per-market SSOT state advances in ALL cases,
/// keeping decisions byte-identical to the backtest. The kill-switch halts both
/// the live and paper arms.
async fn handle_enter(
    core: &Arc<Mutex<FadeCore>>,
    arm: &Arc<Mutex<LiveArm>>,
    adapter: Option<&PolymarketExecutionAdapter>,
    intent: EnterIntent,
) {
    info!(
        target: "fade_live",
        kind = "would_enter",
        slug = %intent.slug,
        side = intent.side_str,
        token = %intent.token_id,
        p_exo = intent.p_exo,
        edge = intent.edge,
        sigma_bar_bps = intent.sigma_bar_bps,
        strike = intent.strike,
        limit_price = intent.limit_price,
        target_notional = intent.target_notional,
        hold = intent.hold_to_redemption,
        "fade WOULD_ENTER"
    );

    // Decide what THIS tick does: a real submit (live arm), a local paper fill
    // (paper arm), or nothing (shadow-only / kill-switch). Caps are exercised in
    // both live and paper. A tripped kill-switch halts BOTH arms (operator stop)
    // and disarms; state then advances with zero fill, exactly like shadow.
    let (action, capped_notional) = {
        let mut a = arm.lock().expect("arm poisoned");
        if a.live_trade_armed {
            if a.kill_switch_tripped() {
                warn!(
                    target: "fade_live",
                    slug = %intent.slug,
                    "KILL-SWITCH file present: disarming and submitting nothing"
                );
                a.live_trade_armed = false;
                (FillAction::None, 0.0)
            } else {
                let capped = a.cap_notional(&intent.slug, intent.target_notional);
                (if capped > 0.0 { FillAction::Live } else { FillAction::None }, capped)
            }
        } else if a.paper_trade_armed {
            if a.kill_switch_tripped() {
                warn!(
                    target: "fade_live",
                    slug = %intent.slug,
                    "KILL-SWITCH file present: disarming paper arm and filling nothing"
                );
                a.paper_trade_armed = false;
                (FillAction::None, 0.0)
            } else {
                let capped = a.cap_notional(&intent.slug, intent.target_notional);
                (if capped > 0.0 { FillAction::Paper } else { FillAction::None }, capped)
            }
        } else {
            (FillAction::None, 0.0)
        }
    };

    let mut filled_shares = 0.0;
    let mut filled_price = 0.0;
    // Set when a real Live submit was attempted (venue_qty > 0) but the venue
    // rejected it without a fill (the "no orders found to match with FAK order"
    // case at the thin open of a new window). We then skip the state advance so
    // `decide()` re-emits next tick and the marketable taker retries, bounded by
    // the edge gate and the close-90s entry cutoff.
    let mut live_miss = false;
    let is_paper = matches!(action, FillAction::Paper);

    // Size shares off the expected fill VWAP (book-walk to the limit), not the
    // touch. A marketable taker sweeps the thin touch level up to `limit`, so
    // dividing the clip by the touch overshoots the deployed notional (~20% on
    // a thin book). Walking the live asks for `capped_notional` (full depth,
    // matching the backtest fill() with depth_capture_frac=1.0) yields the
    // realistic VWAP, so venue_qty = notional / VWAP deploys ~the clip. Falls
    // back to the touch if the book is unavailable.
    let limit = intent.limit_price.clamp(0.0, 1.0);
    let walk_vwap = {
        let core_g = core.lock().expect("fade core poisoned");
        core_g
            .books
            .get(&intent.token_id)
            .and_then(|l| l.vwap_for_notional(capped_notional, limit))
    };
    let fill_px = walk_vwap
        .unwrap_or_else(|| intent.fill_ask)
        .clamp(0.0, 1.0)
        .min(limit);
    let venue_qty = if fill_px > 0.0 {
        ((capped_notional / fill_px) * 100.0).floor() / 100.0
    } else {
        0.0
    };

    if matches!(action, FillAction::Paper) && venue_qty > 0.0 {
        // PAPER: build the SubmitOrderRequest EXACTLY as the live path does (so
        // construction is exercised: limit cap + touch-sized qty), then route to
        // the LOCAL fill sim instead of adapter.submit(). NO signer/adapter/network.
        let _req = SubmitOrderRequest {
            client_order_id: ClientOrderId::from(format!(
                "exo-fade-paper:{}:{}:{}",
                intent.slug, intent.side_str, intent.now_ns
            )),
            market_id: MarketId::from(intent.slug.as_str()),
            instrument_id: InstrumentId::from(intent.token_id.as_str()),
            side: TradeSide::Buy,
            limit_price: limit,
            quantity: venue_qty,
            post_only: false,
            time_in_force: TimeInForce::Ioc,
            expires_at_ms: None,
            strategy_tag: "exo-fade".to_string(),
            quote_level_tag: Some("exo-fade-taker".to_string()),
            submitted_at_ms: now_unix_ms() as u64,
        };
        info!(
            target: "fade_live",
            kind = "paper_submit",
            slug = %intent.slug,
            side = intent.side_str,
            token = %intent.token_id,
            limit_price = limit,
            fill_price = fill_px,
            quantity = venue_qty,
            "fade PAPER_SUBMIT (local sim, no adapter)"
        );
        // Local paper fill: a marketable IOC takes the touch; a small clip vs
        // book depth is assumed to fill fully at the touch ask (the same at-touch
        // model the backtest uses). Caps are exercised identically to live.
        filled_shares = venue_qty;
        filled_price = fill_px;
        arm.lock()
            .expect("arm poisoned")
            .record_submitted(&intent.slug, fill_px * venue_qty);
        info!(
            target: "fade_live",
            kind = "paper_fill",
            slug = %intent.slug,
            side = intent.side_str,
            token = %intent.token_id,
            price = fill_px,
            qty = venue_qty,
            "fade PAPER_FILL (assumed full at touch ask)"
        );
    }

    if matches!(action, FillAction::Live) && venue_qty > 0.0 {
        if let Some(adapter) = adapter {
            {
                {
                    let req = SubmitOrderRequest {
                        client_order_id: ClientOrderId::from(format!(
                            "exo-fade:{}:{}:{}",
                            intent.slug, intent.side_str, intent.now_ns
                        )),
                        // No condition-id->market_id map here; the slug is a
                        // stable per-market key for venue order attribution.
                        market_id: MarketId::from(intent.slug.as_str()),
                        instrument_id: InstrumentId::from(intent.token_id.as_str()),
                        side: TradeSide::Buy,
                        // The order's protective cap is the marketable limit; the
                        // qty is sized at the touch so the deployed notional is
                        // ~clip (mirrors the backtest's notional / avg_price).
                        limit_price: limit,
                        quantity: venue_qty,
                        post_only: false,
                        // exo-fade is a marketable taker that sweeps to a
                        // limit: IOC, never resting, never post-only. (A later
                        // step adds an `exo-fade-taker` prefix to the runner's
                        // tag-driven IOC routing; we set IOC directly here so
                        // this standalone binary is correct regardless.)
                        time_in_force: TimeInForce::Ioc,
                        expires_at_ms: None,
                        strategy_tag: "exo-fade".to_string(),
                        quote_level_tag: Some("exo-fade-taker".to_string()),
                        submitted_at_ms: now_unix_ms() as u64,
                    };
                    match adapter.submit(req).await {
                        Ok(ack) => {
                            // The adapter ack does not report a filled size; a
                            // marketable IOC buy is assumed filled at the touch
                            // for held-lot bookkeeping (the same at-touch model
                            // the backtest uses). The authoritative fill/position
                            // truth is the venue Data API (synced for redemption).
                            filled_shares = venue_qty;
                            filled_price = fill_px;
                            arm.lock()
                                .expect("arm poisoned")
                                .record_submitted(&intent.slug, fill_px * venue_qty);
                            info!(
                                target: "fade_live",
                                kind = "submitted",
                                slug = %intent.slug,
                                side = intent.side_str,
                                token = %intent.token_id,
                                limit_price = limit,
                                fill_price = fill_px,
                                quantity = venue_qty,
                                accepted = ack.accepted,
                                venue_order_id = ?ack.venue_order_id,
                                venue_message = ?ack.venue_message,
                                "fade SUBMITTED real-money taker buy"
                            );
                        }
                        Err(error) => {
                            live_miss = true;
                            warn!(
                                target: "fade_live",
                                slug = %intent.slug,
                                error = %error,
                                "fade submit MISS (no fill); retrying next tick"
                            );
                        }
                    }
                }
            }
        }
    }

    // A real Live submit that the venue rejected without a fill: leave the
    // per-market state untouched (clip not burned, still armed) so the next
    // decide tick re-emits and the taker retries into the building book.
    if live_miss {
        return;
    }

    // Advance the SSOT per-market state (cooldown/arming/clip + held lot).
    // hold_to_redemption is always true under the frozen config (exit_after_s
    // = 0): we NEVER sell. The held lot rides to resolution and redeems there.
    core.lock().expect("fade core poisoned").commit_entry(
        &intent.slug,
        intent.side,
        filled_price,
        filled_shares,
        is_paper,
        intent.delta_set_armed,
        intent.delta_next_entry_ns,
        intent.delta_inc_clips,
    );

    // Held lots ride to resolution; the periodic `redeem_sweep` (in the main
    // loop) recovers collateral post-close. Resolution detection here is
    // conservative: redeem once close_ns + REDEEM_MARGIN_S has elapsed; the
    // relayer/CTF rejects an unresolved market, which is a safe failure.
}

/// Periodic post-close redemption sweep. For each persisted `PendingRedeem`
/// whose market closed at least `REDEEM_MARGIN_S` ago, submit an on-chain redeem
/// of the index sets actually held (the winning leg pays $1/share; a held losing
/// leg returns nothing but the call succeeds). Sourced from the redeem ledger so
/// a restart resumes open redemptions. Mirrors `redeem_once.rs`'s request
/// construction via the already-connected adapter (no second signer).
///
/// Gated identically to submission: only runs when armed and the kill-switch is
/// clear (shadow/paper holds no real positions, so there is nothing to redeem).
/// Idempotent: redeemed condition ids are tracked and lots cleared on success.
async fn redeem_sweep(
    core: &Arc<Mutex<FadeCore>>,
    arm: &Arc<Mutex<LiveArm>>,
    adapter: Option<&PolymarketExecutionAdapter>,
    now_ns: i64,
) {
    // Gate exactly like submission: armed + kill-switch clear. A tripped
    // kill-switch means the operator is halting; skip redeem too (and disarm,
    // matching handle_enter).
    {
        let mut a = arm.lock().expect("arm poisoned");
        if !a.live_trade_armed {
            return;
        }
        if a.kill_switch_tripped() {
            warn!(
                target: "fade_live",
                "KILL-SWITCH file present: disarming and skipping redeem sweep"
            );
            a.live_trade_armed = false;
            return;
        }
    }

    let Some(adapter) = adapter else {
        return;
    };

    let candidates = {
        let core = core.lock().expect("fade core poisoned");
        core.redeem_candidates(now_ns)
    };

    for pending in candidates {
        let PendingRedeem { condition_id, slug, index_sets, .. } = pending;
        // Redeem only the index sets we actually hold (empty -> both, as a
        // safety fallback so a winning leg is never stranded).
        let index_sets = if index_sets.is_empty() { vec![1, 2] } else { index_sets };
        let req = RedeemPositionsRequest {
            command_id: ClientOrderId::from(format!("exo-fade-redeem:{slug}:{now_ns}")),
            market_id: MarketId::from(slug.as_str()),
            condition_id: condition_id.clone(),
            // Default collateral (active trading collateral).
            collateral_token_address: None,
            index_sets: index_sets.clone(),
            submitted_at_ms: now_unix_ms() as u64,
        };
        info!(
            target: "fade_live",
            kind = "redeem_submit",
            slug = %slug,
            condition_id = %condition_id,
            index_sets = ?index_sets,
            "fade SUBMITTING redeem (held index sets)"
        );
        match adapter.redeem_positions(req).await {
            Ok(ack) => {
                info!(
                    target: "fade_live",
                    kind = "redeem_ack",
                    slug = %slug,
                    condition_id = %condition_id,
                    accepted = ack.accepted,
                    venue_message = ?ack.venue_message,
                    "fade redeem ACK; clearing held lots"
                );
                core.lock()
                    .expect("fade core poisoned")
                    .mark_redeemed(&slug, &condition_id);
            }
            Err(error) => {
                // Most likely the market is not yet resolved on-chain (safe
                // rejection): leave the lots tracked so the next sweep retries.
                warn!(
                    target: "fade_live",
                    slug = %slug,
                    condition_id = %condition_id,
                    error = %error,
                    "fade redeem FAILED (likely unresolved); will retry next sweep"
                );
            }
        }
    }
}

/// Periodic paper-settle sweep. Locally resolves paper lots whose market closed
/// at least `REDEEM_MARGIN_S` ago, using the SAME Binance spot tape the belief
/// reads (no extra feed): a Yes/up lot won if spot-at-close > strike (the
/// spot-at-open the belief adopted), No/down inverse. No adapter, no signer, no
/// network. Logs one `paper_settle` per lot and a `paper_pnl` running total.
/// Idempotent: settled lots are removed inside `paper_settle_sweep`.
fn paper_settle_sweep(core: &Arc<Mutex<FadeCore>>, now_ns: i64) {
    let (settlements, total, count) = {
        let mut core = core.lock().expect("fade core poisoned");
        let s = core.paper_settle_sweep(now_ns);
        (s, core.paper_pnl_total, core.paper_lots_settled)
    };
    if settlements.is_empty() {
        return;
    }
    for s in &settlements {
        info!(
            target: "fade_live",
            kind = "paper_settle",
            slug = %s.slug,
            side = match s.side { Side::Yes => "up", Side::No => "down" },
            won = s.won,
            pnl = s.lot_pnl,
            "fade PAPER_SETTLE (local spot-vs-strike)"
        );
    }
    info!(
        target: "fade_live",
        kind = "paper_pnl",
        paper_pnl_total = total,
        paper_lots_settled = count,
        "fade PAPER_PNL running total"
    );
}

/// Build the live CLOB execution adapter from the standard Polymarket env
/// (same names redeem_once.rs / the runtime use). Only called when armed.
/// Pre-warm the spot vol buffer from Binance 1m klines (last ~70 min) so the
/// strategy clears the vol-lookback warm-up gate immediately on (re)start rather
/// than accumulating ~1h of live tape. Each kline contributes one point at its
/// closeTime (ms) + close price; the realized-vol estimate from 1m sampling is a
/// sound proxy and is refined by incoming ticks once the WS feed catches up.
async fn bootstrap_spot_from_klines(core: &Arc<Mutex<FadeCore>>) -> Result<usize> {
    bootstrap_klines(
        core,
        "https://api.binance.com/api/v3/klines?symbol=BTCUSDT&interval=1m&limit=70",
        false,
    )
    .await
}

/// Perp counterpart: the model blends the perp price at 0.75 weight, so a fresh
/// process must pre-warm `perp_buf` too or it trades a spot-only belief until the
/// live perp tape spans the lookback (~1h). USDT-M futures klines, same shape.
async fn bootstrap_perp_from_klines(core: &Arc<Mutex<FadeCore>>) -> Result<usize> {
    bootstrap_klines(
        core,
        "https://fapi.binance.com/fapi/v1/klines?symbol=BTCUSDT&interval=1m&limit=70",
        true,
    )
    .await
}

async fn bootstrap_klines(
    core: &Arc<Mutex<FadeCore>>,
    url: &str,
    perp: bool,
) -> Result<usize> {
    let body: serde_json::Value = reqwest::get(url).await?.json().await?;
    let arr = body
        .as_array()
        .context("binance klines response was not an array")?;
    let mut core = core.lock().expect("fade core poisoned");
    let mut n = 0usize;
    for k in arr {
        let close_ms = k.get(6).and_then(serde_json::Value::as_i64);
        let close_px = k
            .get(4)
            .and_then(serde_json::Value::as_str)
            .and_then(|s| s.parse::<f64>().ok());
        if let (Some(ms), Some(px)) = (close_ms, close_px) {
            if perp {
                core.push_perp(ms, px, 0.0);
            } else {
                core.push_spot(ms, px, 0.0, false);
            }
            n += 1;
        }
    }
    Ok(n)
}

async fn connect_live_adapter() -> Result<PolymarketExecutionAdapter> {
    let private_key = std::env::var("POLYMARKET_PRIVATE_KEY")
        .or_else(|_| std::env::var("METAMASK_PRIVATE_KEY"))
        .context("POLYMARKET_PRIVATE_KEY must be set to arm real-money submission")?;
    let signature_type_raw =
        std::env::var("POLYMARKET_SIGNATURE_TYPE").unwrap_or_else(|_| "eoa".to_string());
    let signature_type = PolymarketSignatureType::parse(&signature_type_raw)
        .map_err(|e| anyhow::anyhow!("invalid POLYMARKET_SIGNATURE_TYPE: {e:?}"))?;
    let funder_address = std::env::var("POLYMARKET_FUNDER_ADDRESS")
        .ok()
        .or_else(|| std::env::var("POLYMARKET_PROXY_WALLET_ADDRESS").ok());

    let credentials = PolymarketL1Credentials {
        private_key,
        signature_type,
        funder_address: funder_address.clone(),
    };

    // Build the full execution config (V2 protocol + relayer), mirroring the
    // proven redeem_once / runner env reads. poly_1271 deposit wallets REQUIRE
    // the CLOB V2 path + relayer creds; the bare connect_with_l1 (V1 default)
    // errors on them (as_legacy_sdk). Start from defaults (correct CTF /
    // collateral / builder constants, neg_risk=false for BTC-5m) and override
    // only the env-driven fields.
    let env_some =
        |a: &str, b: &str| std::env::var(a).ok().or_else(|| std::env::var(b).ok());
    let mut config = PolymarketConfig::default();
    if let Ok(v) = std::env::var("POLYMARKET_CLOB_API_URL") {
        config.api_url = v;
    }
    if let Ok(v) = std::env::var("POLYMARKET_DATA_API_URL") {
        config.data_api_url = v;
    }
    if let Ok(v) = std::env::var("POLYMARKET_RELAYER_URL") {
        config.relayer_url = v;
    }
    config.relayer_api_key = env_some("RELAYER_API_KEY", "POLYMARKET_RELAYER_API_KEY");
    config.relayer_api_key_address =
        env_some("RELAYER_API_KEY_ADDRESS", "POLYMARKET_RELAYER_API_KEY_ADDRESS");
    if let Ok(v) = std::env::var("POLYMARKET_CTF_CONTRACT_ADDRESS") {
        config.ctf_contract_address = v;
    }
    if let Some(v) = env_some(
        "POLYMARKET_CTF_COLLATERAL_TOKEN_ADDRESS",
        "POLYMARKET_COLLATERAL_TOKEN_ADDRESS",
    ) {
        config.ctf_collateral_token_address = v;
    }
    if let Ok(v) = std::env::var("POLYMARKET_COLLATERAL_TOKEN_ADDRESS") {
        config.collateral_token_address = v;
    }
    config.proxy_wallet_address = funder_address;
    config.polygon_rpc_url = std::env::var("POLYGON_RPC_URL").ok();
    config.protocol = ClobProtocolVersion::parse(
        &std::env::var("POLYMARKET_CLOB_VERSION").unwrap_or_else(|_| "v2".to_string()),
    )
    .map_err(|e| anyhow::anyhow!("invalid POLYMARKET_CLOB_VERSION: {e:?}"))?;

    // Use the EXISTING L2 API creds: the V2 SDK consumes config.credentials and
    // skips POST /auth/api-key, which 400s with "Could not create api key" when
    // the account already has a key (as ours does). Without these it tries to
    // create a key and every order fails. Falls back to None (create/derive)
    // only if any are unset.
    if let (Ok(api_key), Ok(api_secret), Ok(api_passphrase)) = (
        std::env::var("POLYMARKET_API_KEY"),
        std::env::var("POLYMARKET_API_SECRET"),
        std::env::var("POLYMARKET_API_PASSPHRASE"),
    ) {
        config.credentials = Some(PolymarketCredentials {
            api_key,
            api_secret,
            api_passphrase,
            private_key: credentials.private_key.clone(),
            signature_type: credentials.signature_type,
            funder_address: credentials.funder_address.clone(),
        });
    }

    PolymarketExecutionAdapter::connect_with_l1_config(config, credentials)
        .await
        .map_err(|e| anyhow::anyhow!("failed to connect live execution adapter: {e}"))
}

/// Live data feeds. Read-only consumers of public endpoints: the only
/// outbound payloads are websocket subscriptions and pings. Mirrors the
/// pm_app::shadow feed module (which lives in a different crate and cannot be
/// imported here).
mod feeds {
    use super::{EntryState, FadeCore, MarketWindow};
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use anyhow::{Context, Result};
    use futures_util::{SinkExt, StreamExt};
    use serde_json::Value;
    use tokio::sync::watch;
    use tokio_tungstenite::{connect_async, tungstenite::Message};
    use tracing::{debug, info, warn};

    const BINANCE_SPOT_WS_URL: &str = "wss://stream.binance.com:9443/ws/btcusdt@trade";
    const BINANCE_PERP_WS_URL: &str = "wss://fstream.binance.com/ws/btcusdt@aggTrade";
    const PM_BOOK_WS_URL: &str = "wss://ws-subscriptions-clob.polymarket.com/ws/market";
    const GAMMA_MARKETS_URL: &str = "https://gamma-api.polymarket.com/markets";
    const DISCOVERY_INTERVAL: Duration = Duration::from_secs(20);
    const STALE_TIMEOUT: Duration = Duration::from_secs(45);
    const MAX_BACKOFF: Duration = Duration::from_secs(30);

    type Core = Arc<Mutex<FadeCore>>;

    fn now_ms() -> i64 {
        super::now_unix_ms()
    }

    async fn backoff_sleep(backoff: &mut Duration) {
        tokio::time::sleep(*backoff).await;
        *backoff = (*backoff * 2).min(MAX_BACKOFF);
    }

    fn value_f64(v: Option<&Value>) -> Option<f64> {
        match v? {
            Value::Number(n) => n.as_f64(),
            Value::String(s) => s.trim().parse().ok(),
            _ => None,
        }
    }

    fn value_i64(v: Option<&Value>) -> Option<i64> {
        match v? {
            Value::Number(n) => n.as_i64(),
            Value::String(s) => s.trim().parse().ok(),
            _ => None,
        }
    }

    // Binance spot

    pub async fn binance_spot_feed(core: Core, mut shutdown: watch::Receiver<bool>) {
        let mut backoff = Duration::from_secs(1);
        while !*shutdown.borrow() {
            match spot_once(&core, &mut shutdown).await {
                Ok(()) => break,
                Err(error) => {
                    warn!(target: "fade_live", ?error, "binance spot feed failed; reconnecting");
                    backoff_sleep(&mut backoff).await;
                }
            }
        }
    }

    async fn spot_once(core: &Core, shutdown: &mut watch::Receiver<bool>) -> Result<()> {
        let (stream, _) = connect_async(BINANCE_SPOT_WS_URL)
            .await
            .context("connecting binance spot ws")?;
        let (mut write, mut read) = stream.split();
        let mut pings = tokio::time::interval(Duration::from_secs(15));
        pings.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let mut last_frame = tokio::time::Instant::now();
        loop {
            tokio::select! {
                _ = shutdown.changed() => {
                    let _ = write.send(Message::Close(None)).await;
                    return Ok(());
                }
                _ = pings.tick() => {
                    if last_frame.elapsed() > STALE_TIMEOUT {
                        anyhow::bail!("binance spot ws stale");
                    }
                    write.send(Message::Ping(Vec::new().into())).await.context("spot ping")?;
                }
                frame = read.next() => {
                    last_frame = tokio::time::Instant::now();
                    match frame {
                        Some(Ok(Message::Text(text))) => {
                            if let Err(error) = handle_spot_text(core, &text) {
                                warn!(target: "fade_live", ?error, "skipping malformed spot message");
                            }
                        }
                        Some(Ok(Message::Ping(payload))) => { write.send(Message::Pong(payload)).await.ok(); }
                        Some(Ok(Message::Close(_))) => anyhow::bail!("spot ws closed by remote"),
                        Some(Ok(_)) => {}
                        Some(Err(error)) => return Err(error).context("spot ws frame error"),
                        None => anyhow::bail!("spot ws stream ended"),
                    }
                }
            }
        }
    }

    fn handle_spot_text(core: &Core, text: &str) -> Result<()> {
        let payload: Value = serde_json::from_str(text).context("decode spot payload")?;
        if payload.get("e").and_then(Value::as_str) != Some("trade") {
            return Ok(());
        }
        let (Some(price), Some(qty)) =
            (value_f64(payload.get("p")), value_f64(payload.get("q")))
        else {
            anyhow::bail!("spot trade missing price/quantity");
        };
        let exchange_ms = value_i64(payload.get("T"))
            .or_else(|| value_i64(payload.get("E")))
            .context("spot trade missing T/E")?;
        let is_buyer_maker = payload.get("m").and_then(Value::as_bool).unwrap_or(false);
        core.lock()
            .expect("fade core poisoned")
            .push_spot(exchange_ms, price, qty, is_buyer_maker);
        Ok(())
    }

    // Binance perp (futures)

    pub async fn binance_perp_feed(core: Core, mut shutdown: watch::Receiver<bool>) {
        let mut backoff = Duration::from_secs(1);
        while !*shutdown.borrow() {
            match perp_once(&core, &mut shutdown).await {
                Ok(()) => break,
                Err(error) => {
                    warn!(target: "fade_live", ?error, "binance perp feed failed; reconnecting");
                    backoff_sleep(&mut backoff).await;
                }
            }
        }
    }

    async fn perp_once(core: &Core, shutdown: &mut watch::Receiver<bool>) -> Result<()> {
        let (stream, _) = connect_async(BINANCE_PERP_WS_URL)
            .await
            .context("connecting binance perp ws")?;
        let (mut write, mut read) = stream.split();
        let mut pings = tokio::time::interval(Duration::from_secs(15));
        pings.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let mut last_frame = tokio::time::Instant::now();
        loop {
            tokio::select! {
                _ = shutdown.changed() => {
                    let _ = write.send(Message::Close(None)).await;
                    return Ok(());
                }
                _ = pings.tick() => {
                    if last_frame.elapsed() > STALE_TIMEOUT {
                        anyhow::bail!("binance perp ws stale");
                    }
                    write.send(Message::Ping(Vec::new().into())).await.context("perp ping")?;
                }
                frame = read.next() => {
                    last_frame = tokio::time::Instant::now();
                    match frame {
                        Some(Ok(Message::Text(text))) => {
                            if let Err(error) = handle_perp_text(core, &text) {
                                warn!(target: "fade_live", ?error, "skipping malformed perp message");
                            }
                        }
                        Some(Ok(Message::Ping(payload))) => { write.send(Message::Pong(payload)).await.ok(); }
                        Some(Ok(Message::Close(_))) => anyhow::bail!("perp ws closed by remote"),
                        Some(Ok(_)) => {}
                        Some(Err(error)) => return Err(error).context("perp ws frame error"),
                        None => anyhow::bail!("perp ws stream ended"),
                    }
                }
            }
        }
    }

    fn handle_perp_text(core: &Core, text: &str) -> Result<()> {
        let payload: Value = serde_json::from_str(text).context("decode perp payload")?;
        if payload.get("e").and_then(Value::as_str) != Some("aggTrade") {
            return Ok(());
        }
        let (Some(price), Some(qty)) =
            (value_f64(payload.get("p")), value_f64(payload.get("q")))
        else {
            anyhow::bail!("perp aggTrade missing price/quantity");
        };
        let exchange_ms = value_i64(payload.get("T"))
            .or_else(|| value_i64(payload.get("E")))
            .context("perp aggTrade missing T/E")?;
        core.lock()
            .expect("fade core poisoned")
            .push_perp(exchange_ms, price, qty);
        Ok(())
    }

    // Gamma market discovery

    pub async fn gamma_discovery_feed(
        core: Core,
        assets_tx: watch::Sender<Vec<String>>,
        mut shutdown: watch::Receiver<bool>,
    ) {
        let client = reqwest::Client::new();
        let mut ticker = tokio::time::interval(DISCOVERY_INTERVAL);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        while !*shutdown.borrow() {
            tokio::select! {
                _ = shutdown.changed() => break,
                _ = ticker.tick() => {
                    let now_s = now_ms() / 1_000;
                    let current_open = now_s - now_s.rem_euclid(300);
                    for open_ts in [current_open, current_open + 300] {
                        let slug = format!("{}{}", super::SLUG_PREFIX, open_ts);
                        match fetch_gamma_market(&client, &slug).await {
                            Ok(Some(market)) => {
                                core.lock().expect("fade core poisoned").upsert_market(market);
                            }
                            Ok(None) => debug!(target: "fade_live", slug, "gamma no market yet"),
                            Err(error) => warn!(target: "fade_live", ?error, slug, "gamma fetch failed"),
                        }
                    }
                    let tokens = core.lock().expect("fade core poisoned").subscribed_tokens();
                    if *assets_tx.borrow() != tokens {
                        let _ = assets_tx.send(tokens);
                    }
                }
            }
        }
    }

    async fn fetch_gamma_market(client: &reqwest::Client, slug: &str) -> Result<Option<MarketWindow>> {
        let payload = client
            .get(GAMMA_MARKETS_URL)
            .query(&[("slug", slug)])
            .header("User-Agent", "fade-live/1.0")
            .header("Accept", "application/json")
            .timeout(Duration::from_secs(10))
            .send()
            .await?
            .error_for_status()?
            .json::<Value>()
            .await?;
        let Some(items) = payload.as_array() else {
            return Ok(None);
        };
        Ok(items.iter().find_map(|item| parse_gamma_market(item, slug)))
    }

    fn parse_gamma_market(item: &Value, slug: &str) -> Option<MarketWindow> {
        if item.get("slug").and_then(Value::as_str) != Some(slug) {
            return None;
        }
        let open_ts_s: i64 = slug.rsplit('-').next()?.parse().ok()?;
        let tokens = parse_json_string_list(item.get("clobTokenIds"));
        let outcomes = parse_json_string_list(item.get("outcomes"));
        let (up_token, down_token) = order_up_down(&tokens, &outcomes)?;
        // CTF index set = 1 << (token position in clobTokenIds). `order_up_down`
        // already guaranteed tokens.len() >= 2; tokens[0] is set 1, else set 2.
        let up_index_set: u64 = if up_token == tokens[0] { 1 } else { 2 };
        let down_index_set: u64 = if down_token == tokens[0] { 1 } else { 2 };
        let condition_id = item
            .get("conditionId")
            .or_else(|| item.get("condition_id"))
            .and_then(Value::as_str)
            .map(str::to_string);
        Some(MarketWindow {
            slug: slug.to_string(),
            open_ts_s,
            close_ts_s: open_ts_s + 300,
            up_token,
            down_token,
            up_index_set,
            down_index_set,
            condition_id,
            entry: EntryState { armed: true, next_entry_ns: i64::MIN },
            n_clips: 0,
            held: Vec::new(),
        })
    }

    fn parse_json_string_list(value: Option<&Value>) -> Vec<String> {
        match value {
            Some(Value::Array(items)) => items
                .iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect(),
            Some(Value::String(raw)) => serde_json::from_str::<Value>(raw)
                .ok()
                .map(|v| parse_json_string_list(Some(&v)))
                .unwrap_or_default(),
            _ => Vec::new(),
        }
    }

    fn order_up_down(tokens: &[String], outcomes: &[String]) -> Option<(String, String)> {
        if tokens.len() < 2 {
            return None;
        }
        if outcomes.len() == tokens.len() {
            let mut up = None;
            let mut down = None;
            for (token, outcome) in tokens.iter().zip(outcomes) {
                match outcome.trim().to_ascii_lowercase().as_str() {
                    "up" | "yes" => up = Some(token.clone()),
                    "down" | "no" => down = Some(token.clone()),
                    _ => {}
                }
            }
            if let (Some(up), Some(down)) = (up, down) {
                return Some((up, down));
            }
        }
        Some((tokens[0].clone(), tokens[1].clone()))
    }

    // Polymarket book websocket

    enum BookExit {
        Shutdown,
        AssetsChanged,
    }

    pub async fn polymarket_book_feed(
        core: Core,
        mut assets_rx: watch::Receiver<Vec<String>>,
        mut shutdown: watch::Receiver<bool>,
    ) {
        let mut backoff = Duration::from_secs(1);
        while !*shutdown.borrow() {
            let assets = assets_rx.borrow_and_update().clone();
            if assets.is_empty() {
                tokio::select! {
                    _ = shutdown.changed() => return,
                    _ = assets_rx.changed() => continue,
                    _ = tokio::time::sleep(Duration::from_secs(2)) => continue,
                }
            }
            match book_once(&core, &assets, &mut assets_rx, &mut shutdown).await {
                Ok(BookExit::Shutdown) => return,
                Ok(BookExit::AssetsChanged) => {
                    backoff = Duration::from_secs(1);
                    info!(target: "fade_live", "book subscription set changed; resubscribing");
                }
                Err(error) => {
                    warn!(target: "fade_live", ?error, "book feed failed; reconnecting");
                    backoff_sleep(&mut backoff).await;
                }
            }
        }
    }

    async fn book_once(
        core: &Core,
        assets: &[String],
        assets_rx: &mut watch::Receiver<Vec<String>>,
        shutdown: &mut watch::Receiver<bool>,
    ) -> Result<BookExit> {
        let (stream, _) = connect_async(PM_BOOK_WS_URL)
            .await
            .context("connecting polymarket book ws")?;
        let (mut write, mut read) = stream.split();
        let subscribe = serde_json::json!({
            "assets_ids": assets,
            "type": "market",
        });
        write
            .send(Message::Text(subscribe.to_string().into()))
            .await
            .context("book subscribe")?;
        let mut pings = tokio::time::interval(Duration::from_secs(10));
        pings.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let mut last_frame = tokio::time::Instant::now();
        loop {
            tokio::select! {
                _ = shutdown.changed() => {
                    let _ = write.send(Message::Close(None)).await;
                    return Ok(BookExit::Shutdown);
                }
                _ = assets_rx.changed() => {
                    let _ = write.send(Message::Close(None)).await;
                    return Ok(BookExit::AssetsChanged);
                }
                _ = pings.tick() => {
                    if last_frame.elapsed() > Duration::from_secs(45) {
                        anyhow::bail!("book ws stale");
                    }
                    write.send(Message::Text("PING".to_string().into())).await.context("book ping")?;
                }
                frame = read.next() => {
                    last_frame = tokio::time::Instant::now();
                    match frame {
                        Some(Ok(Message::Text(text))) => {
                            if text == "PONG" { continue; }
                            if let Err(error) = handle_book_text(core, &text) {
                                warn!(target: "fade_live", ?error, "skipping malformed book message");
                            }
                        }
                        Some(Ok(Message::Ping(payload))) => { write.send(Message::Pong(payload)).await.ok(); }
                        Some(Ok(Message::Close(_))) => anyhow::bail!("book ws closed by remote"),
                        Some(Ok(_)) => {}
                        Some(Err(error)) => return Err(error).context("book ws frame error"),
                        None => anyhow::bail!("book ws stream ended"),
                    }
                }
            }
        }
    }

    fn handle_book_text(core: &Core, text: &str) -> Result<()> {
        let payload: Value = serde_json::from_str(text).context("decode book payload")?;
        match payload {
            Value::Array(items) => {
                for item in items {
                    handle_book_event(core, &item);
                }
            }
            Value::Object(_) => handle_book_event(core, &payload),
            _ => {}
        }
        Ok(())
    }

    fn parse_levels(value: Option<&Value>) -> Vec<(f64, f64)> {
        value
            .and_then(Value::as_array)
            .map(|levels| {
                levels
                    .iter()
                    .filter_map(|level| {
                        let price = value_f64(level.get("price").or_else(|| level.get("p")))?;
                        let size = value_f64(level.get("size").or_else(|| level.get("s")))
                            .unwrap_or(0.0);
                        Some((price, size))
                    })
                    .collect()
            })
            .unwrap_or_default()
    }

    fn handle_book_event(core: &Core, event: &Value) {
        let event_type = event.get("event_type").and_then(Value::as_str).unwrap_or_else(|| {
            if event.get("bids").is_some() || event.get("asks").is_some() {
                "book"
            } else {
                "unknown"
            }
        });
        match event_type {
            "book" => {
                let Some(asset_id) = event.get("asset_id").and_then(Value::as_str) else {
                    return;
                };
                let bids = parse_levels(event.get("bids"));
                let asks = parse_levels(event.get("asks"));
                core.lock()
                    .expect("fade core poisoned")
                    .apply_book_snapshot(asset_id, &bids, &asks);
            }
            "price_change" => {
                let changes = event
                    .get("price_changes")
                    .or_else(|| event.get("pc"))
                    .and_then(Value::as_array)
                    .cloned()
                    .unwrap_or_default();
                let mut core = core.lock().expect("fade core poisoned");
                for change in changes {
                    let Some(asset_id) = change
                        .get("asset_id")
                        .or_else(|| change.get("a"))
                        .and_then(Value::as_str)
                    else {
                        continue;
                    };
                    let price = value_f64(change.get("price").or_else(|| change.get("p")));
                    let size = value_f64(change.get("size").or_else(|| change.get("s")));
                    // Polymarket `side`: BUY -> bid level, SELL -> ask level.
                    let is_buy_side = change
                        .get("side")
                        .and_then(Value::as_str)
                        .map(|s| s.eq_ignore_ascii_case("buy"))
                        .unwrap_or(true);
                    if let (Some(price), Some(size)) = (price, size) {
                        core.apply_price_change(asset_id, is_buy_side, price, size);
                    }
                }
            }
            _ => {}
        }
    }

    #[cfg(test)]
    mod tests {
        use serde_json::json;

        // up_token == tokens[0] -> up_index_set 1, down_index_set 2.
        #[test]
        fn index_set_maps_to_token_position() {
            let item = json!({
                "slug": "btc-updown-5m-1700000000",
                "clobTokenIds": ["TOK_UP", "TOK_DOWN"],
                "outcomes": ["Up", "Down"],
                "conditionId": "0xCID",
            });
            let m = super::parse_gamma_market(&item, "btc-updown-5m-1700000000")
                .expect("market should parse");
            assert_eq!(m.up_token, "TOK_UP");
            assert_eq!(m.down_token, "TOK_DOWN");
            assert_eq!(m.up_index_set, 1);
            assert_eq!(m.down_index_set, 2);
        }

        // Reversed token order: down is tokens[0] -> down_index_set 1, up 2.
        #[test]
        fn index_set_maps_reversed_token_order() {
            let item = json!({
                "slug": "btc-updown-5m-1700000300",
                "clobTokenIds": ["TOK_DOWN", "TOK_UP"],
                "outcomes": ["Down", "Up"],
                "conditionId": "0xCID2",
            });
            let m = super::parse_gamma_market(&item, "btc-updown-5m-1700000300")
                .expect("market should parse");
            assert_eq!(m.up_token, "TOK_UP");
            assert_eq!(m.down_token, "TOK_DOWN");
            assert_eq!(m.down_index_set, 1);
            assert_eq!(m.up_index_set, 2);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::PendingRedeem;

    #[test]
    fn pending_redeem_serde_roundtrip() {
        let entries = vec![
            PendingRedeem {
                condition_id: "0xabc".to_string(),
                slug: "btc-updown-5m-1700000000".to_string(),
                close_ns: 1_700_000_300_000_000_000,
                index_sets: vec![1],
            },
            PendingRedeem {
                condition_id: "0xdef".to_string(),
                slug: "btc-updown-5m-1700000300".to_string(),
                close_ns: 1_700_000_600_000_000_000,
                index_sets: vec![1, 2],
            },
        ];
        let json = serde_json::to_string(&entries).expect("serialize");
        let back: Vec<PendingRedeem> = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(entries, back);
    }
}
