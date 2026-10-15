//! The maker's on/off switch. The entry authority can always reach it. For a
//! `Custom` entry that authority is the quoted user's own key, fixed at
//! creation, so a maker can always stop their quoter. A book's entry answers
//! to the State admin roles, because its stored authority is only the admin
//! key that registered it. The switch writes through to the approved copy in
//! the market's slab, so a kill takes effect at once instead of at the next
//! approval.
//!
//! Activation of an approved `Custom` entry on a market whose book is attached
//! also creates or re-prices the entry's cross conditions, and the authority
//! pays the rent. Relay cranks no cross against the entry without them, so
//! activation of such an entry refuses to run without those accounts.

use {
    crate::{
        error::ErrorCode,
        instructions::{
            clob::{CrossConditionsWrite, MarketCrossTerms},
            quoter_registry::check_quoter_config_authority,
        },
        state::{
            clob_crank::{ClobCrankConditionsV0, CLOB_CRANK_CONDITIONS_PDA_SEED},
            perp_market::PerpMarket,
            prop_amm::{
                slot_for_entry, QuoterSlabExt, QuoterSlabV0, QuoterType, QuoterV0,
                QUOTER_SLAB_PDA_SEED,
            },
            quoter_cross::{QuoterCrossConditionsV0, QUOTER_CROSS_CONDITIONS_PDA_SEED},
            state::State,
        },
        validate,
    },
    anchor_lang::{prelude::*, Discriminator},
};

#[derive(Accounts)]
pub struct UpdateQuoterActive<'info> {
    /// Mutable, because it pays the cross conditions' rent on first arming.
    #[account(mut)]
    pub authority: Signer<'info>,
    #[account(mut)]
    pub quoter: AccountLoader<'info, QuoterV0>,
    /// The market's slab, so the switch reaches the approved copy. It is
    /// optional because an entry that was never approved has no copy to write.
    /// An approved entry that omits it keeps the live copy as it was, and the
    /// staged value lands at the next approval. A maker who wants the switch
    /// to take effect at once passes the slab.
    #[account(
        mut,
        seeds = [
            QUOTER_SLAB_PDA_SEED,
            quoter.load()?.config.market.to_le_bytes().as_ref(),
        ],

        bump
    )]
    pub quoter_slab: Option<AccountLoader<'info, QuoterSlabV0>>,
    /// Read for the admin check that a non-`Custom` entry needs. A `Custom`
    /// entry answers to its own stored authority and omits this account.
    pub state: Option<AccountLoader<'info, State>>,
    /// The rest arm the entry's cross conditions. Activation of a `Custom`
    /// entry that holds a slot requires `market_conditions`. It requires the
    /// rest when that account shows the book attached, and refuses them when
    /// it does not.
    #[account(
        seeds = [b"perp_market", quoter.load()?.config.market.to_le_bytes().as_ref()],
        bump
    )]
    pub perp_market: Option<AccountLoader<'info, PerpMarket>>,
    /// CHECK: bound by address. It is read only when the attach created it.
    #[account(
        seeds = [
            CLOB_CRANK_CONDITIONS_PDA_SEED,
            quoter.load()?.config.market.to_le_bytes().as_ref(),
        ],

        bump
    )]
    pub market_conditions: Option<UncheckedAccount<'info>>,
    #[account(
        init_if_needed,
        seeds = [QUOTER_CROSS_CONDITIONS_PDA_SEED, quoter.key().as_ref()],
        space = QuoterCrossConditionsV0::SIZE,
        bump,
        payer = authority
    )]
    pub cross_conditions: Option<AccountLoader<'info, QuoterCrossConditionsV0>>,
    pub system_program: Option<Program<'info, System>>,
}

#[derive(Clone, Copy, AnchorSerialize, AnchorDeserialize)]
pub struct UpdateQuoterActiveArgs {
    pub active: bool,
}

pub fn handle_update_quoter_active(
    ctx: Context<UpdateQuoterActive>,
    args: UpdateQuoterActiveArgs,
) -> Result<()> {
    let UpdateQuoterActiveArgs { active } = args;
    check_quoter_config_authority(
        &ctx.accounts.quoter.load()?.config,
        &ctx.accounts.authority.key(),
        ctx.accounts.state.as_ref(),
    )?;

    ctx.accounts.quoter.load_mut()?.config.is_active = active;
    let holds_slot = match &ctx.accounts.quoter_slab {
        Some(slab) => {
            let mut slots = slab.slots_mut()?;
            let index = slot_for_entry(&slots, &ctx.accounts.quoter.key());
            if let Some(index) = index {
                slots[index].config.is_active = active;
            }

            index.is_some()
        }
        None => false,
    };

    let is_custom = ctx.accounts.quoter.load()?.config.quoter_type == QuoterType::Custom;
    match &ctx.accounts.quoter_slab {
        Some(slab) if active && is_custom && holds_slot => arm_cross_conditions(ctx.accounts, slab),
        // Anchor creates a passed `cross_conditions` before the handler runs,
        // so a call that arms nothing must not pass one.
        _ => {
            validate!(
                ctx.accounts.cross_conditions.is_none(),
                ErrorCode::InvalidQuoterConfig,
                "this call arms no cross conditions; omit cross_conditions"
            )?;
            Ok(())
        }
    }
}

fn arm_cross_conditions<'info>(
    accounts: &UpdateQuoterActive<'info>,
    slab: &AccountLoader<'info, QuoterSlabV0>,
) -> Result<()> {
    let market_conditions = accounts.market_conditions.as_ref().ok_or_else(|| {
        msg!("activating an approved Custom entry requires market_conditions");
        error!(ErrorCode::InvalidQuoterConfig)
    })?;
    let Some(market) = attached_market_terms(market_conditions)? else {
        validate!(
            accounts.cross_conditions.is_none(),
            ErrorCode::InvalidQuoterConfig,
            "the market's book is not attached; omit cross_conditions"
        )?;
        return Ok(());
    };

    let (Some(perp_market), Some(cross_conditions)) =
        (&accounts.perp_market, &accounts.cross_conditions)
    else {
        msg!("activating a Custom entry on an attached market requires perp_market and cross_conditions");
        return Err(error!(ErrorCode::InvalidQuoterConfig));
    };

    CrossConditionsWrite {
        state: crate::state::pdas::state(),
        quoter: accounts.quoter.key(),
        caller: accounts.authority.key(),
        perp_market,
        quoter_slab: slab,
        market,
        cross_conditions,
    }
    .write(None)
}

/// The market's cross terms, or `None` while no attach has created its crank
/// conditions with a top-of-book region.
fn attached_market_terms(market_conditions: &AccountInfo) -> Result<Option<MarketCrossTerms>> {
    if market_conditions.owner != &crate::ID {
        return Ok(None);
    }

    let data = market_conditions.try_borrow_data()?;
    let body = data
        .strip_prefix(ClobCrankConditionsV0::DISCRIMINATOR)
        .and_then(|body| body.get(..std::mem::size_of::<ClobCrankConditionsV0>()));
    let Some(body) = body else {
        return Ok(None);
    };

    let conditions = bytemuck::pod_read_unaligned::<ClobCrankConditionsV0>(body);
    if conditions.top_of_book_len == 0 {
        return Ok(None);
    }

    MarketCrossTerms::from_conditions(&conditions).map(Some)
}
