//! Create a [`QuoterV0`] staging entry for one perp market, quoter program and
//! quoted user. For a `Custom` entry the quoted user's authority must be the
//! creating authority, so creation is the consent. A staging entry fills
//! nothing. The admin copies its config into the market's `QuoterSlabV0` with
//! `update_quoter_approved`, and fills read only that copy. The account list is
//! set before that with `update_quoter_accounts`.
//!
//! Only the admin designates any other type. The type decides whose balances
//! the entry may move. A `Custom` entry is held to the one account it consented
//! for. A book settles for whoever rests on it, and velocity cannot bind it to
//! one account. A maker that could type its own entry as a book would get the
//! second rule, so the type is a warm-admin decision and it cannot change.
//!
//! A `Clob` entry also designates the market's book, once and for good. The
//! designation is refused when an approved quoter's registered account list
//! already names that account. Such a quoter would receive the book plus the
//! slab signature that every quoter CPI carries, and the book gates its whole
//! authority surface on that signature. See [`crate::signer`].

use {
    crate::{
        auth::check_warm,
        error::ErrorCode,
        state::{
            perp_market::PerpMarket,
            prop_amm::{
                list_stays_off_the_book, occupied_slots, QuoterSlabExt, QuoterSlabV0, QuoterType,
                QuoterV0, QUOTER_PDA_SEED, QUOTER_SLAB_PDA_SEED,
            },
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
    /// Becomes `QuoterV0::authority`, which manages the entry's config.
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
    /// Written when the entry is the market's book. A `Clob` entry becomes the
    /// market's `clob_market` here, once and for good.
    #[account(
        mut,
        seeds = [b"perp_market", args.market_index.to_le_bytes().as_ref()],
        bump
    )]
    pub perp_market: AccountLoader<'info, PerpMarket>,
    /// Read for the admin check that a non-`Custom` type needs.
    pub state: AccountLoader<'info, State>,
    /// The market's approved set. A book designation needs it, because the
    /// designation is refused when an approved entry already names the book
    /// account. Every other registration omits it.
    #[account(
        seeds = [QUOTER_SLAB_PDA_SEED, args.market_index.to_le_bytes().as_ref()],
        bump
    )]
    pub quoter_slab: Option<AccountLoader<'info, QuoterSlabV0>>,
    /// CHECK: the constraint only requires a program. Velocity never trusts it.
    #[account(executable)]
    pub quoter_program: UncheckedAccount<'info>,
    /// CHECK: the velocity `User` quoted for. For a `Custom` entry the handler
    /// loads it and requires `authority` to be its authority, so creation is the
    /// consent. A `Vamm` or `Clob` entry ignores it.
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
    // A fill calls the quoter's program, and velocity must never be that
    // program. A self-CPI would re-enter the fill under velocity's own authority
    // against accounts the fill already holds.
    validate!(
        ctx.accounts.quoter_program.key() != crate::ID,
        ErrorCode::InvalidQuoterConfig,
        "a quoter program cannot be velocity itself"
    )?;

    if args.quoter_type != QuoterType::Custom {
        // A book's response may name any user the transaction carries, so a
        // maker may not designate one.
        validate!(
            check_warm(&ctx.accounts.authority.key(), &ctx.accounts.state)?,
            ErrorCode::InvalidQuoterAuthority,
            "only the admin may register a {:?} quoter",
            args.quoter_type
        )?;
    }
    if args.quoter_type == QuoterType::Clob {
        // A book settles for whoever it says rests on it, so the program
        // behind a `Clob` entry is the trust root for maker identity. Without
        // this pin, a warm admin could point a market's book at its own
        // program and name any loaded user as a maker at a price of its choosing.
        validate!(
            ctx.accounts.quoter_program.key() == crate::ids::clob_program::id(),
            ErrorCode::InvalidQuoterConfig,
            "a Clob quoter must run velocity's CLOB program {}, not {}",
            crate::ids::clob_program::id(),
            ctx.accounts.quoter_program.key()
        )?;

        // No approved entry may already name the account being designated.
        // Approval holds a registered list apart from the market's book, and an
        // entry approved before the designation was never held to it. Such an
        // entry would receive the book plus the slab signature its own execute
        // holds. The book gates its whole authority surface on that signature.
        {
            let slab = ctx.accounts.quoter_slab.as_ref().ok_or_else(|| {
                msg!("designating a book requires the market's quoter slab");
                error!(ErrorCode::InvalidQuoterConfig)
            })?;
            let slots = slab.slots()?;
            validate!(
                occupied_slots(&slots).all(|(_, slot)| list_stays_off_the_book(
                    slot.config
                        .registered_accounts()
                        .iter()
                        .map(|meta| &meta.pubkey),
                    &args.response_account
                )),
                ErrorCode::InvalidQuoterConfig,
                "an approved quoter already names {} on its account list",
                args.response_account
            )?;
        }

        // The market names its book here and nowhere else, and the choice
        // cannot be changed afterward. A book settles for whoever rests on
        // it, so pointing a market at a second book later would put every
        // user a fill carries behind whoever holds the admin key. The stored
        // key is the book account itself, the entry's response account, and
        // every accounts struct binds market to book with `has_one = clob_market`.
        let mut perp_market = ctx.accounts.perp_market.load_mut()?;
        validate!(
            perp_market.clob_market == Pubkey::default()
                || perp_market.clob_market == args.response_account,
            ErrorCode::InvalidQuoterConfig,
            "perp market {} already names book {}",
            perp_market.market_index,
            perp_market.clob_market
        )?;

        perp_market.clob_market = args.response_account;
    }
    if args.quoter_type == QuoterType::Custom {
        // Creation is the consent. Only the quoted user's authority may
        // register a quoter that settles fills on that user's account. The
        // handler checks the owner, the discriminator and `User.authority` at
        // offset 8 by hand, because the account must be a `User` only for a
        // `Custom` entry.
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
    let config = &mut quoter.config;
    config.user = ctx.accounts.user.key();
    config.program_id = ctx.accounts.quoter_program.key();
    config.response_account = args.response_account;
    config.authority = ctx.accounts.authority.key();
    config.quote_v0_discriminator = args.quote_v0_discriminator;
    config.quote_l3_v0_discriminator = args.quote_l3_v0_discriminator;
    config.execute_v0_discriminator = args.execute_v0_discriminator;
    config.market = args.market_index;
    config.quoter_type = args.quoter_type;
    config.priority = args.quoter_type.default_priority();
    // The maker's switch starts on. Nothing fills until the admin copies this
    // config into the market's slab.
    config.is_active = true;
    Ok(())
}
