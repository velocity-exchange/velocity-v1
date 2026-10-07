//! Create a market's [`QuoterSlabV0`]. That one account holds every approved
//! quoter config for the market. The call is permissionless, because the payer
//! buys rent on an all-vacant slab and only the approval flow writes slots. Any
//! client can make sure the slab exists before it asks the admin to approve.
//!
//! The slab is born with one slot and stays right-sized. Approval grows the
//! account by exactly the slot it needs, and revocation returns trailing
//! vacancy. See `update_quoter_approved`. A fixed creation capacity would
//! either overpay rent or run out of slots, and every reader pays compute per
//! declared slot.
//!
//! A market created before `PerpMarket.quoter_slab` existed reads the field as
//! the default key. This call writes the slab PDA there. The write is safe
//! without a signer, because the address derives from the market index alone.

use {
    crate::{
        error::ErrorCode,
        state::{
            perp_market::PerpMarket,
            prop_amm::{QuoterSlabV0, QUOTER_SLAB_PDA_SEED},
        },
        validate,
    },
    anchor_lang::prelude::*,
};

#[derive(Clone, Copy, AnchorSerialize, AnchorDeserialize)]
pub struct InitializeQuoterSlabArgs {
    pub market_index: u16,
}

#[derive(Accounts)]
#[instruction(args: InitializeQuoterSlabArgs)]
pub struct InitializeQuoterSlab<'info> {
    #[account(mut)]
    pub payer: Signer<'info>,
    /// Existence check, so a slab serves a market that exists.
    #[account(
        mut,
        seeds = [b"perp_market", args.market_index.to_le_bytes().as_ref()],
        bump
    )]
    pub perp_market: AccountLoader<'info, PerpMarket>,
    #[account(
        init,
        seeds = [QUOTER_SLAB_PDA_SEED, args.market_index.to_le_bytes().as_ref()],
        space = QuoterSlabV0::space(1),
        bump,
        payer = payer
    )]
    pub quoter_slab: AccountLoader<'info, QuoterSlabV0>,
    pub rent: Sysvar<'info, Rent>,
    pub system_program: Program<'info, System>,
}

pub fn handle_initialize_quoter_slab(
    ctx: Context<InitializeQuoterSlab>,
    args: InitializeQuoterSlabArgs,
) -> Result<()> {
    bind_quoter_slab(
        &mut *ctx.accounts.perp_market.load_mut()?,
        ctx.accounts.quoter_slab.key(),
    )?;

    let mut slab = ctx.accounts.quoter_slab.load_init()?;
    slab.market = args.market_index;
    slab.capacity = 1;
    // The slab signs every external quoter CPI for this market. Store the bump
    // once so the hot paths never derive it.
    slab.bump = ctx.bumps.quoter_slab;
    Ok(())
}

/// Store the slab on a market that predates the field, and otherwise require
/// the stored slab to be this one.
fn bind_quoter_slab(perp_market: &mut PerpMarket, quoter_slab: Pubkey) -> Result<()> {
    if perp_market.quoter_slab == Pubkey::default() {
        perp_market.quoter_slab = quoter_slab;
        return Ok(());
    }

    validate!(
        perp_market.quoter_slab == quoter_slab,
        ErrorCode::InvalidMarketAccount,
        "market {} stores quoter slab {}, not {}",
        perp_market.market_index,
        perp_market.quoter_slab,
        quoter_slab
    )?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use {super::*, crate::state::pdas};

    #[test]
    fn a_market_without_a_stored_slab_stores_it() {
        let mut market = PerpMarket {
            quoter_slab: Pubkey::default(),
            ..PerpMarket::default()
        };
        let slab = pdas::quoter_slab(market.market_index);

        assert!(bind_quoter_slab(&mut market, slab).is_ok());
        assert_eq!(market.quoter_slab, slab);
    }

    #[test]
    fn a_market_that_stores_this_slab_accepts_it() {
        let slab = pdas::quoter_slab(0);
        let mut market = PerpMarket {
            quoter_slab: slab,
            ..PerpMarket::default()
        };

        assert!(bind_quoter_slab(&mut market, slab).is_ok());
        assert_eq!(market.quoter_slab, slab);
    }

    #[test]
    fn a_market_that_stores_another_slab_rejects_it() {
        let stored = Pubkey::new_unique();
        let mut market = PerpMarket {
            quoter_slab: stored,
            ..PerpMarket::default()
        };

        assert!(bind_quoter_slab(&mut market, pdas::quoter_slab(0)).is_err());
        assert_eq!(market.quoter_slab, stored);
    }
}
