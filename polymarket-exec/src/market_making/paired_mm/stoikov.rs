//! Stoikov-style inventory skew for short-duration binary markets.

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct StoikovParams {
    pub gamma: f64,
    pub k: f64,
    pub max_skew: f64,
}

impl Default for StoikovParams {
    fn default() -> Self {
        Self {
            gamma: 0.20,
            k: 1.0,
            max_skew: 0.20,
        }
    }
}

/// Inventory-aware reservation price.
///
/// `fair` is a binary probability / price in `[0, 1]`.
/// `inventory_qty` is positive when we already own the leg. Positive inventory
/// lowers the next bid for that leg; negative inventory raises it.
/// `vol_bps` is realized volatility over the relevant bar.
/// `tau_fraction` is time remaining as fraction of the market window.
pub fn stoikov_reservation_price(
    fair: f64,
    inventory_qty: f64,
    params: StoikovParams,
    tau_fraction: f64,
    vol_bps: f64,
) -> f64 {
    if !fair.is_finite() {
        return 0.5;
    }

    let gamma = params.gamma.max(1e-9);
    let k = params.k.max(1e-9);
    let sigma = (vol_bps.max(0.0) / 10_000.0).max(0.0);
    let tau = tau_fraction.clamp(0.0, 1.0);

    let inventory_penalty = 0.5 * gamma * inventory_qty;
    let liquidity_penalty = (1.0 / gamma) * (1.0 + gamma / k).ln() * sigma * sigma * tau;
    let skew = (inventory_penalty + liquidity_penalty)
        .clamp(-params.max_skew.abs(), params.max_skew.abs());

    (fair - skew).clamp(0.01, 0.99)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn positive_inventory_lowers_reservation_price() {
        let params = StoikovParams {
            gamma: 0.01,
            k: 1.0,
            max_skew: 0.20,
        };
        let flat = stoikov_reservation_price(0.50, 0.0, params, 0.5, 10.0);
        let long = stoikov_reservation_price(0.50, 10.0, params, 0.5, 10.0);
        assert!(long < flat);
    }
}
