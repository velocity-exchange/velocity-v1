//! Create a [`QuoterV0`] registry entry for (perp market, quoter program,
//! quoted user). For Custom quoters the quoted user's authority must be the
//! creating authority — creation is consent. The entry is born unapproved,
//! so it can never be filled against until the admin vets the CPI surface;
//! account lists are set afterwards via `update_quoter_accounts`.
//!
//! Any other type is the admin's to designate. The type decides whose
//! balances the entry may move: a Custom entry is held to the one account it
//! consented for, while a book settles for whoever rests on it, which
//! velocity cannot bind it to. A maker that could type its own entry as a
//! book would be asking for the second rule and getting it, so the type is a
//! warm-admin decision and it cannot be changed afterwards.

use {
    crate::{
        auth::check_warm,
        error::ErrorCode,
        state::{
            perp_market::PerpMarket,
            prop_amm::{QuoterType, QuoterV0, QUOTER_PDA_SEED},
            state::State,
            traits::Size,
            user::User,
        },
        validate,
    },
    anchor_lang::prelude::*,
};

#[derive(Accounts)]
#[instruction(args: InitializeQuoterArgs)]
pub struct InitializeQuoter<'info> {
    #[account(mut)]
    pub payer: Signer<'info>,
    /// Becomes `QuoterV0::authority` — manages the entry's config.
    pub authority: Signer<'info>,
    #[account(
        init,
        seeds = [
            QUOTER_PDA_SEED,
            args.market_index.to_le_bytes().as_ref(),
            quoter_program.key().as_ref(),
            user.key().as_ref(),
        ],
        space = QuoterV0::SIZE,
        bump,
        payer = payer
    )]
    pub quoter: AccountLoader<'info, QuoterV0>,
    /// Written when the entry is the market's book: a Clob-type entry becomes
    /// the market's `clob_quoter` here, once and for good.
    #[account(
        mut,
        seeds = [b"perp_market", args.market_index.to_le_bytes().as_ref()],
        bump
    )]
    pub perp_market: AccountLoader<'info, PerpMarket>,
    /// Read for the admin check a non-Custom type needs.
    pub state: AccountLoader<'info, State>,
    /// CHECK: only constrained to be a program; velocity never trusts it.
    #[account(executable)]
    pub quoter_program: UncheckedAccount<'info>,
    /// CHECK: the velocity `User` quoted for. For Custom quoters the handler
    /// loads it and requires `authority` to be its authority (creation is
    /// consent). Ignored for Vamm/Clob-type entries.
    pub user: UncheckedAccount<'info>,
    pub rent: Sysvar<'info, Rent>,
    pub system_program: Program<'info, System>,
}

#[derive(Clone, AnchorSerialize, AnchorDeserialize)]
pub struct InitializeQuoterArgs {
    pub market_index: u16,
    pub quoter_type: QuoterType,
    pub response_account: Pubkey,
    pub quote_v0_discriminator: [u8; 8],
    /// Zero when the quoter has no `quote_l3_v0` leg, which is every quoter
    /// that fills from one account.
    pub quote_l3_v0_discriminator: [u8; 8],
    pub execute_v0_discriminator: [u8; 8],
}

pub fn handle_initialize_quoter(
    ctx: Context<InitializeQuoter>,
    args: InitializeQuoterArgs,
) -> Result<()> {
    // A quoter's program is CPI'd during a fill; velocity itself must never be
    // that program. A self-CPI would re-enter the fill under velocity's own
    // authority against accounts a fill already holds, so the callee is always
    // a distinct program.
    validate!(
        ctx.accounts.quoter_program.key() != crate::ID,
        ErrorCode::InvalidQuoterConfig,
        "a quoter program cannot be velocity itself"
    )?;
    if args.quoter_type != QuoterType::Custom {
        // A book's response may name any user the transaction carries, so
        // designating one is not a maker's call to make.
        validate!(
            check_warm(&ctx.accounts.authority.key(), &ctx.accounts.state)?,
            ErrorCode::InvalidQuoterAuthority,
            "only the admin may register a {:?} quoter",
            args.quoter_type
        )?;
    }
    if args.quoter_type == QuoterType::Clob {
        // A book settles for whoever it says rests on it, so the program behind
        // a Clob entry is the trust root for maker identity. Pin it to the CLOB
        // velocity wrote, so the admin's power is to give a market a book, not
        // to choose the code a book runs. Without this a warm admin could point
        // a market's book at a program of its own and name any loaded user as a
        // maker at a price of its choosing.
        validate!(
            ctx.accounts.quoter_program.key() == crate::ids::clob_program::id(),
            ErrorCode::InvalidQuoterConfig,
            "a Clob quoter must run velocity's CLOB program {}, not {}",
            crate::ids::clob_program::id(),
            ctx.accounts.quoter_program.key()
        )?;
        // The market names its book here, and only here: a book settles for
        // whoever rests on it, so a market that could be pointed at a second
        // one later would put every user a fill carries behind whoever holds
        // the admin key. Registering the book *is* the designation, and it is
        // a one-way door.
        let mut perp_market = ctx.accounts.perp_market.load_mut()?;
        // A market may name its book before the entry exists — the PDA is
        // derivable — so naming the entry being registered is the ordinary
        // case. Naming a *different* one is the case this refuses.
        validate!(
            perp_market.clob_quoter == Pubkey::default()
                || perp_market.clob_quoter == ctx.accounts.quoter.key(),
            ErrorCode::InvalidQuoterConfig,
            "perp market {} already names clob quoter {}",
            perp_market.market_index,
            perp_market.clob_quoter
        )?;
        perp_market.clob_quoter = ctx.accounts.quoter.key();
    }
    if args.quoter_type == QuoterType::Custom {
        // Creation is consent: only the quoted user's authority may register
        // a quoter that settles fills on that user's account. Verified
        // manually (owner + discriminator + `User.authority` at offset 8,
        // the struct's first field) because the account is only required to
        // be a `User` for Custom entries.
        let info = &ctx.accounts.user;
        validate!(
            info.owner == &crate::ID,
            ErrorCode::InvalidQuoterConfig,
            "quoted user is not a velocity account"
        )?;
        let data = info.try_borrow_data()?;
        validate!(
            data.len() >= 40 && &data[..8] == User::DISCRIMINATOR,
            ErrorCode::InvalidQuoterConfig,
            "quoted user is not a User account"
        )?;
        let mut authority_bytes = [0u8; 32];
        authority_bytes.copy_from_slice(&data[8..40]);
        let user_authority = Pubkey::new_from_array(authority_bytes);
        validate!(
            user_authority == ctx.accounts.authority.key(),
            ErrorCode::InvalidQuoterAuthority,
            "custom quoters must be created by the quoted user's authority"
        )?;
    }

    let mut quoter = ctx.accounts.quoter.load_init()?;
    quoter.user = ctx.accounts.user.key();
    quoter.program_id = ctx.accounts.quoter_program.key();
    quoter.response_account = args.response_account;
    quoter.authority = ctx.accounts.authority.key();
    quoter.quote_v0_discriminator = args.quote_v0_discriminator;
    quoter.quote_l3_v0_discriminator = args.quote_l3_v0_discriminator;
    quoter.execute_v0_discriminator = args.execute_v0_discriminator;
    quoter.market = args.market_index;
    quoter.quoter_type = args.quoter_type;
    quoter.priority = args.quoter_type.default_priority();
    // The maker's own switch is on from birth; nothing fills until the admin
    // vets the CPI surface (`is_approved`).
    quoter.is_active = true;
    quoter.is_approved = false;
    Ok(())
}
