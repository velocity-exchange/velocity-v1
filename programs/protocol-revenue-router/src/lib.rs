//! Protocol revenue router: splits Velocity's withdrawn perp protocol fees
//! between the DFX recovery pool and the treasury on a daily marginal ladder.

use {crate::state::Tier, anchor_lang::prelude::*};

pub mod errors;
pub mod events;
pub mod ids;
pub mod instructions;
pub mod math;
pub mod state;

use instructions::*;

declare_id!("39PAxdVaWHYH62bR5AWTkMVd52Y4QeJLjQ8TChupghgT");

declare_program!(dfx_redemption);

#[program]
pub mod protocol_revenue_router {
    use super::*;

    pub fn initialize(ctx: Context<Initialize>, tiers: Vec<Tier>) -> Result<()> {
        instructions::initialize::initialize(ctx, tiers)
    }

    pub fn update_config(ctx: Context<UpdateConfig>, args: UpdateConfigArgs) -> Result<()> {
        instructions::update_config::update_config(ctx, args)
    }

    pub fn distribute(ctx: Context<Distribute>) -> Result<()> {
        instructions::distribute::distribute(ctx)
    }
}
