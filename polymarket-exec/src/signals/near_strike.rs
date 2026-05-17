//! Near-strike fragility detection for short-dated binary BTC bars.
//!
//! This does not try to predict the final tick. It prevents a high-confidence
//! model probability from becoming a high-dollar load when the current BTC
//! price is still inside the noise band around the strike.

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct NearStrikeFragility {
    pub in_fragile_zone: bool,
    pub fragility_factor: f64,
    pub reason: &'static str,
}

impl Default for NearStrikeFragility {
    fn default() -> Self {
        Self {
            in_fragile_zone: false,
            fragility_factor: 1.0,
            reason: "outside_band",
        }
    }
}

impl NearStrikeFragility {
    pub fn blocks_entry(self) -> bool {
        self.in_fragile_zone && self.fragility_factor < 0.20
    }
}

pub fn compute_near_strike_fragility(
    spot_vs_strike_bps: Option<f64>,
    time_remaining_s: f64,
    favorite_ask: f64,
    flow_score: f64,
) -> NearStrikeFragility {
    let Some(distance_bps) = spot_vs_strike_bps.filter(|v| v.is_finite()).map(f64::abs) else {
        return NearStrikeFragility::default();
    };
    let time_remaining_s = time_remaining_s.max(0.0);
    let band_bps = if time_remaining_s <= 60.0 {
        1.0
    } else if time_remaining_s <= 120.0 {
        2.0
    } else {
        3.5
    };

    if distance_bps > band_bps {
        return NearStrikeFragility::default();
    }

    let mut factor: f64 = if favorite_ask >= 0.90 {
        0.25
    } else if favorite_ask >= 0.75 {
        0.55
    } else {
        0.15
    };

    if flow_score < -0.30 {
        factor *= 0.10;
    }

    NearStrikeFragility {
        in_fragile_zone: true,
        fragility_factor: factor.clamp(0.0, 1.0),
        reason: if factor < 0.20 {
            "fragile_block"
        } else {
            "fragile_shrink"
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn outside_band_has_no_penalty() {
        let fragility = compute_near_strike_fragility(Some(3.0), 45.0, 0.92, 0.0);

        assert!(!fragility.in_fragile_zone);
        assert_eq!(fragility.fragility_factor, 1.0);
    }

    #[test]
    fn high_cert_favorite_near_strike_is_shrunk_not_blocked() {
        let fragility = compute_near_strike_fragility(Some(0.5), 45.0, 0.93, 0.0);

        assert!(fragility.in_fragile_zone);
        assert!(!fragility.blocks_entry());
        assert_eq!(fragility.fragility_factor, 0.25);
    }

    #[test]
    fn sub_seventy_five_favorite_near_strike_is_blocked() {
        let fragility = compute_near_strike_fragility(Some(0.5), 45.0, 0.72, 0.0);

        assert!(fragility.in_fragile_zone);
        assert!(fragility.blocks_entry());
        assert_eq!(fragility.reason, "fragile_block");
    }

    #[test]
    fn adverse_flow_hard_blocks_fragile_zone() {
        let fragility = compute_near_strike_fragility(Some(0.5), 45.0, 0.93, -0.40);

        assert!(fragility.in_fragile_zone);
        assert!(fragility.blocks_entry());
        assert!(fragility.fragility_factor < 0.05);
    }
}
