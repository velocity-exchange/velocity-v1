//! Velocity CLOB quoter program (Anchor v2 / anchor-next). Price-time-priority
//! resting book, exposed to velocity through the quoter interface: `quote_v0`/
//! `execute_v0` write borsh responses into the market's response-buffer PDA
//! and return a `ResponsePointerV0` via return data.
//!
//! Placement policy lives in velocity: `place_authority` (the velocity signer
//! PDA) is the only signer allowed to place/cancel/execute — velocity verifies
//! `User` authority and margin/flow policy before CPI'ing here. `quote_v0` is
//! read-only and ungated.
//!
//! Discriminators stay 8-byte anchor defaults (not `#[discrim]` single-byte):
//! the velocity quoter registry stores `[u8; 8]` discriminators.

use anchor_lang_v2::prelude::*;

pub mod error;
pub mod events;
pub mod instructions;
pub mod state;

// Re-exported so integration tests can reach wincode/BORSH_CONFIG through the
// crate without their own git dep.
pub use anchor_lang_v2;
pub use instructions::*;

declare_id!("BPX47ur8TbgZQgtJcGJvdcQMMFbmBP7ZrhpiUmLuHKqU");

#[program]
pub mod clob {
    use super::*;

    pub fn initialize_market_v0(
        ctx: &mut Context<InitializeMarketV0>,
        config: state::MarketConfigV0,
    ) -> Result<()> {
        instructions::initialize_market_v0::handle_initialize_market_v0(ctx, config)
    }

    pub fn update_market_v0(
        ctx: &mut Context<UpdateMarketV0>,
        args: UpdateMarketArgsV0,
    ) -> Result<()> {
        instructions::update_market_v0::handle_update_market_v0(ctx, args)
    }

    pub fn place_order_v0(
        ctx: &mut Context<PlaceOrderV0>,
        args: PlaceOrderArgsV0,
    ) -> Result<state::OrderRefV0> {
        instructions::place_order_v0::handle_place_order_v0(ctx, args)
    }

    pub fn cancel_order_v0(
        ctx: &mut Context<CancelOrderV0>,
        args: CancelOrderArgsV0,
    ) -> Result<state::RemovedOrderV0> {
        instructions::cancel_order_v0::handle_cancel_order_v0(ctx, args)
    }

    pub fn evict_worst_v0(
        ctx: &mut Context<EvictWorstV0>,
        args: EvictWorstArgsV0,
    ) -> Result<state::RemovedOrderV0> {
        instructions::evict_worst_v0::handle_evict_worst_v0(ctx, args)
    }

    pub fn remove_expired_v0(
        ctx: &mut Context<RemoveExpiredV0>,
        args: RemoveExpiredArgsV0,
    ) -> Result<state::RemovedOrderV0> {
        instructions::remove_expired_v0::handle_remove_expired_v0(ctx, args)
    }

    pub fn resize_market_v0(
        ctx: &mut Context<ResizeMarketV0>,
        args: ResizeMarketArgsV0,
    ) -> Result<()> {
        instructions::resize_market_v0::handle_resize_market_v0(ctx, args)
    }

    pub fn quote_v0(
        ctx: &mut Context<QuoteV0>,
        args: QuoteArgsV0,
    ) -> Result<state::ResponsePointerV0> {
        instructions::quote_v0::handle_quote_v0(ctx, args)
    }

    pub fn execute_v0(
        ctx: &mut Context<ExecuteV0>,
        args: ExecuteArgsV0,
    ) -> Result<state::ResponsePointerV0> {
        instructions::execute_v0::handle_execute_v0(ctx, args)
    }
}
