//! Velocity CLOB quoter program, built on Anchor v2. The book rests orders in
//! price-time priority. Velocity reaches it through the quoter interface.
//! `quote_v0` and `execute_v0` write wincode responses into the market's
//! response-buffer PDA and return a `ResponsePointerV0` as return data.
//!
//! Placement policy lives in velocity. `place_authority` is velocity's quoter
//! CPI signer PDA, and the only signer that can place, cancel or execute.
//! Velocity checks `User` authority, margin and flow policy before it calls
//! here. `execute_v0` shares that gate because it consumes resting orders and
//! creates no positions. Only velocity can settle the fill it returns.
//! `quote_v0` is read-only and ungated.
//!
//! Discriminators stay at the 8-byte anchor default rather than the
//! single-byte `#[discrim]` form. The velocity quoter registry stores
//! `[u8; 8]` discriminators.
//!
//! Module layout: [`state`] holds the account layout and the wire types.
//! [`book`] holds the order-book algorithm over them, and the streaming
//! encoder that writes quote and execute payloads into the market's response
//! region. [`config`] holds the bounds every market config must meet.
//! [`emit`] holds the event log path that allocates nothing.
//! [`instructions`] holds one file per instruction.

use anchor_lang::prelude::*;

pub mod book;
pub mod config;
pub mod emit;
pub mod error;
pub mod events;
pub mod instructions;
pub mod state;

#[cfg(test)]
mod tests;

// Integration tests reach wincode and BORSH_CONFIG through this crate, so they
// need no git dependency of their own.
pub use {anchor_lang, instructions::*, relay_spec};

declare_id!("BPX47ur8TbgZQgtJcGJvdcQMMFbmBP7ZrhpiUmLuHKqU");

// Velocity refuses a quoter config whose program is not this key, and reads the
// key from `clob-wire`. A divergence here would brick every CLOB path at
// runtime, so it fails the build instead.
const _: () = assert!(clob_wire::is_clob_program_id(ID.to_bytes()));

#[program]
pub mod clob {
    use super::*;

    pub fn initialize_market_v0(
        ctx: &mut Context<InitializeMarketV0>,
        config: state::MarketConfigV0,
    ) -> Result<()> {
        instructions::initialize_market_v0::handle_initialize_market_v0(ctx, config)
    }

    /// Close an empty market and return its rent to `rent_recipient`.
    pub fn close_market_v0(ctx: &mut Context<CloseMarketV0>) -> Result<()> {
        instructions::close_market_v0::handle_close_market_v0(ctx)
    }

    pub fn update_market_v0(
        ctx: &mut Context<UpdateMarketV0>,
        args: UpdateMarketArgsV0,
    ) -> Result<()> {
        instructions::update_market_v0::handle_update_market_v0(ctx, args)
    }

    /// Name the key that may take over this market's config. It takes over
    /// only when it signs `accept_market_authority_v0`.
    pub fn propose_market_authority_v0(
        ctx: &mut Context<ProposeMarketAuthorityV0>,
        args: ProposeMarketAuthorityArgsV0,
    ) -> Result<()> {
        instructions::propose_market_authority_v0::handle_propose_market_authority_v0(ctx, args)
    }

    pub fn accept_market_authority_v0(ctx: &mut Context<AcceptMarketAuthorityV0>) -> Result<()> {
        instructions::accept_market_authority_v0::handle_accept_market_authority_v0(ctx)
    }

    pub fn place_order_v0(
        ctx: &mut Context<GatedMarketV0>,
        args: PlaceOrderArgsV0,
    ) -> Result<state::OrderRefV0> {
        instructions::place_order_v0::handle_place_order_v0(ctx, args)
    }

    pub fn cancel_order_v0(
        ctx: &mut Context<GatedMarketV0>,
        args: CancelOrderArgsV0,
    ) -> Result<state::RemovedOrderV0> {
        instructions::cancel_order_v0::handle_cancel_order_v0(ctx, args)
    }

    pub fn cancel_all_v0(
        ctx: &mut Context<GatedMarketV0>,
        args: CancelAllArgsV0,
    ) -> Result<state::CancelAllOutcomeV0> {
        instructions::cancel_all_v0::handle_cancel_all_v0(ctx, args)
    }

    pub fn evict_worst_v0(
        ctx: &mut Context<GatedMarketV0>,
        args: EvictWorstArgsV0,
    ) -> Result<state::RemovedOrderV0> {
        instructions::evict_worst_v0::handle_evict_worst_v0(ctx, args)
    }

    pub fn remove_expired_v0(
        ctx: &mut Context<GatedMarketV0>,
        args: RemoveExpiredArgsV0,
    ) -> Result<state::RemovedOrderV0> {
        instructions::remove_expired_v0::handle_remove_expired_v0(ctx, args)
    }

    /// Read-only. Names the order the book lets a caller remove next. A caller
    /// simulates it to find work, then sends the removal it names.
    pub fn next_removal_v0(
        ctx: &mut Context<MarketViewV0>,
        args: NextRemovalArgsV0,
    ) -> Result<OrderViewV0> {
        instructions::next_removal_v0::handle_next_removal_v0(ctx, args)
    }

    /// Read-only. Reports what the book requires of an order before it holds
    /// one. A caller builds against these rules instead of learning them from
    /// a rejection.
    pub fn order_rules_v0(ctx: &mut Context<MarketViewV0>) -> Result<OrderRulesV0> {
        instructions::order_rules_v0::handle_order_rules_v0(ctx)
    }

    /// Read-only. Reports what the book holds for a set of refs, one answer
    /// per ref. A ref that no longer names a live order comes back as
    /// `OrderViewV0::NONE`.
    pub fn orders_v0(ctx: &mut Context<MarketViewV0>, args: OrdersArgsV0) -> Result<OrdersV0> {
        instructions::orders_v0::handle_orders_v0(ctx, args)
    }

    /// Read-only. Reports the best matchable order on each side. Any cross
    /// settles between those two orders. A caller simulates it, like
    /// `next_removal_v0`.
    pub fn next_cross_v0(ctx: &mut Context<MarketViewV0>) -> Result<NextCrossV0> {
        instructions::next_cross_v0::handle_next_cross_v0(ctx)
    }

    /// Register the resolvers for this book's own crank conditions. The book
    /// states when a turner must wake. The program that owns the flow states
    /// what to do. Returns the account offset the condition block sits at, for
    /// the watch registration.
    pub fn set_crank_conditions_v0(
        ctx: &mut Context<GatedMarketV0>,
        args: CrankConditionsArgsV0,
    ) -> Result<CrankBlockV0> {
        instructions::set_crank_conditions_v0::handle_set_crank_conditions_v0(ctx, args)
    }

    pub fn resize_market_v0(
        ctx: &mut Context<ResizeMarketV0>,
        args: ResizeMarketArgsV0,
    ) -> Result<()> {
        instructions::resize_market_v0::handle_resize_market_v0(ctx, args)
    }

    pub fn quote_v0(
        ctx: &mut Context<ResponseMarketV0>,
        args: QuoteArgsV0<'_>,
    ) -> Result<state::ResponsePointerV0> {
        instructions::quote_v0::handle_quote_v0(ctx, args)
    }

    pub fn quote_l3_v0(
        ctx: &mut Context<ResponseMarketV0>,
        args: L3ArgsV0,
    ) -> Result<state::ResponsePointerV0> {
        instructions::quote_l3_v0::handle_quote_l3_v0(ctx, args)
    }

    pub fn execute_v0(
        ctx: &mut Context<GatedMarketV0>,
        args: ExecuteArgsV0<'_>,
    ) -> Result<state::ResponsePointerV0> {
        instructions::execute_v0::handle_execute_v0(ctx, args)
    }

    pub fn fill_v0(
        ctx: &mut Context<GatedMarketV0>,
        args: FillArgsV0,
    ) -> Result<state::FillOutcomeV0> {
        instructions::fill_v0::handle_fill_v0(ctx, args)
    }
}
