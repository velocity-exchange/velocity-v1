//! Protocol revenue router: splits Velocity's withdrawn perp protocol fees
//! between the DFX recovery pool and the treasury on a daily marginal ladder.

// Anchor's `#[program]` emits a `let _: T = panic!()` type check per instruction argument at the
// crate root, outside any item this crate can annotate.
#![allow(clippy::diverging_sub_expression)]

use {crate::state::Tier, anchor_lang::prelude::*};

pub mod errors;
pub mod events;
pub mod ids;
pub mod instructions;
pub mod math;
pub mod state;

use instructions::*;

declare_id!("rout8Eh6aU911sDDyJaDWGY61mSfVhSNXqk1Bw9xeNn");

declare_program!(dfx_redemption);

#[program]
pub mod protocol_revenue_router {
    use super::*;

    pub fn initialize(ctx: Context<Initialize>, tiers: Vec<Tier>) -> Result<()> {
        instructions::initialize::initialize(ctx, tiers)
    }

    pub fn update_config(ctx: Context<UpdateConfig>, tiers: Option<Vec<Tier>>) -> Result<()> {
        instructions::update_config::update_config(ctx, tiers)
    }

    pub fn distribute(ctx: Context<Distribute>) -> Result<()> {
        instructions::distribute::distribute(ctx)
    }
}
