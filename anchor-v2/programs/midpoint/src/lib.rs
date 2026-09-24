//! Velocity midpoint (spline) quoter program (Anchor v2 / anchor-next).
//!
//! The program is multi-tenant. A maker does not deploy a quoter program. A
//! maker creates a [`state::MidpointQuoterV0`] PDA instance of this one and
//! registers it as a Custom quoter in velocity's registry. Each instance is a
//! spline around a midpoint. It holds a per-side ladder of
//! `(offset-from-mid, size)` levels that change rarely, plus a mid price. The
//! maker's hot key tracks the mid tick by tick through `set_mid_v0`. That is
//! the hot path. It stays near the compute floor, and a litesvm test pins its
//! compute use.
//!
//! Velocity reaches this program through the quoter interface. `quote_v0` and
//! `execute_v0` stream wincode responses into the instance's response tail and
//! return a `ResponsePointerV0` through return data. Execute is gated on the
//! registered `execute_authority`, which is velocity's quoter CPI signer PDA.
//! Velocity clamps size to the quoted user's margin before it calls here. The
//! mid-staleness gate is the safety property. A dead feed stops quoting on its
//! own.
//!
//! Each instance holds two authorities, and neither derives from the other.
//! The maker's config key is `authority`. The quoted velocity `User`'s wallet
//! is `user_authority`, which signs creation and seeds the PDA. No trust
//! bearing value is configured locally. The protected-flow gate reads
//! `taker_served_window` off the quoter wire. That field is velocity's
//! assertion that the flow served the swift hold or the book's activation
//! delay. Velocity reads the flow co-signature once, at its own boundary, so
//! rotating a compromised flow key is one velocity admin call and not a
//! per-maker migration.
//!
//! Discriminators stay at the 8-byte anchor default. Velocity's quoter registry
//! stores `[u8; 8]` discriminators.

use anchor_lang::prelude::*;

pub mod emit;
pub mod error;
pub mod events;
pub mod instructions;
pub mod state;

// Re-exported so integration tests reach wincode and BORSH_CONFIG through this
// crate without their own git dependency.
pub use {anchor_lang, instructions::*};

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
