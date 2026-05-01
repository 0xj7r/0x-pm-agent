//! New extensible strategy interface.
//!
//! The legacy `crate::strategy` module remains live while we migrate. New
//! market-agnostic implementations should use this module: market descriptors
//! define what is traded, strategy adapters define how it is traded.

pub mod paired_mm;
pub mod registry;
pub mod traits;

pub use paired_mm::{PairedMmStrategy, PairedMmStrategyConfig};
pub use registry::{RegisteredStrategy, StrategyKey, StrategyRegistry};
pub use traits::{StrategyFillInput, StrategyInput, TradingStrategy};
