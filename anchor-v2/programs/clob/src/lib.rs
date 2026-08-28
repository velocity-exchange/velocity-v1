//! Velocity CLOB quoter program (Anchor v2 / anchor-next). Price-time-priority
//! resting book, exposed to velocity through the quoter interface: `quote_v0`/
//! `execute_v0` write wincode responses into the market's response-buffer PDA
//! and return a `ResponsePointerV0` via return data.
//!
//! Placement policy lives in velocity: `place_authority` (velocity's quoter
//! CPI signer PDA) is the only signer allowed to place/cancel/execute —
//! velocity verifies `User` authority and margin/flow policy before CPI'ing
//! here. `execute_v0` shares that gate because it consumes resting orders
//! while creating no positions: only velocity can settle the fill it returns.
//! `quote_v0` is read-only and ungated.
//!
//! Discriminators stay 8-byte anchor defaults (not `#[discrim]` single-byte):
//! the velocity quoter registry stores `[u8; 8]` discriminators.
//!
//! Module layout: [`state`] is the account layout and wire types, [`book`]
//! the order-book algorithm over them (arena access, traversal, invariants)
//! plus the streaming encoder that writes quote/execute payloads straight
//! into the market's response region, [`emit`] the allocation-free event log
//! path, and [`instructions`] one file per instruction.

use anchor_lang_v2::prelude::*;

pub mod book;
pub mod emit;
pub mod error;
pub mod events;
pub mod instructions;
pub mod state;

#[cfg(test)]
mod tests;

// Re-exported so integration tests can reach wincode/BORSH_CONFIG through the
// crate without their own git dep.
pub use {anchor_lang_v2, instructions::*, relay_spec};

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

    pub fn cancel_all_v0(
        ctx: &mut Context<CancelAllV0>,
        args: CancelAllArgsV0,
    ) -> Result<state::CancelAllOutcomeV0> {
        instructions::cancel_all_v0::handle_cancel_all_v0(ctx, args)
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

    /// Read-only: which order the book would let a caller remove next. Meant
    /// to be simulated — a caller runs it to find work, then sends the
    /// removal it names.
    pub fn next_removal_v0(
        ctx: &mut Context<NextRemovalV0Accounts>,
        args: NextRemovalArgsV0,
    ) -> Result<OrderViewV0> {
        instructions::next_removal_v0::handle_next_removal_v0(ctx, args)
    }

    /// Read-only: what the book requires of an order before it will hold
    /// one — a caller builds against these rather than finding out by
    /// rejection.
    pub fn order_rules_v0(ctx: &mut Context<OrderRulesV0Accounts>) -> Result<OrderRulesV0> {
        instructions::order_rules_v0::handle_order_rules_v0(ctx)
    }

    /// Read-only: what the book holds for a set of refs, one answer per ref.
    /// A ref that no longer names a live order comes back as
    /// `OrderViewV0::NONE`.
    pub fn orders_v0(ctx: &mut Context<OrdersV0Accounts>, args: OrdersArgsV0) -> Result<OrdersV0> {
        instructions::orders_v0::handle_orders_v0(ctx, args)
    }

    /// Read-only: the best matchable order on each side — what any cross
    /// settles between. Meant to be simulated, like `next_removal_v0`.
    pub fn next_cross_v0(ctx: &mut Context<NextCrossV0Accounts>) -> Result<NextCrossV0> {
        instructions::next_cross_v0::handle_next_cross_v0(ctx)
    }

    /// Register who resolves this book's own crank conditions. The wakes are
    /// the book's; the answers belong to the program that owns its flow.
    /// Returns the account offset the block sits at, for the watch
    /// registration.
    pub fn set_crank_conditions_v0(
        ctx: &mut Context<SetCrankConditionsV0>,
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
        ctx: &mut Context<QuoteV0>,
        args: QuoteArgsV0<'_>,
    ) -> Result<state::ResponsePointerV0> {
        instructions::quote_v0::handle_quote_v0(ctx, args)
    }

    pub fn quote_l3_v0(
        ctx: &mut Context<QuoteL3V0>,
        args: L3ArgsV0,
    ) -> Result<state::ResponsePointerV0> {
        instructions::quote_l3_v0::handle_quote_l3_v0(ctx, args)
    }

    pub fn execute_v0(
        ctx: &mut Context<ExecuteV0>,
        args: ExecuteArgsV0<'_>,
    ) -> Result<state::ResponsePointerV0> {
        instructions::execute_v0::handle_execute_v0(ctx, args)
    }

    pub fn fill_v0(ctx: &mut Context<FillV0>, args: FillArgsV0) -> Result<state::FillOutcomeV0> {
        instructions::fill_v0::handle_fill_v0(ctx, args)
    }
}
