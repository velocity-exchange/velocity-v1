//! One file per instruction: context struct at the top, handler below.

pub mod execute_v0;
pub mod initialize_quoter_v0;
pub mod quote_v0;
pub mod set_levels_v0;
pub mod set_mid_v0;
pub mod update_quoter_v0;

pub use {
    execute_v0::*, initialize_quoter_v0::*, quote_v0::*, set_levels_v0::*, set_mid_v0::*,
    update_quoter_v0::*,
};
