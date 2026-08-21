//! Velocity midpoint (spline) quoter program (Anchor v2 / anchor-next).
//!
//! **Production and multi-tenant**: makers don't deploy their own quoter
//! programs — they create [`state::MidpointQuoterV0`] PDA instances of this
//! one and register them as Custom quoters in velocity's registry. Each
//! instance is a *spline around a midpoint*: per-side ladders of
//! `(offset-from-mid, size)` that change rarely, plus a mid price the
//! maker's hot key tracks tick-by-tick through `set_mid_v0` — the hot path,
//! deliberately kept near the compute floor (see its module doc) and
//! CU-pinned by a litesvm test.
//!
//! Exposed to velocity through the quoter interface: `quote_v0`/`execute_v0`
//! stream borsh responses directly into the instance's response tail and
//! return a `ResponsePointerV0` via return data. Execute is gated on the
//! registered `execute_authority` (velocity's quoter CPI signer PDA —
//! velocity clamps size to the quoted user's margin before CPI'ing here). Safety is the
//! mid-staleness gate: a dead feed stops quoting on its own.
//!
//! Two authorities are held per instance and neither derives from the other:
//! the maker's config key (`authority`) and the quoted velocity `User`'s
//! wallet (`user_authority`, which signs creation and seeds the PDA). Nothing
//! *trust*-bearing is configured locally: the attested-flow gate reads
//! velocity's live `State.hot_flow_authority` on every quote (see
//! [`velocity`]), so rotating a compromised flow key is one velocity admin
//! call, not a per-maker migration.
//!
//! Discriminators stay 8-byte anchor defaults: the velocity quoter registry
//! stores `[u8; 8]` discriminators.

use anchor_lang_v2::prelude::*;

pub mod emit;
pub mod error;
pub mod events;
pub mod instructions;
pub mod introspection;
pub mod state;
pub mod velocity;

// Re-exported so integration tests can reach wincode/BORSH_CONFIG through the
// crate without their own git dep.
pub use {anchor_lang_v2, instructions::*};

declare_id!("eb3Kwmht4evPGGonNHCQs1h7ng63ZUwZ9TyV1qPo23D");

#[program]
pub mod midpoint {
    use super::*;

    pub fn initialize_quoter_v0(
        ctx: &mut Context<InitializeQuoterV0>,
        config: state::QuoterConfigV0,
    ) -> Result<()> {
        instructions::initialize_quoter_v0::handle_initialize_quoter_v0(ctx, config)
    }

    pub fn update_quoter_v0(
        ctx: &mut Context<UpdateQuoterV0>,
        args: UpdateQuoterArgsV0,
    ) -> Result<()> {
        instructions::update_quoter_v0::handle_update_quoter_v0(ctx, args)
    }

    pub fn set_mid_v0(ctx: &mut Context<SetMidV0>, args: SetMidArgsV0) -> Result<()> {
        instructions::set_mid_v0::handle_set_mid_v0(ctx, args)
    }

    pub fn set_levels_v0(ctx: &mut Context<SetLevelsV0>, args: SetLevelsArgsV0) -> Result<()> {
        instructions::set_levels_v0::handle_set_levels_v0(ctx, args)
    }

    pub fn cancel_all_v0(
        ctx: &mut Context<CancelAllV0>,
        args: CancelAllArgsV0,
    ) -> Result<state::CancelAllOutcomeV0> {
        instructions::cancel_all_v0::handle_cancel_all_v0(ctx, args)
    }

    pub fn quote_v0(
        ctx: &mut Context<QuoteV0>,
        args: QuoteArgsV0<'_>,
    ) -> Result<state::ResponsePointerV0> {
        instructions::quote_v0::handle_quote_v0(ctx, args)
    }

    pub fn execute_v0(
        ctx: &mut Context<ExecuteV0>,
        args: ExecuteArgsV0<'_>,
    ) -> Result<state::ResponsePointerV0> {
        instructions::execute_v0::handle_execute_v0(ctx, args)
    }
}
