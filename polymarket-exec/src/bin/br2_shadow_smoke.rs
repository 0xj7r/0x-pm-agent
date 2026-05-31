//! Smoke test for the live br2 shadow adapter.
//!
//! Instantiates `Br2ShadowAdapter` with the 062901 champion params, optionally
//! loads the frozen 062901 meta-calibrator snapshot (path via the first CLI arg
//! or `BR2_SNAPSHOT_PATH`), feeds a synthetic BTC-5m market end-to-end, and logs
//! the shadow orders br2 WOULD place. Submits nothing.
//!
//! Run: `cargo run -p polymarket-exec --bin br2_shadow_smoke [snapshot.json]`

use polymarket_exec::runtime::br2_shadow::{
    Br2ShadowAdapter, SpotTrade, YesTopOfBook,
};

const NS_PER_MS: i64 = 1_000_000;
const MARKET_WINDOW_NS: i64 = 300 * 1_000_000_000;

fn ms(n: i64) -> i64 {
    n * NS_PER_MS
}

fn main() -> anyhow::Result<()> {
    let snapshot_path = std::env::args()
        .nth(1)
        .or_else(|| std::env::var("BR2_SNAPSHOT_PATH").ok());

    let snapshot = match snapshot_path.as_deref() {
        Some(p) => {
            let snap = Br2ShadowAdapter::load_snapshot_from_path(std::path::Path::new(p))?;
            println!(
                "loaded 062901 meta-calibrator snapshot from {p} (updates={}, beta_enabled={})",
                snap.updates,
                snap.beta_enabled()
            );
            Some(snap)
        }
        None => {
            println!("no snapshot path supplied; running with a fresh (untrained) calibrator");
            None
        }
    };

    let mut adapter = Br2ShadowAdapter::new(snapshot);

    // Synthetic Binance tape: a BTC uptrend with realistic tick noise so the
    // realized-vol gate (1.25 bps over 180s) is exercised, aggressor buys
    // dominating (is_buyer_maker = false => aggressive buy).
    let market_open_ns = ms(1_000_000);
    let market_close_ns = market_open_ns + MARKET_WINDOW_NS;
    let mut price = 64_000.0_f64;
    let mut t = market_open_ns - ms(180_000); // 180s warmup for vol window
    let mut step = 0u64;
    while t <= market_close_ns {
        // Trend + deterministic oscillation (no rng dep): ~6 bps swings.
        step += 1;
        let wobble = ((step as f64 * 0.7).sin()) * 18.0;
        price += 1.2 + wobble;
        adapter.on_spot_trade(SpotTrade {
            ts_ns: t,
            price,
            quantity: 5.0,
            is_buyer_maker: step % 5 == 0, // mostly aggressive buys
        });
        t += ms(200);
    }

    adapter.on_market_open(42, market_open_ns, market_close_ns);
    println!("opened synthetic BTC-5m market id=42, 5-minute window");

    let mut total = 0usize;
    let mut decision_t = market_open_ns;
    while decision_t < market_close_ns {
        let secs_to_close = (market_close_ns - decision_t) as f64 / 1e9;
        // Late favourite at ~0.74 ask: a strong-but-not-priced-in favourite so
        // the model edge (calibrated_p - ask) can clear the 9c champion gate.
        let yes_ask = if secs_to_close < 120.0 { 0.74 } else { 0.55 };
        let yes_bid = yes_ask - 0.02;
        let tob = YesTopOfBook {
            ts_ns: decision_t,
            yes_bid,
            yes_bid_size: 500.0,
            yes_ask,
            yes_ask_size: 500.0,
        };
        for order in adapter.on_decision(&tob) {
            println!("{}", order.log_line());
            total += 1;
        }
        decision_t += ms(1_000);
    }

    if let Some(stats) = adapter.gate_stats() {
        println!(
            "gate stats (late-favourite lane): window_checks={} checks={} skew_fail={} low_vol_fail={} whipsaw_fail={} model_conf_fail={} model_risk_fail={} model_edge_fail={} emits={}",
            stats.late_favourite_window_checks,
            stats.late_favourite_checks,
            stats.late_favourite_skew_fail,
            stats.late_favourite_low_vol_fail,
            stats.late_favourite_whipsaw_fail,
            stats.late_favourite_model_confidence_fail,
            stats.late_favourite_model_risk_fail,
            stats.late_favourite_model_edge_fail,
            stats.late_favourite_emits,
        );
    }

    adapter.on_market_close();
    println!("smoke test complete: br2 produced {total} shadow order(s), submitted 0");
    Ok(())
}
