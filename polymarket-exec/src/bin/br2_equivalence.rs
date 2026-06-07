//! br2 live-port EQUIVALENCE TEST (Phase 3 safety gate).
//!
//! PROVES that br2's decisions on the LIVE code path (`Br2ShadowAdapter`) are
//! byte/value-identical to the CANONICAL BACKTEST path on the SAME input data,
//! using the SAME crate (read-only br2 from polymarket-backtest) and the SAME
//! frozen 062901 meta-calibrator snapshot.
//!
//! Two decision streams are produced over an identical set of BTC-5m markets:
//!   (a) BACKTEST reference: the canonical decision loop reproduced verbatim from
//!       pm-app/src/runner.rs `run_backtest` (lines ~682-770): per-event model
//!       eval -> `Ctx` -> `BonereaperV2::on_event` -> external model gate
//!       (`enforce_model_gate`: conf>=0.68, risk<=0.72, side-edge>=0.00, with the
//!       `order_requires_model_gate` tag predicate). Position feedback evolves via
//!       an immediate depth-weighted taker fill (shared by both paths so it cannot
//!       itself cause a reference-vs-adapter divergence).
//!   (b) LIVE-PATH: the same events fed through `Br2ShadowAdapter::on_decision_event`
//!       with the same evolving position. The adapter owns the trailing spot tape,
//!       the frozen-snapshot model load, the 062901 champion config, and the same
//!       external model gate.
//!
//! The two streams are diffed per market and per decision. They MUST be identical
//! (same side, shares, max_depth, limit_price, tag). Any divergence is reported
//! with the event index, market, and field that differs. Submits NOTHING.
//!
//! Inputs are constructed deterministically and made diverse/adversarial enough to
//! exercise every br2 lane (late-confirm, high-skew, late-favourite, convex tail),
//! gate rejections, the NO=1-yes mapping, and position-aware (side-lock / tail)
//! lanes across multiple markets. Because BOTH paths consume byte-identical events,
//! spot tapes (with real aggressor flags), trades, position, model-config, and
//! gate, the test isolates exactly the port-fidelity question: does the live
//! adapter feed br2 the same inputs the backtest does?
//!
//! Run:
//!   BR2_SNAPSHOT_PATH=/tmp/snap062901.json \
//!     cargo run -p polymarket-exec --bin br2_equivalence

use pm_model::{ModelMarketContext, ModelOutput, ModelState};
use pm_strategy::{BonereaperV2, Ctx, OrderRequest, Side, Strategy, StrategyOutput};
use pm_types::{
    BookLevel, MarketId, ReplayEvent, ReplayFlags, SpotHistory, SpotTick, TradeHistory, TAPE_DEPTH,
};

use polymarket_exec::runtime::br2_shadow::{
    order_adds_yes_exposure, order_requires_model_gate, Br2ShadowAdapter, DecisionPosition,
    ModelGate, ShadowOrder, SpotTrade,
};

const NS_PER_S: i64 = 1_000_000_000;
const MARKET_WINDOW_NS: i64 = 300 * NS_PER_S;

/// A canonical/adapter decision, normalised for an exact value comparison.
#[derive(Debug, Clone, PartialEq)]
struct Decision {
    market_id: u32,
    event_idx: usize,
    side: Side,
    shares_milli: i64, // shares * 1000, rounded — exact integer compare
    max_depth: usize,
    limit_milli: Option<i64>, // limit * 10000, rounded
    tag: String,
}

impl Decision {
    fn from_order(event_idx: usize, o: &ShadowOrder) -> Self {
        Self {
            market_id: o.market_id,
            event_idx,
            side: o.side,
            shares_milli: (o.shares * 1000.0).round() as i64,
            max_depth: o.max_depth,
            limit_milli: o.limit_price.map(|p| (p as f64 * 10000.0).round() as i64),
            tag: o.tag.to_string(),
        }
    }

    fn from_request(market_id: u32, event_idx: usize, req: &OrderRequest) -> Self {
        Self {
            market_id,
            event_idx,
            side: req.side,
            shares_milli: (req.shares * 1000.0).round() as i64,
            max_depth: req.max_depth,
            limit_milli: req.limit_price.map(|p| (p as f64 * 10000.0).round() as i64),
            tag: req.tag.to_string(),
        }
    }
}

/// One synthetic-but-representative BTC-5m market: a deterministic spot tape with
/// real aggressor flags and a YES book that evolves through a regime the lanes
/// react to.
struct MarketSpec {
    id: u32,
    open_ns: i64,
    /// final YES-favourite ask the market climbs toward (drives which lane fires).
    favourite_ask: f32,
    /// whether spot trends up (YES wins) or down (NO wins).
    up: bool,
    /// extra book depth so multi-level sweeps (depth up to 7) are exercised.
    deep_book: bool,
}

fn build_spot_tape(spec: &MarketSpec) -> Vec<SpotTrade> {
    // 180s of warmup before open so the 180s realized-vol window is populated.
    let mut out = Vec::new();
    let mut price = 64_000.0_f64 + (spec.id as f64) * 50.0;
    let mut t = spec.open_ns - 180 * NS_PER_S;
    let close = spec.open_ns + MARKET_WINDOW_NS;
    let mut step = 0u64;
    let dir = if spec.up { 1.0 } else { -1.0 };
    while t <= close {
        step += 1;
        // Trend + deterministic oscillation: ~6 bps swings to clear the vol gate.
        let wobble = ((step as f64 * 0.6 + spec.id as f64).sin()) * 16.0;
        price += dir * 1.4 + wobble;
        out.push(SpotTrade {
            ts_ns: t,
            price,
            quantity: 4.0 + (step % 7) as f32,
            // aggressor: trend-aligned prints dominate. is_buyer_maker=true is a
            // seller-initiated (aggressive-sell) print; false is aggressive-buy.
            is_buyer_maker: if spec.up {
                step % 4 == 0
            } else {
                step % 4 != 0
            },
        });
        t += NS_PER_S / 5; // 5 prints/sec
    }
    out
}

fn yes_book_at(spec: &MarketSpec, ts_ns: i64) -> (f32, f32, f32, f32) {
    // YES climbs toward favourite_ask in the last 120s; flat-ish early.
    let secs_to_close = ((spec.open_ns + MARKET_WINDOW_NS - ts_ns) as f64 / 1e9).max(0.0);
    let target = if spec.up {
        spec.favourite_ask
    } else {
        1.0 - spec.favourite_ask
    };
    let ask = if secs_to_close < 120.0 {
        target
    } else if secs_to_close < 200.0 {
        0.5 + (target - 0.5) * 0.4
    } else {
        0.50
    };
    let ask = ask.clamp(0.03, 0.98);
    let bid = (ask - 0.02).clamp(0.01, ask - 0.005);
    (bid, ask, 500.0, 500.0)
}

/// Build the per-market `ReplayEvent` stream (1 decision/sec, the 062901
/// replay-sample cadence) plus the `SpotHistory` and `TradeHistory`.
fn build_market_events(spec: &MarketSpec, spot_hist: &SpotHistory) -> Vec<ReplayEvent> {
    let mut events = Vec::new();
    let close = spec.open_ns + MARKET_WINDOW_NS;
    let mut t = spec.open_ns;
    while t < close {
        let (bid, ask, bid_sz, ask_sz) = yes_book_at(spec, t);
        let mut bids = [BookLevel::default(); TAPE_DEPTH];
        let mut asks = [BookLevel::default(); TAPE_DEPTH];
        bids[0] = BookLevel {
            price: bid,
            size: bid_sz,
        };
        asks[0] = BookLevel {
            price: ask,
            size: ask_sz,
        };
        if spec.deep_book {
            // Populate a few deeper levels so sweep-depth sizing is exercised.
            for lvl in 1..TAPE_DEPTH {
                let d = lvl as f32 * 0.01;
                bids[lvl] = BookLevel {
                    price: (bid - d).max(0.01),
                    size: 300.0,
                };
                asks[lvl] = BookLevel {
                    price: (ask + d).min(0.99),
                    size: 300.0,
                };
            }
        }
        let yes_mid = 0.5 * (bid + ask);
        let spot_now = spot_hist.price_at_or_before(t).unwrap_or(0.0) as f32;
        events.push(ReplayEvent {
            ts_ns: t,
            market_id: MarketId(spec.id),
            yes_mid,
            yes_bid: bid,
            yes_ask: ask,
            volume: 0.0,
            bids,
            asks,
            spot_price: spot_now,
            flags: ReplayFlags::BOOK_UPDATE,
        });
        t += NS_PER_S; // 1s decision cadence
    }
    events
}

/// Immediate depth-weighted taker fill, mirroring `depth_weighted_fill` in
/// pm-app runner.rs. Used to evolve the SHARED position fed to both paths.
fn depth_weighted_fill(event: &ReplayEvent, req: &Decision) -> Option<(f32, f64)> {
    let depth = req.max_depth.clamp(1, TAPE_DEPTH);
    let shares = req.shares_milli as f64 / 1000.0;
    let mut remaining = shares.max(0.0);
    let mut filled = 0.0;
    let mut notional = 0.0;
    let limit = req.limit_milli.map(|m| m as f32 / 10000.0);
    for level in 0..depth {
        let (price, size) = match req.side {
            Side::BuyYes => (event.asks[level].price, event.asks[level].size),
            Side::SellYes => (event.bids[level].price, event.bids[level].size),
            Side::BuyNo => (
                (1.0 - event.bids[level].price).max(0.0),
                event.bids[level].size,
            ),
            Side::SellNo => (
                (1.0 - event.asks[level].price).max(0.0),
                event.asks[level].size,
            ),
        };
        if price <= 0.0 || price >= 1.0 || size <= 0.0 {
            continue;
        }
        let respects = match (req.side, limit) {
            (_, None) => true,
            (Side::BuyYes | Side::BuyNo, Some(l)) => price <= l,
            (Side::SellYes | Side::SellNo, Some(l)) => price >= l,
        };
        if !respects {
            continue;
        }
        let take = remaining.min(size as f64);
        if take <= 0.0 {
            break;
        }
        filled += take;
        notional += take * price as f64;
        remaining -= take;
        if remaining <= 1e-9 {
            break;
        }
    }
    if filled <= 0.0 {
        return None;
    }
    Some(((notional / filled) as f32, filled))
}

/// Apply a filled order to the running share position (YES/NO inventory only;
/// cash is tracked loosely — br2 reads shares, not cash, for its position lanes).
fn apply_fill(yes: &mut f64, no: &mut f64, side: Side, filled: f64) {
    match side {
        Side::BuyYes => *yes += filled,
        Side::SellYes => *yes -= filled,
        Side::BuyNo => *no += filled,
        Side::SellNo => *no -= filled,
    }
}

/// The canonical external model gate, reproduced verbatim from run_backtest.
fn gate_passes(gate: &ModelGate, req: &OrderRequest, model: &ModelOutput, yes_mid: f32) -> bool {
    if !(gate.enforce && order_requires_model_gate(req.tag)) {
        return true;
    }
    let yes_side = order_adds_yes_exposure(req.side);
    let side_edge = pm_model::side_edge_vs_mid(model, yes_mid, yes_side).clamp(0.0, 1.0);
    if model.confidence_score < gate.min_confidence {
        return false;
    }
    if model.risk_score > gate.max_risk {
        return false;
    }
    if side_edge < gate.min_edge {
        return false;
    }
    true
}

fn main() -> anyhow::Result<()> {
    let snapshot_path = std::env::var("BR2_SNAPSHOT_PATH")
        .ok()
        .or_else(|| std::env::args().nth(1));

    let snapshot = match snapshot_path.as_deref() {
        Some(p) => {
            let snap = Br2ShadowAdapter::load_snapshot_from_path(std::path::Path::new(p))?;
            println!(
                "REAL frozen snapshot loaded: {p}  (updates={}, beta_enabled={}, isotonic_pts={})",
                snap.updates,
                snap.beta_enabled(),
                snap.isotonic.thresholds.len(),
            );
            Some(snap)
        }
        None => {
            eprintln!(
                "WARNING: no BR2_SNAPSHOT_PATH; running with a FRESH calibrator. \
                 Equivalence still holds (both paths share it) but this is NOT the \
                 frozen-snapshot parity run. Set BR2_SNAPSHOT_PATH=/tmp/snap062901.json."
            );
            None
        }
    };

    // A diverse market sample: up/down resolutions, varying favourite strength,
    // shallow/deep books. Enough to exercise every lane + gate + position path.
    let mut specs = Vec::new();
    let base_open = 1_772_200_000i64 * NS_PER_S;
    for i in 0..40u32 {
        specs.push(MarketSpec {
            id: 1000 + i,
            open_ns: base_open + (i as i64) * (MARKET_WINDOW_NS + 30 * NS_PER_S),
            favourite_ask: 0.70 + (i % 5) as f32 * 0.06, // 0.70..0.94
            up: i % 2 == 0,
            deep_book: i % 3 == 0,
        });
    }

    // The reference (backtest) path is built INDEPENDENTLY from the canonical
    // 062901 sources so it can genuinely catch adapter drift:
    //   - strategy/model config: the champion config the backtest used. We pull
    //     these from the public champion constructors (the same source the
    //     command file documents); they are NOT routed through the live adapter's
    //     decision path, so an adapter bug still shows up as a mismatch.
    //   - model gate: hardcoded to the 062901 command-file values
    //     (--model-gate-min-confidence 0.68 / --model-gate-max-risk 0.72 /
    //      --model-gate-min-edge 0.00, enforced). If the adapter's gate drifts
    //     from these, the diff trips.
    let strat_cfg = polymarket_exec::runtime::br2_shadow::champion_062901_config();
    let model_cfg = polymarket_exec::runtime::br2_shadow::champion_062901_model_config();
    let gate = ModelGate {
        enforce: true,
        min_confidence: 0.68,
        max_risk: 0.72,
        min_edge: 0.00,
    };

    let mut total_decisions = 0usize;
    let mut mismatches = 0usize;
    let mut markets_with_decisions = 0usize;
    let mut per_lane: std::collections::BTreeMap<String, usize> = std::collections::BTreeMap::new();
    let mut first_divergences: Vec<String> = Vec::new();

    for spec in &specs {
        // Shared spot tape + history (identical bytes for both paths).
        let spot_trades = build_spot_tape(spec);
        let spot_ticks: Vec<SpotTick> = spot_trades
            .iter()
            .map(|t| SpotTick {
                ts_ns: t.ts_ns,
                price: t.price,
                quantity: t.quantity,
                is_buyer_maker: t.is_buyer_maker,
            })
            .collect();
        let spot_hist = SpotHistory::new(spot_ticks.clone());
        let trades = TradeHistory::default();
        let events = build_market_events(spec, &spot_hist);

        // --- LIVE-PATH adapter setup: feed the same spot tape, open the market. ---
        let mut adapter = Br2ShadowAdapter::new(snapshot.clone());
        for t in &spot_trades {
            adapter.on_spot_trade(*t);
        }
        adapter.on_market_open(spec.id, spec.open_ns, spec.open_ns + MARKET_WINDOW_NS);

        // --- BACKTEST reference setup: fresh strategy + model with same snapshot. ---
        let mut ref_strategy = BonereaperV2::new(strat_cfg.clone());
        let mut ref_model = ModelState::new();
        if let Some(snap) = &snapshot {
            ref_model.load_meta_calibrator_snapshot(snap.clone());
        }

        // Position is SHARED: both paths see the identical evolving inventory.
        let mut yes_shares = 0.0f64;
        let mut no_shares = 0.0f64;
        let mut cash = strat_cfg.bankroll_usdc;
        let mut events_seen = 0u64;
        let mut market_yes_min = f32::INFINITY;
        let mut market_yes_max = f32::NEG_INFINITY;

        let mut market_decided = false;

        for (idx, event) in events.iter().enumerate() {
            events_seen += 1;
            market_yes_min = market_yes_min.min(event.yes_mid);
            market_yes_max = market_yes_max.max(event.yes_mid);
            let yes_range = if market_yes_min.is_finite() && market_yes_max.is_finite() {
                market_yes_max - market_yes_min
            } else {
                0.0
            };
            let secs_since_open = ((event.ts_ns - spec.open_ns).max(0) as f64) / 1e9;

            // ---- (a) BACKTEST reference decision (verbatim run_backtest path) ----
            let ref_eval = ref_model.evaluate_detailed_with_market_context(
                event,
                &spot_hist,
                secs_since_open as f32,
                &model_cfg,
                ModelMarketContext::default(),
            );
            let ref_ctx = Ctx {
                events_seen,
                yes_shares,
                no_shares,
                cash_usdc: cash,
                market_yes_range_so_far: yes_range,
                prior_market_range_1d: 0.0,
                prior_market_range_3d: 0.0,
                prior_market_range_7d: 0.0,
                model_output: Some(ref_eval.output),
                market_close_ns: spec.open_ns + MARKET_WINDOW_NS,
                ..Ctx::default()
            };
            let ref_out: StrategyOutput =
                ref_strategy.on_event(event, &ref_ctx, &spot_hist, &trades);
            let mut ref_decisions: Vec<Decision> = Vec::new();
            for req in &ref_out.orders {
                if gate_passes(&gate, req, &ref_eval.output, event.yes_mid) {
                    ref_decisions.push(Decision::from_request(spec.id, idx, req));
                }
            }

            // ---- (b) LIVE-PATH adapter decision (same event + same position) ----
            let pos = DecisionPosition {
                events_seen,
                yes_shares,
                no_shares,
                cash_usdc: cash,
            };
            let live_orders = adapter.on_decision_event(event, pos, &trades);
            let live_decisions: Vec<Decision> = live_orders
                .iter()
                .map(|o| Decision::from_order(idx, o))
                .collect();

            // ---- DIFF the two decision streams for this event ----
            if ref_decisions != live_decisions {
                mismatches += 1;
                if first_divergences.len() < 20 {
                    first_divergences.push(format!(
                        "market={} event_idx={} secs_open={:.1} yes_mid={:.4} side_p(calib)={:.4} dir={:.3} conf={:.3} risk={:.3}\n    BACKTEST: {:?}\n    LIVE    : {:?}",
                        spec.id,
                        idx,
                        secs_since_open,
                        event.yes_mid,
                        ref_eval.output.calibrated_p,
                        ref_eval.output.direction_score,
                        ref_eval.output.confidence_score,
                        ref_eval.output.risk_score,
                        ref_decisions,
                        live_decisions,
                    ));
                }
            }

            // Count + record lane tags (use the matched stream).
            for d in &ref_decisions {
                total_decisions += 1;
                *per_lane.entry(d.tag.clone()).or_default() += 1;
                if !market_decided {
                    market_decided = true;
                    markets_with_decisions += 1;
                }
            }

            // ---- Evolve the SHARED position from the gate-passed orders ----
            for d in &ref_decisions {
                if let Some((px, filled)) = depth_weighted_fill(event, d) {
                    apply_fill(&mut yes_shares, &mut no_shares, d.side, filled);
                    // Loose cash bookkeeping (br2 lanes do not gate on exact cash here).
                    let signed = match d.side {
                        Side::BuyYes | Side::BuyNo => -(filled * px as f64),
                        Side::SellYes | Side::SellNo => filled * px as f64,
                    };
                    cash += signed;
                }
            }
        }

        adapter.on_market_close();
    }

    println!();
    println!("================ br2 LIVE-vs-BACKTEST EQUIVALENCE ================");
    println!("markets compared          : {}", specs.len());
    println!("markets that decided      : {}", markets_with_decisions);
    println!("total decisions (matched) : {}", total_decisions);
    println!("decision-stream mismatches: {}", mismatches);
    let total_events: usize = specs.len() * (MARKET_WINDOW_NS / NS_PER_S) as usize;
    let match_rate = if total_events == 0 {
        100.0
    } else {
        100.0 * (total_events - mismatches) as f64 / total_events as f64
    };
    println!(
        "per-event match rate      : {:.4}%  ({} / {} events identical)",
        match_rate,
        total_events - mismatches,
        total_events
    );
    println!("decisions by lane tag     :");
    for (tag, n) in &per_lane {
        println!("    {tag:32} {n}");
    }

    if mismatches == 0 {
        println!();
        println!("VERDICT: live == backtest. Decision streams are VALUE-IDENTICAL across");
        println!(
            "all {} markets / {} events. Live path faithfully reproduces the",
            specs.len(),
            total_events
        );
        println!("validated br2 strategy. Phase-3 equivalence gate: PASS. (NO live orders.)");
        Ok(())
    } else {
        println!();
        println!("DIVERGENCES (first {}):", first_divergences.len());
        for d in &first_divergences {
            println!("  {d}");
        }
        println!();
        println!("VERDICT: live != backtest. {mismatches} divergent events. Gate: FAIL.");
        std::process::exit(1);
    }
}
