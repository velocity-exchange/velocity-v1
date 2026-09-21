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

    pub fn initialize(
        ctx: Context<Initialize>,
        admin: Pubkey,
        cranker: Pubkey,
        treasury: Pubkey,
        tiers: Vec<Tier>,
    ) -> Result<()> {
        instructions::initialize::initialize(ctx, admin, cranker, treasury, tiers)
    }

    pub fn set_admin(ctx: Context<AdminUpdate>, new_admin: Pubkey) -> Result<()> {
        instructions::set_admin::set_admin(ctx, new_admin)
    }

    pub fn set_cranker(ctx: Context<AdminUpdate>, new_cranker: Pubkey) -> Result<()> {
        instructions::set_cranker::set_cranker(ctx, new_cranker)
    }

    pub fn set_treasury(ctx: Context<AdminUpdate>, new_treasury: Pubkey) -> Result<()> {
        instructions::set_treasury::set_treasury(ctx, new_treasury)
    }

    pub fn set_tiers(ctx: Context<AdminUpdate>, tiers: Vec<Tier>) -> Result<()> {
        instructions::set_tiers::set_tiers(ctx, tiers)
    }

    pub fn distribute(ctx: Context<Distribute>) -> Result<()> {
        instructions::distribute::distribute(ctx)
    }
}
