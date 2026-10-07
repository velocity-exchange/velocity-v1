//! Admin surface of the CLOB attachment: one file per instruction.

pub mod resize_perp_market_clob_book;
pub mod update_perp_market_clob_book_config;
pub mod update_perp_market_clob_quoter;

pub use {
    resize_perp_market_clob_book::*, update_perp_market_clob_book_config::*,
    update_perp_market_clob_quoter::*,
};
