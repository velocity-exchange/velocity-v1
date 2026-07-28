//! One file per instruction: context struct at the top, handler below.

pub mod cancel_order_v0;
pub mod evict_worst_v0;
pub mod execute_v0;
pub mod initialize_market_v0;
pub mod place_order_v0;
pub mod quote_v0;
pub mod remove_expired_v0;
pub mod resize_market_v0;
pub mod update_market_v0;

pub use cancel_order_v0::*;
pub use evict_worst_v0::*;
pub use execute_v0::*;
pub use initialize_market_v0::*;
pub use place_order_v0::*;
pub use quote_v0::*;
pub use remove_expired_v0::*;
pub use resize_market_v0::*;
pub use update_market_v0::*;
