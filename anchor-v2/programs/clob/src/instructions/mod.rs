//! One file per instruction: context struct at the top, handler below.

pub mod cancel_all_v0;
pub mod cancel_order_v0;
pub mod evict_worst_v0;
pub mod execute_v0;
pub mod initialize_market_v0;
pub mod next_cross_v0;
pub mod next_removal_v0;
pub mod order_rules_v0;
pub mod orders_v0;
pub mod place_order_v0;
pub mod quote_l3_v0;
pub mod quote_v0;
pub mod remove_expired_v0;
pub mod resize_market_v0;
pub mod set_crank_conditions_v0;
pub mod update_market_v0;

pub use {
    cancel_all_v0::*, cancel_order_v0::*, evict_worst_v0::*, execute_v0::*,
    initialize_market_v0::*, next_cross_v0::*, next_removal_v0::*, order_rules_v0::*, orders_v0::*,
    place_order_v0::*, quote_l3_v0::*, quote_v0::*, remove_expired_v0::*, resize_market_v0::*,
    set_crank_conditions_v0::*, update_market_v0::*,
};
