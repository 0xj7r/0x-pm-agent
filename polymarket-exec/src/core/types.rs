//! Shared domain types for orders, fills, quotes, and runtime commands.

use std::fmt;

use serde::{Deserialize, Serialize};

pub type EpochMillis = u64;

macro_rules! id_type {
    ($name:ident) => {
        #[derive(
            Clone, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize,
        )]
        pub struct $name(String);

        impl $name {
            pub fn new(value: impl Into<String>) -> Self {
                Self(value.into())
            }

            pub fn as_str(&self) -> &str {
                &self.0
            }

            pub fn is_empty(&self) -> bool {
                self.0.is_empty()
            }
        }

        impl From<&str> for $name {
            fn from(value: &str) -> Self {
                Self::new(value)
            }
        }

        impl From<String> for $name {
            fn from(value: String) -> Self {
                Self::new(value)
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                self.0.fmt(f)
            }
        }
    };
}

id_type!(MarketId);
id_type!(InstrumentId);
id_type!(ClientOrderId);
id_type!(OrderId);

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum RuntimeStatus {
    #[default]
    Starting,
    Running,
    RiskOff,
    Degraded,
    Stopped,
}

/// Per-market execution state derived by the runtime from orders, inventory,
/// venue reconciliation, and merge lifecycle state.
///
/// Strategies use this to distinguish fresh entry from close-side repair. The
/// runtime remains the authority for accepting or rejecting resulting intents.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum MarketLedgerState {
    #[default]
    Flat,
    QuotingPaired,
    PartiallyFilled,
    Recycling,
    MergePending,
    Drifted,
    Resolved,
}

impl MarketLedgerState {
    pub fn allows_fresh_entry(self) -> bool {
        matches!(
            self,
            Self::Flat | Self::QuotingPaired | Self::Recycling | Self::MergePending | Self::Drifted
        )
    }

    pub fn allows_close_side(self) -> bool {
        matches!(
            self,
            Self::PartiallyFilled | Self::Recycling | Self::MergePending | Self::Drifted
        )
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum TradeSide {
    Buy,
    Sell,
}

impl TradeSide {
    pub fn sign(self) -> f64 {
        match self {
            Self::Buy => 1.0,
            Self::Sell => -1.0,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum FillLiquidity {
    Maker,
    Taker,
    #[default]
    Unknown,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
pub enum CloseMethod {
    #[default]
    Unknown,
    Merge,
    Redeem,
    Sell,
    Settle,
    Settlement,
}

impl CloseMethod {
    pub fn from_raw(value: &str) -> Self {
        match value.to_ascii_lowercase().as_str() {
            "merge" | "mergepositions" => Self::Merge,
            "redeem" | "redeempositions" => Self::Redeem,
            "sell" => Self::Sell,
            "settle" | "settlement" => Self::Settlement,
            _ => Self::Unknown,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Unknown => "unknown",
            Self::Merge => "merge",
            Self::Redeem => "redeem",
            Self::Sell => "sell",
            Self::Settle | Self::Settlement => "settlement",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct BookLevel {
    pub price: f64,
    pub quantity: f64,
}

impl BookLevel {
    pub fn new(price: f64, quantity: f64) -> Self {
        Self { price, quantity }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct QuoteSnapshot {
    pub best_bid: Option<BookLevel>,
    pub best_ask: Option<BookLevel>,
    pub bid_levels: Vec<BookLevel>,
    pub ask_levels: Vec<BookLevel>,
    pub depth_observed_at_ms: Option<EpochMillis>,
    pub last_trade_price: Option<f64>,
    pub taker_buy_qty_60s: f64,
    pub taker_sell_qty_60s: f64,
    pub observed_at_ms: EpochMillis,
}

impl QuoteSnapshot {
    pub fn mid_price(&self) -> Option<f64> {
        match (&self.best_bid, &self.best_ask) {
            (Some(bid), Some(ask)) => Some((bid.price + ask.price) * 0.5),
            _ => self.last_trade_price,
        }
    }

    pub fn spread(&self) -> Option<f64> {
        match (&self.best_bid, &self.best_ask) {
            (Some(bid), Some(ask)) => Some(ask.price - bid.price),
            _ => None,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct MarketSnapshot {
    pub market_id: MarketId,
    pub instrument_id: InstrumentId,
    pub quote: QuoteSnapshot,
}

impl MarketSnapshot {
    pub fn mark_price(&self) -> Option<f64> {
        self.quote
            .mid_price()
            .or(self.quote.best_ask.as_ref().map(|level| level.price))
            .or(self.quote.best_bid.as_ref().map(|level| level.price))
    }
}

/// Whether an order intent ADDS exposure (Entry) or REMOVES it (Close).
///
/// Per CLAUDE.md "Distinguishing entry vs close intents": entry-time caps
/// (max_open_orders, max_leg_cost, max_gross_cost, max_submit_per_window,
/// drift block) prevent accumulation runaway. Close intents must NOT be
/// trapped in those caps — blocking a close leaves us stuck with the
/// exact directional exposure the cap was meant to prevent.
///
/// This was previously expressed as 5 separate `quote_level_tag.starts_with("mm-hedge-rescue")`
/// string checks across runtime/mod.rs, core/risk.rs, market_making/quote_reconciler.rs,
/// strategy.rs, and accept_intent's drift block. Promoted to a typed
/// enum so any future gate someone adds doesn't silently re-trap rescues.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
pub enum IntentKind {
    /// Adds exposure: paired-bid maker entries, single-leg accumulations.
    /// Subject to all entry-time caps.
    #[default]
    Entry,
    /// Removes exposure: hedge rescue (FAK lift opposite leg for merge),
    /// reduce-only sells. Bypasses entry-time caps because the goal is
    /// to UNWIND the exposure, not add to it.
    Close,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct OrderIntent {
    pub client_order_id: ClientOrderId,
    pub market_id: MarketId,
    pub instrument_id: InstrumentId,
    pub side: TradeSide,
    pub limit_price: f64,
    pub quantity: f64,
    pub reduce_only: bool,
    pub reason: String,
    pub quote_level_tag: Option<String>,
    pub created_at_ms: EpochMillis,
    /// Optional shared identifier across legs that must succeed/fail together.
    /// When one leg of a pair gets rejected by the venue, the runtime cancels
    /// the mate to prevent naked exposure (handoff incident #4 guard).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pair_id: Option<String>,
    /// Entry vs close classification. Drives gate behavior — close intents
    /// bypass entry-time caps. Defaults to Entry for backwards compat.
    #[serde(default)]
    pub kind: IntentKind,
}

impl OrderIntent {
    pub fn new_buy(
        client_order_id: ClientOrderId,
        market_id: MarketId,
        instrument_id: InstrumentId,
        limit_price: f64,
        quantity: f64,
        reason: impl Into<String>,
        created_at_ms: EpochMillis,
    ) -> Self {
        Self {
            client_order_id,
            market_id,
            instrument_id,
            side: TradeSide::Buy,
            limit_price,
            quantity,
            reduce_only: false,
            reason: reason.into(),
            quote_level_tag: None,
            created_at_ms,
            pair_id: None,
            kind: IntentKind::Entry,
        }
    }

    pub fn new_sell(
        client_order_id: ClientOrderId,
        market_id: MarketId,
        instrument_id: InstrumentId,
        limit_price: f64,
        quantity: f64,
        reason: impl Into<String>,
        created_at_ms: EpochMillis,
    ) -> Self {
        Self {
            client_order_id,
            market_id,
            instrument_id,
            side: TradeSide::Sell,
            limit_price,
            quantity,
            reduce_only: true,
            reason: reason.into(),
            quote_level_tag: None,
            created_at_ms,
            pair_id: None,
            kind: IntentKind::Close,
        }
    }

    pub fn notional_usd(&self) -> f64 {
        self.limit_price * self.quantity
    }

    pub fn signed_notional_usd(&self) -> f64 {
        self.notional_usd() * self.side.sign()
    }
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct MergeIntent {
    pub command_id: ClientOrderId,
    pub market_id: MarketId,
    pub condition_id: Option<String>,
    pub yes_instrument_id: InstrumentId,
    pub no_instrument_id: InstrumentId,
    pub quantity: f64,
    pub expected_cash_usd: f64,
    pub expected_cost_usd: f64,
    pub expected_fee_usd: f64,
    pub expected_gas_usd: f64,
    pub reason: String,
    pub created_at_ms: EpochMillis,
}

impl MergeIntent {
    pub fn expected_net_gain_usd(&self) -> f64 {
        self.expected_cash_usd
            - self.expected_cost_usd
            - self.expected_fee_usd
            - self.expected_gas_usd
    }
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct RedeemIntent {
    pub command_id: ClientOrderId,
    pub market_id: MarketId,
    pub condition_id: Option<String>,
    pub reason: String,
    pub created_at_ms: EpochMillis,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct FillReport {
    pub order_id: Option<OrderId>,
    pub client_order_id: Option<ClientOrderId>,
    pub market_id: MarketId,
    pub instrument_id: InstrumentId,
    pub side: TradeSide,
    pub price: f64,
    pub quantity: f64,
    pub fee_usd: f64,
    pub liquidity: FillLiquidity,
    pub close_method: Option<CloseMethod>,
    pub observed_at_ms: EpochMillis,
}

impl FillReport {
    pub fn notional_usd(&self) -> f64 {
        self.price * self.quantity
    }
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub enum RuntimeCommand {
    Submit(OrderIntent),
    Cancel {
        client_order_id: ClientOrderId,
        reason: String,
    },
    Merge(MergeIntent),
    Redeem(RedeemIntent),
    Noop,
}

/// Market-making quote kind used for typed reporting and risk attribution.
///
/// This replaces string-prefix classification such as
/// `mm-paired-bid:l1` / `mm-hedge-rescue` at new seams. Existing tags can
/// still carry the human-readable venue/debug label, but decisions should
/// pass through typed variants first.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum MmQuoteKind {
    PairedEntry,
    CapitalRecycle,
    ConvexAccumulation,
    HedgeRescue,
    ReduceOnlyExit,
    LateBarCore,
}

impl MmQuoteKind {
    pub fn from_quote_level_tag(tag: &str) -> Option<Self> {
        let tag = tag.to_ascii_lowercase();
        if tag.contains("convex") || tag.contains("cheap-tail") || tag.contains("reversal-hedge") {
            Some(Self::ConvexAccumulation)
        } else if tag.contains("capital-recycle") || tag.contains("buy-light") {
            Some(Self::CapitalRecycle)
        } else if tag.contains("mm-paired-bid")
            || tag.contains("paired-mm")
            || tag.contains("paired-core")
        {
            Some(Self::PairedEntry)
        } else if tag.contains("hedge-rescue") || tag.contains("rescue") {
            Some(Self::HedgeRescue)
        } else if tag.contains("sell-unwind") || tag.contains("reduce") {
            Some(Self::ReduceOnlyExit)
        } else if tag.contains("late-bar-core") {
            Some(Self::LateBarCore)
        } else {
            None
        }
    }

    /// Stable attribution bucket used by runtime metrics and paper reports.
    pub fn attribution_bucket(self) -> &'static str {
        match self {
            Self::PairedEntry => "paired_ladder",
            Self::CapitalRecycle => "capital_recycle",
            Self::ConvexAccumulation => "convex_accum",
            Self::HedgeRescue => "hedge_rescue",
            Self::ReduceOnlyExit => "reduce_cleanup",
            Self::LateBarCore => "other_submit",
        }
    }
}

pub fn classify_quote_level_tag_for_attribution(tag: &str) -> &'static str {
    if tag.is_empty() {
        return "untagged_submit";
    }
    MmQuoteKind::from_quote_level_tag(tag)
        .map(MmQuoteKind::attribution_bucket)
        .unwrap_or("other_submit")
}

/// Scope of a strategy suppression.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum SuppressionScope {
    /// Skip paired-entry quotes only. Convex accumulation / close-side rescue
    /// may still be valid.
    PairedOnly,
    /// Skip all entry paths. Close-side rescue and merge commands still pass
    /// through the risk boundary.
    AllEntry,
    /// Runtime is degraded enough that no new venue action should be emitted
    /// except explicit cancel/flatten logic owned by runtime.
    AllActions,
}

/// Typed cooling / suppression reasons for strategy decisions.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum CoolingReason {
    RuntimeDegraded,
    BtcRegimeInactive,
    AsymmetricFillCooldown,
    PostFillCooldown,
    PremiumFairCap,
    MarketMidMoved,
    BtcTrending,
    GrossCostCap,
    SideImbalanceCap,
    EndOfBar,
    NoSignal(String),
    Other(String),
}

impl CoolingReason {
    pub fn scope(&self) -> SuppressionScope {
        match self {
            Self::RuntimeDegraded
            | Self::BtcRegimeInactive
            | Self::AsymmetricFillCooldown
            | Self::PostFillCooldown
            | Self::GrossCostCap
            | Self::SideImbalanceCap
            | Self::EndOfBar => SuppressionScope::AllEntry,
            Self::PremiumFairCap | Self::MarketMidMoved | Self::BtcTrending => {
                SuppressionScope::PairedOnly
            }
            Self::NoSignal(_) | Self::Other(_) => SuppressionScope::AllEntry,
        }
    }
}

/// Typed strategy output for the new paired-MM seam.
///
/// The legacy crate still has `strategy::StrategyDecision`; this type is the
/// target interface for the refactored engine. Strategy modules propose typed
/// decisions; runtime/risk adapters approve, reject, or translate them into
/// `RuntimeCommand`s.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub enum StrategyDecision {
    QuoteSet {
        intents: Vec<OrderIntent>,
        notes: Vec<String>,
    },
    CapitalRecycle {
        intents: Vec<OrderIntent>,
        notes: Vec<String>,
    },
    Rescue {
        intents: Vec<OrderIntent>,
        notes: Vec<String>,
    },
    Merge {
        intent: MergeIntent,
        notes: Vec<String>,
    },
    Mixed {
        intents: Vec<OrderIntent>,
        commands: Vec<RuntimeCommand>,
        notes: Vec<String>,
    },
    Suppress {
        scope: SuppressionScope,
        reason: CoolingReason,
        preserve_quotes: bool,
        notes: Vec<String>,
    },
    Noop {
        notes: Vec<String>,
    },
}

impl StrategyDecision {
    pub fn quote_set(intents: Vec<OrderIntent>, notes: Vec<String>) -> Self {
        Self::QuoteSet { intents, notes }
    }

    pub fn rescue(intents: Vec<OrderIntent>, notes: Vec<String>) -> Self {
        Self::Rescue { intents, notes }
    }

    pub fn capital_recycle(intents: Vec<OrderIntent>, notes: Vec<String>) -> Self {
        Self::CapitalRecycle { intents, notes }
    }

    pub fn suppress(reason: CoolingReason, preserve_quotes: bool, notes: Vec<String>) -> Self {
        let scope = reason.scope();
        Self::Suppress {
            scope,
            reason,
            preserve_quotes,
            notes,
        }
    }

    pub fn noop() -> Self {
        Self::Noop { notes: Vec::new() }
    }

    pub fn intents(&self) -> &[OrderIntent] {
        match self {
            Self::QuoteSet { intents, .. }
            | Self::CapitalRecycle { intents, .. }
            | Self::Rescue { intents, .. }
            | Self::Mixed { intents, .. } => intents,
            Self::Merge { .. } | Self::Suppress { .. } | Self::Noop { .. } => &[],
        }
    }

    pub fn into_runtime_commands(self) -> Vec<RuntimeCommand> {
        match self {
            Self::QuoteSet { intents, .. }
            | Self::CapitalRecycle { intents, .. }
            | Self::Rescue { intents, .. } => {
                intents.into_iter().map(RuntimeCommand::Submit).collect()
            }
            Self::Merge { intent, .. } => vec![RuntimeCommand::Merge(intent)],
            Self::Mixed {
                intents, commands, ..
            } => commands
                .into_iter()
                .chain(intents.into_iter().map(RuntimeCommand::Submit))
                .collect(),
            Self::Suppress { .. } | Self::Noop { .. } => Vec::new(),
        }
    }
}
