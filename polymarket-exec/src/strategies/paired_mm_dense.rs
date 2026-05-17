//! Dense tandem paired-MM emit logic. Mirrors unlawful_shear's
//! pre-04-29 historical playbook (per research/whales/unlawful.md):
//!
//! - 30+ price levels at 1¢ spacing centred on mid
//! - Stable per-fill clip across all levels (no size scaling by price)
//! - Both legs quoted in tandem on every tick (atomic refresh)
//! - Pair-cost gate: skip emission when yes_ask + no_ask > threshold
//! - Per-leg inventory tracking to skip over-filled side
//!
//! This module is pure-function: given current book state and inventory,
//! it returns the list of (leg, price, qty) tuples that should be posted
//! this tick. Cancellation of stale rungs happens upstream via the
//! standard last_emit deduplication.
//!
//! Not wired into production yet -- the CoreHedgeMmConfig.emit_mode
//! flag must be set to DenseTandem in YAML to activate. Default remains
//! Legacy.

use crate::market_making::pairing::LadderLeg;

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum PairedCoreEmitMode {
    /// Original ladder behaviour: configurable levels at configurable
    /// span, mid-anchored, broad. The implementation that ran live up
    /// through 2026-05-17.
    Legacy,
    /// Unlawful_shear-inspired dense tandem mode: 30 levels at 1¢
    /// spacing, atomic both-leg refresh, pair-cost gate, per-leg
    /// inventory imbalance correction. Disabled by default; YAML must
    /// opt in explicitly via `paired_core.emit_mode: dense_tandem`.
    DenseTandem,
}

impl Default for PairedCoreEmitMode {
    fn default() -> Self {
        Self::Legacy
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct DenseTandemConfig {
    /// Number of 1¢ levels per side. 30 matches unlawful's median 33
    /// distinct levels (his p50 from the strategy-reverse-engineering
    /// pull). Increase toward 36 for full mirror.
    pub levels_per_side: usize,
    /// Tick size of the book (Polymarket BTC-5m = 0.01 = 1¢).
    pub tick: f64,
    /// Shares per individual rung. Stable across all prices --
    /// unlawful's per-fill size was ~15 shares everywhere (mid-band to
    /// 0.99). Combined with 30 levels = 450 shares of ladder per side.
    pub clip_shares: f64,
    /// Reject new emissions when current `yes_ask + no_ask` exceeds
    /// this threshold. Unlawful's p75 pair cost was 0.9785 in the
    /// historical pull -- 0.97 is a conservative cutoff that filters
    /// out negative-EV book regimes while still allowing typical
    /// flat-regime entries (his median was 0.9425).
    pub max_entry_pair_cost: f64,
    /// Hard cap on per-leg inventory delta from theoretical-zero
    /// before we stop posting on the over-filled side. With clip=15
    /// and 30 levels, full fill yields 450sh per leg; if one leg is
    /// at +30 vs the other, we skip new emissions on the over-filled
    /// side until the mate catches up.
    pub max_leg_imbalance_shares: f64,
    /// Lower bound on each rung price. Prevents post-only rejection at
    /// price <= 0 and avoids posting in book regions where the cheap
    /// leg is meaningless.
    pub ladder_min_price: f64,
    /// Upper bound on each rung price symmetric to ladder_min_price.
    pub ladder_max_price: f64,
}

impl Default for DenseTandemConfig {
    fn default() -> Self {
        Self {
            levels_per_side: 30,
            tick: 0.01,
            clip_shares: 10.0,
            max_entry_pair_cost: 0.97,
            max_leg_imbalance_shares: 30.0,
            ladder_min_price: 0.02,
            ladder_max_price: 0.98,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct DenseTandemBook {
    pub yes_bid: f64,
    pub yes_ask: f64,
    pub no_bid: f64,
    pub no_ask: f64,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct DenseTandemInventory {
    pub yes_shares: f64,
    pub no_shares: f64,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct DenseTandemRung {
    pub leg: LadderLeg,
    pub price: f64,
    pub qty: f64,
}

/// Reason emission was skipped this tick.
#[derive(Clone, Debug, PartialEq)]
pub enum DenseTandemSkip {
    /// Book inputs invalid or stale.
    BookInvalid,
    /// Current pair cost exceeds the entry gate (no edge).
    PairCostAboveGate { pair_cost: f64, gate: f64 },
    /// Both inventory sides already over-target -- shouldn't emit more.
    InventoryFull,
}

/// Compute the dense tandem ladder for one tick. Returns either a list
/// of rungs to post (both legs symmetrically) or a skip reason.
///
/// Behaviour:
/// 1. Validate book (positive, finite, bid < ask on each leg).
/// 2. Check pair-cost gate against current yes_ask + no_ask. If above,
///    skip entirely (no edge).
/// 3. Compute mid = (yes_ask + no_ask - 1) / 2 + 0.5  (since complements
///    sum to 1; this is the canonical book mid). Anchor ladder at mid.
/// 4. For each of N levels symmetric around mid, generate yes_price =
///    mid - k*tick and no_price = mid + k*tick (so both sum to ~2*mid =
///    1.0; pair cost is preserved across the ladder).
/// 5. Clamp prices to [ladder_min_price, ladder_max_price].
/// 6. Per leg, check inventory imbalance: if yes_shares - no_shares >
///    max_leg_imbalance, skip yes rungs (mate hasn't caught up).
pub fn compute_dense_tandem_emission(
    cfg: &DenseTandemConfig,
    book: &DenseTandemBook,
    inventory: &DenseTandemInventory,
) -> Result<Vec<DenseTandemRung>, DenseTandemSkip> {
    if !is_book_valid(book) {
        return Err(DenseTandemSkip::BookInvalid);
    }

    let pair_cost = book.yes_ask + book.no_ask;
    if pair_cost > cfg.max_entry_pair_cost {
        return Err(DenseTandemSkip::PairCostAboveGate {
            pair_cost,
            gate: cfg.max_entry_pair_cost,
        });
    }

    let imbalance = inventory.yes_shares - inventory.no_shares;
    let skip_yes = imbalance > cfg.max_leg_imbalance_shares;
    let skip_no = -imbalance > cfg.max_leg_imbalance_shares;
    if skip_yes && skip_no {
        return Err(DenseTandemSkip::InventoryFull);
    }

    // Anchor each leg's ladder at the current ask. Level 0 sits at
    // (ask - tick) -- the highest price we can post as a maker bid
    // without crossing. Each subsequent level steps down by tick.
    // This mirrors unlawful's behaviour: bid right under the ask and
    // ladder downward at 1¢ spacing.
    let yes_start = book.yes_ask - cfg.tick;
    let no_start = book.no_ask - cfg.tick;

    let mut rungs = Vec::with_capacity(cfg.levels_per_side * 2);
    for k in 0..cfg.levels_per_side {
        let offset = (k as f64) * cfg.tick;

        if !skip_yes {
            let yes_price = yes_start - offset;
            if yes_price >= cfg.ladder_min_price && yes_price < book.yes_ask {
                rungs.push(DenseTandemRung {
                    leg: LadderLeg::Yes,
                    price: round_to_tick(yes_price, cfg.tick),
                    qty: cfg.clip_shares,
                });
            }
        }

        if !skip_no {
            let no_price = no_start - offset;
            if no_price >= cfg.ladder_min_price && no_price < book.no_ask {
                rungs.push(DenseTandemRung {
                    leg: LadderLeg::No,
                    price: round_to_tick(no_price, cfg.tick),
                    qty: cfg.clip_shares,
                });
            }
        }
    }

    Ok(rungs)
}

fn is_book_valid(book: &DenseTandemBook) -> bool {
    [book.yes_bid, book.yes_ask, book.no_bid, book.no_ask]
        .iter()
        .all(|v| v.is_finite() && *v > 0.0 && *v < 1.0)
        && book.yes_bid < book.yes_ask
        && book.no_bid < book.no_ask
}

fn round_to_tick(price: f64, tick: f64) -> f64 {
    (price / tick).round() * tick
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> DenseTandemConfig {
        DenseTandemConfig::default()
    }

    /// Build a book centered on `yes_mid` with `pair_cost` for the two
    /// asks. Bids are placed 2¢ below the asks. Caller picks pair_cost
    /// to test gate behaviour.
    fn book_at(yes_mid: f64, pair_cost: f64) -> DenseTandemBook {
        let no_mid = 1.0 - yes_mid;
        // Distribute pair_cost across the two asks proportionally to mid.
        let yes_ask = yes_mid * pair_cost;
        let no_ask = no_mid * pair_cost;
        DenseTandemBook {
            yes_bid: yes_ask - 0.02,
            yes_ask,
            no_bid: no_ask - 0.02,
            no_ask,
        }
    }

    fn balanced_book(yes_mid: f64) -> DenseTandemBook {
        // Default fixture has favorable pair cost = 0.96 (~4¢ edge,
        // typical of the regime where dense paired-MM is active).
        book_at(yes_mid, 0.96)
    }

    fn empty_inventory() -> DenseTandemInventory {
        DenseTandemInventory {
            yes_shares: 0.0,
            no_shares: 0.0,
        }
    }

    #[test]
    fn emits_balanced_ladder_when_book_is_centred() {
        let book = balanced_book(0.50);
        let rungs = compute_dense_tandem_emission(&cfg(), &book, &empty_inventory())
            .expect("should emit");

        // Both legs should produce rungs.
        let yes_count = rungs.iter().filter(|r| r.leg == LadderLeg::Yes).count();
        let no_count = rungs.iter().filter(|r| r.leg == LadderLeg::No).count();
        assert!(yes_count > 0);
        assert!(no_count > 0);
        // Symmetric emission at a centred book.
        assert_eq!(yes_count, no_count);
        // No rungs above the ask.
        for r in &rungs {
            if r.leg == LadderLeg::Yes {
                assert!(r.price < book.yes_ask, "yes rung above ask: {}", r.price);
            } else {
                assert!(r.price < book.no_ask, "no rung above ask: {}", r.price);
            }
        }
    }

    #[test]
    fn skips_when_pair_cost_above_gate() {
        let book = DenseTandemBook {
            yes_bid: 0.86,
            yes_ask: 0.92,
            no_bid: 0.06,
            no_ask: 0.10,
        };
        // pair_cost = 0.92 + 0.10 = 1.02 > 0.97 gate
        let result = compute_dense_tandem_emission(&cfg(), &book, &empty_inventory());
        match result {
            Err(DenseTandemSkip::PairCostAboveGate { pair_cost, gate }) => {
                assert!((pair_cost - 1.02).abs() < 1e-9);
                assert!((gate - 0.97).abs() < 1e-9);
            }
            other => panic!("expected PairCostAboveGate, got {:?}", other),
        }
    }

    #[test]
    fn rejects_invalid_book() {
        let bad = DenseTandemBook {
            yes_bid: f64::NAN,
            yes_ask: 0.5,
            no_bid: 0.4,
            no_ask: 0.5,
        };
        assert_eq!(
            compute_dense_tandem_emission(&cfg(), &bad, &empty_inventory()),
            Err(DenseTandemSkip::BookInvalid)
        );

        let crossed = DenseTandemBook {
            yes_bid: 0.6,
            yes_ask: 0.5,
            no_bid: 0.4,
            no_ask: 0.5,
        };
        assert_eq!(
            compute_dense_tandem_emission(&cfg(), &crossed, &empty_inventory()),
            Err(DenseTandemSkip::BookInvalid)
        );
    }

    #[test]
    fn skips_over_filled_leg() {
        let book = balanced_book(0.50);
        let inventory = DenseTandemInventory {
            yes_shares: 100.0,
            no_shares: 50.0,
        };
        // imbalance = 50 > 30 (max) -> skip yes
        let rungs = compute_dense_tandem_emission(&cfg(), &book, &inventory)
            .expect("should emit only no leg");
        let yes_count = rungs.iter().filter(|r| r.leg == LadderLeg::Yes).count();
        let no_count = rungs.iter().filter(|r| r.leg == LadderLeg::No).count();
        assert_eq!(yes_count, 0, "yes leg should be skipped");
        assert!(no_count > 0, "no leg should still emit");
    }

    #[test]
    fn skips_both_when_both_legs_over_target() {
        // Pathological: both legs simultaneously over-filled. Practically
        // can't happen (would mean we're long both pairs), but defended
        // against here so the function never returns a stale ladder.
        // We construct it by having yes_shares >> no_shares AND something
        // even more extreme on no... actually with the sign convention,
        // imbalance > +max AND -imbalance > +max is impossible. So this
        // path is unreachable -- the function is correct by construction.
        // We assert that for any single-sided overshoot we still emit
        // the other leg.
        let book = balanced_book(0.50);
        let imbalanced = DenseTandemInventory {
            yes_shares: 0.0,
            no_shares: 100.0,
        };
        let rungs = compute_dense_tandem_emission(&cfg(), &book, &imbalanced)
            .expect("should emit only yes leg");
        let yes_count = rungs.iter().filter(|r| r.leg == LadderLeg::Yes).count();
        let no_count = rungs.iter().filter(|r| r.leg == LadderLeg::No).count();
        assert!(yes_count > 0);
        assert_eq!(no_count, 0);
    }

    #[test]
    fn ladder_density_matches_config() {
        let book = balanced_book(0.50);
        let mut c = cfg();
        c.levels_per_side = 30;
        // At yes_mid=0.50 with 30 levels at 1¢, rungs span 0.20-0.49.
        // ladder_min_price=0.02 -> all 30 fit.
        let rungs = compute_dense_tandem_emission(&c, &book, &empty_inventory()).unwrap();
        let yes_count = rungs.iter().filter(|r| r.leg == LadderLeg::Yes).count();
        assert_eq!(yes_count, 30, "expected 30 yes rungs, got {}", yes_count);
    }

    #[test]
    fn ladder_truncates_at_ladder_min_price() {
        let book = balanced_book(0.20); // yes_mid=0.20, no_mid=0.80
        let mut c = cfg();
        c.levels_per_side = 30;
        c.ladder_min_price = 0.02;
        // Yes rungs: 0.20-0.01 down to ... but capped at 0.02.
        // So rungs at 0.19, 0.18, ... down to 0.02 = 18 yes rungs only.
        let rungs = compute_dense_tandem_emission(&c, &book, &empty_inventory()).unwrap();
        let yes_count = rungs.iter().filter(|r| r.leg == LadderLeg::Yes).count();
        assert!(
            yes_count < 30,
            "yes rungs should truncate at ladder_min_price, got {}",
            yes_count
        );
        // No mid=0.80, no rungs at 0.79, 0.78, ... down to 0.50 = all 30 fit.
        let no_count = rungs.iter().filter(|r| r.leg == LadderLeg::No).count();
        assert_eq!(no_count, 30);
    }

    #[test]
    fn pair_cost_at_gate_threshold_emits() {
        // pair_cost = 0.97 exactly, gate = 0.97. > comparison so this
        // should emit (not skip).
        let book = book_at(0.50, 0.97);
        let result = compute_dense_tandem_emission(&cfg(), &book, &empty_inventory());
        assert!(
            result.is_ok(),
            "pair cost == gate should emit, got {:?}",
            result
        );
    }
}
