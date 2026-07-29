//! Create a [`QuoterV0`] registry entry for (perp market, quoter program,
//! quoted user). For Custom quoters the quoted user's authority must be the
//! creating authority — creation is consent. The entry is born unapproved,
//! so it can never be filled against until the admin vets the CPI surface;
//! account lists are set afterwards via `update_quoter_accounts`.

use {
    crate::{
        error::ErrorCode,
        state::{
            perp_market::PerpMarket,
            prop_amm::{QuoterType, QuoterV0, QUOTER_PDA_SEED},
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
    #[account(
        seeds = [b"perp_market", args.market_index.to_le_bytes().as_ref()],
        bump
    )]
    pub perp_market: AccountLoader<'info, PerpMarket>,
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
    pub execute_v0_discriminator: [u8; 8],
}

pub fn handle_initialize_quoter(
    ctx: Context<InitializeQuoter>,
    args: InitializeQuoterArgs,
) -> Result<()> {
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
