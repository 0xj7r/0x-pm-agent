//! External I/O namespace for REST, websockets, spot feed, and execution adapters.

pub mod api;
pub mod clob_v2;
pub mod eoa_polygon;
pub mod execution_adapter;
mod execution_types;
pub mod incentives_api;
pub mod market_ws;
pub mod polygon_rpc;
pub mod raw_frame;
mod raw_trades;
pub mod relayer;
pub mod spot_ws;
pub mod user_ws;

pub use raw_frame::RawFrame;
