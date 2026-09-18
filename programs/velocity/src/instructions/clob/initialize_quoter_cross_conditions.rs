//! Create a Custom quoter's relay cross-discovery conditions, or re-price
//! conditions that already exist.
//!
//! The instruction is permissionless. Every input is checked against the
//! registry and the market, and the caller only pays the rent. An attach
//! requires the entry to be active and approved, and requires the market's
//! canonical CLOB to be attached. The conditions PDA derives from the entry
//! key. A repeat call re-prices in place, which covers a new keeper payment, a
//! changed watch declaration, and a rotated CLOB. The market attach is
//! idempotent in the same way.

use {
    crate::{
        error::ErrorCode,
        state::{
            clob_crank::{ClobCrankConditionsV0, CLOB_CRANK_CONDITIONS_PDA_SEED},
            perp_market::PerpMarket,
            prop_amm::{
                clob_slot_index, slot_for_entry, QuoterSlabExt, QuoterSlabV0, QuoterType, QuoterV0,
            },
            quoter_cross::{
                QuoterCrossConditionsV0, QUOTER_CROSS_CLOB, QUOTER_CROSS_CONDITIONS_PDA_SEED,
                QUOTER_CROSS_FALLBACK, QUOTER_CROSS_WATCH,
            },
            state::State,
        },
        validate,
    },
    anchor_lang::{prelude::*, Discriminator},
    relay_spec::{AccountRefV0, ConditionV0, CrankSpecV0},
    std::convert::TryInto,
};

#[derive(Accounts)]
pub struct InitializeQuoterCrossConditions<'info> {
    #[account(mut)]
    pub payer: Signer<'info>,
    pub state: AccountLoader<'info, State>,
    /// The Custom entry to discover crosses for. The conditions PDA derives
    /// from it. Its live config comes from the slab rather than from here.
    pub quoter: AccountLoader<'info, QuoterV0>,
    #[account(
        seeds = [b"perp_market", quoter.load()?.config.market.to_le_bytes().as_ref()],
        bump,
        has_one = quoter_slab
    )]
    pub perp_market: AccountLoader<'info, PerpMarket>,
    /// The market's slab. It holds the entry's approved config, and the
    /// book's config at slot 0. The book is the other leg of every staged
    /// cross. The market's `has_one` binds the slab, which costs a memcmp where
    /// a seeds constraint would pay for a PDA derivation.
    #[account()]
    pub quoter_slab: AccountLoader<'info, QuoterSlabV0>,
    /// The market's crank conditions, which are the source of truth for the
    /// keeper payment.
    #[account(
        seeds = [
            CLOB_CRANK_CONDITIONS_PDA_SEED,
            quoter.load()?.config.market.to_le_bytes().as_ref(),
        ],
        bump
    )]
    pub market_conditions: AccountLoader<'info, ClobCrankConditionsV0>,
    #[account(
        init_if_needed,
        seeds = [QUOTER_CROSS_CONDITIONS_PDA_SEED, quoter.key().as_ref()],
        space = QuoterCrossConditionsV0::SIZE,
        bump,
        payer = payer
    )]
    pub cross_conditions: AccountLoader<'info, QuoterCrossConditionsV0>,
    pub rent: Sysvar<'info, Rent>,
    pub system_program: Program<'info, System>,
}

/// Ceiling on the cross-discovery poll interval, roughly an hour of slots.
///
/// This instruction is permissionless and re-prices an existing account in
/// place, so anybody may set the interval on anybody's entry. The interval is
/// the floor under a maker's own reprice watch, and an unbounded value removes
/// that floor. The ceiling keeps the worst a third party can do to a bounded
/// delay rather than an indefinite one. For scale, the liquidation liveness
/// poll is [`crate::state::user_conditions::LIQ_LIVENESS_POLL_SLOTS`], about
/// two minutes.
pub const QUOTER_CROSS_FALLBACK_MAX_SLOTS: u64 = 9_000;

#[derive(Clone, Copy, AnchorSerialize, AnchorDeserialize)]
pub struct InitializeQuoterCrossConditionsArgs {
    /// The poll interval behind the reprice watch. It is the discovery floor
    /// when the maker's declared watch misses a reprice. Bounded above by
    /// [`QUOTER_CROSS_FALLBACK_MAX_SLOTS`].
    pub expire_fallback_slots: u64,
}

pub fn handle_initialize_quoter_cross_conditions(
    ctx: Context<InitializeQuoterCrossConditions>,
    args: InitializeQuoterCrossConditionsArgs,
) -> Result<()> {
    let InitializeQuoterCrossConditionsArgs {
        expire_fallback_slots,
    } = args;
    validate!(
        expire_fallback_slots > 0,
        ErrorCode::DefaultError,
        "fallback interval must be nonzero"
    )?;
    validate!(
        expire_fallback_slots <= QUOTER_CROSS_FALLBACK_MAX_SLOTS,
        ErrorCode::DefaultError,
        "fallback interval {} is past the {} slot ceiling",
        expire_fallback_slots,
        QUOTER_CROSS_FALLBACK_MAX_SLOTS
    )?;
    let slots = ctx.accounts.quoter_slab.slots()?;
    let quoter_slot = slot_for_entry(&slots, &ctx.accounts.quoter.key()).ok_or_else(|| {
        msg!("quoter holds no slab slot; approve it first");
        error!(ErrorCode::QuoterNotOnSlab)
    })?;
    let quoter = &slots[quoter_slot].config;
    validate!(
        quoter.quoter_type == QuoterType::Custom,
        ErrorCode::InvalidQuoterConfig,
        "cross conditions are for Custom quoters (the CLOB's are on the market conditions)"
    )?;
    validate!(
        slots[quoter_slot].quotes(),
        ErrorCode::InvalidQuoterConfig,
        "quoter must be active and approved to attach cross discovery"
    )?;
    let book_slot = clob_slot_index(&slots).ok_or_else(|| {
        msg!("quoter slab holds no book slot");
        error!(ErrorCode::QuoterNotOnSlab)
    })?;
    let clob = &slots[book_slot].config;
    validate!(
        ctx.accounts.perp_market.load()?.clob_market == clob.response_account,
        ErrorCode::InvalidQuoterConfig,
        "the slab's book is not the market's canonical book"
    )?;

    // The resolver stages `crank_cross_match` and nothing else, so the floor
    // is that crank's payment out of the market's reservoir. The watched region
    // comes from the same account. The book reported it when the market
    // attached, so nothing here derives where the book's heads sit.
    let (keeper_payment_lamports, top_of_book_offset, top_of_book_len) = {
        let market_conditions = ctx.accounts.market_conditions.load()?;
        (
            u64::from(market_conditions.crank_payments.cross),
            market_conditions.top_of_book_offset,
            market_conditions.top_of_book_len,
        )
    };
    validate!(
        top_of_book_len > 0,
        ErrorCode::InvalidQuoterConfig,
        "market conditions carry no top-of-book region; re-run the market's attach"
    )?;
    let (oracle, quote_spot_market_index) = {
        let market = ctx.accounts.perp_market.load()?;
        (market.oracle, market.quote_spot_market_index)
    };
    let clob_market = clob.response_account;

    // The resolver's account list, in order: the shared scratch at index 0,
    // which is where the response pointer says the payload lives, the
    // conditions, the CLOB book, the state, the market's quoter slab holding
    // both legs' approved configs, the entry's quoted user, the CLOB program,
    // then the entry's registered quote surface and its program. That is
    // everything the two generic quote CPIs need.
    //
    // The list is stored once next to the block. Each condition points at it
    // through relay's resolver-list indirection rather than inlining a copy.
    //
    // The book is writable because the resolver quotes it through
    // `quote_l3_v0`, which streams its answer into the market account's own
    // response tail. Nothing a resolver sends ever lands.
    let mut resolver_accounts = vec![
        AccountRefV0::writable(crate::state::pdas::relay_scratch().to_bytes()),
        AccountRefV0::readonly(ctx.accounts.cross_conditions.key().to_bytes()),
        AccountRefV0::writable(clob_market.to_bytes()),
        AccountRefV0::readonly(ctx.accounts.state.key().to_bytes()),
        AccountRefV0::readonly(ctx.accounts.quoter_slab.key().to_bytes()),
        AccountRefV0::readonly(quoter.user.to_bytes()),
        AccountRefV0::readonly(clob.program_id.to_bytes()),
    ];
    for meta in quoter.leg_metas(quoter.quote_leg_indexes())? {
        resolver_accounts.push(if meta.is_writable {
            AccountRefV0::writable(meta.pubkey.to_bytes())
        } else {
            AccountRefV0::readonly(meta.pubkey.to_bytes())
        });
    }
    resolver_accounts.push(AccountRefV0::readonly(quoter.program_id.to_bytes()));

    let disc8 = |disc: &[u8]| -> Result<[u8; 8]> {
        disc.try_into().map_err(|_| error!(ErrorCode::DefaultError))
    };
    let spec = CrankSpecV0 {
        resolver_program: crate::ID.to_bytes(),
        resolver_disc: disc8(crate::instruction::ResolveCrankCrossMatchQuoter::DISCRIMINATOR)?,
        min_payment: keeper_payment_lamports,
    };

    let mut conditions = ctx.accounts.cross_conditions.load_init().or_else(|_| {
        // The account already exists on a re-attach, so re-price in place.
        ctx.accounts.cross_conditions.load_mut()
    })?;
    conditions.quoter = ctx.accounts.quoter.key();
    conditions.clob_market = clob_market;
    conditions.clob_program = clob.program_id;
    conditions.oracle = oracle;
    conditions.market_index = quoter.market;
    conditions.quote_spot_market_index = quote_spot_market_index;
    conditions.init_block()?;
    let resolvers = conditions.write_resolver_list(&resolver_accounts)?;

    // The maker-declared reprice watch. It stays inactive when the maker
    // declares nothing. The fallback poll is then the only wake for this
    // side.
    if quoter.watch_len > 0 {
        conditions.set_condition(
            QUOTER_CROSS_WATCH,
            &(ConditionV0::on_account_change(
                relay_spec::WatchedRegion::new(
                    quoter.watch_account.to_bytes(),
                    quoter.watch_offset,
                    quoter.watch_len,
                ),
                spec,
                resolvers,
            )),
        )?;
    } else {
        conditions.set_condition(
            QUOTER_CROSS_WATCH,
            &relay_spec::bytemuck::Zeroable::zeroed(),
        )?;
    }
    conditions.set_condition(
        QUOTER_CROSS_CLOB,
        // The book's own top-of-book region. A crossing order is always a new
        // best, so a change there covers every cross this entry could take.
        &(ConditionV0::on_account_change(
            relay_spec::WatchedRegion::new(
                clob_market.to_bytes(),
                top_of_book_offset,
                top_of_book_len,
            ),
            spec,
            resolvers,
        )),
    )?;
    conditions.set_condition(
        QUOTER_CROSS_FALLBACK,
        &(ConditionV0::every_slots(expire_fallback_slots, spec, resolvers)),
    )?;
    Ok(())
}
