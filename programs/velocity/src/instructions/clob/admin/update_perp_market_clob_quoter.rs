//! Attach a market's canonical CLOB quoter, or re-price one that is already
//! attached. The instruction names the book's slab entry as the mandatory
//! route baseline, mirrors the book's placement rules, and writes the market's
//! relay crank conditions and keeper reservoir.

use {
    crate::{
        auth::check_warm,
        error::ErrorCode,
        instructions::constraints::perp_market_valid,
        load_mut, msg,
        state::{
            clob_crank::{ClobCrankConditionsV0, CrankCostUnitsV0, CrankPaymentsV0},
            perp_market::PerpMarket,
            prop_amm::{ClobCrankBlockV0, ClobMarket, QuoterSlabExt, QuoterSlabV0, QuoterV0},
            state::State,
        },
        validate,
    },
    anchor_lang::prelude::*,
};

#[derive(Accounts)]
pub struct AdminUpdatePerpMarketClobQuoter<'info> {
    /// Also pays the conditions account's rent on first attach.
    #[account(mut, constraint = check_warm(&admin.key(), &state)?)]
    pub admin: Signer<'info>,
    pub state: AccountLoader<'info, State>,
    /// `has_one = clob_market` holds because registration
    /// (`initialize_quoter`) designated the book before any attach.
    #[account(mut, has_one = quoter_slab, has_one = clob_market)]
    pub perp_market: AccountLoader<'info, PerpMarket>,
    /// Writable, because the attach mirrors the book's placement rules onto
    /// the staging entry. A later re-approval then copies them forward.
    #[account(mut)]
    pub quoter: AccountLoader<'info, crate::state::prop_amm::QuoterV0>,
    /// Writable, because the attach mirrors the book's placement rules onto
    /// the approved copy in the book's slot. The hot paths then read a loaded
    /// field instead of calling `order_rules_v0` by CPI. The market's `has_one`
    /// binds this account.
    #[account(mut)]
    pub quoter_slab: AccountLoader<'info, crate::state::prop_amm::QuoterSlabV0>,
    /// CHECK: the perp market's `has_one` binds it to the book the market
    /// designated. Writable because the attach registers velocity's resolvers
    /// on it. The conditions for expiry, activation, a side at its cap and a
    /// crossed book live on this account because those are facts about it.
    #[account(mut)]
    pub clob_market: UncheckedAccount<'info>,
    /// CHECK: a Clob slot's program is pinned to velocity's CLOB at
    /// registration. The handler checks it again through the slot.
    #[account(address = crate::ids::clob_program::id())]
    pub clob_program: UncheckedAccount<'info>,
    /// The market's relay conditions and keeper reservoir. The attach creates
    /// them, or re-prices them, so a new market needs no separate crank
    /// ceremony.
    #[account(
        init_if_needed,
        seeds = [
            crate::state::clob_crank::CLOB_CRANK_CONDITIONS_PDA_SEED,
            perp_market.load()?.market_index.to_le_bytes().as_ref(),
        ],

        space = crate::state::clob_crank::ClobCrankConditionsV0::SIZE,
        bump,
        payer = admin
    )]
    pub crank_conditions: AccountLoader<'info, crate::state::clob_crank::ClobCrankConditionsV0>,
    /// Read-only. The treasury holds the levels a reservoir is kept between.
    /// The attach resolves the low level onto this market.
    #[account(
        seeds = [crate::state::crank_treasury::CRANK_TREASURY_PDA_SEED],
        bump
    )]
    pub treasury: AccountLoader<'info, crate::state::crank_treasury::CrankTreasuryV0>,
    pub rent: Sysvar<'info, Rent>,
    pub system_program: Program<'info, System>,
}

/// Names the market's canonical CLOB quoter entry. Every router fill must
/// then carry it, so no route can exclude the book. A dead entry is still
/// passed but skipped at quote time, so deactivating the book never stops
/// fills. There is no clear instruction. Kill the entry instead.
#[derive(Clone, Copy, AnchorSerialize, AnchorDeserialize)]
pub struct UpdatePerpMarketClobQuoterArgs {
    /// What each crank requests, measured by simulating it. The lamport
    /// payments derive from these and `State.transaction_fee_rails`. A change
    /// in what the network charges is therefore one write to the rails plus a
    /// re-run of this instruction per market.
    pub crank_cost_units: CrankCostUnitsV0,
    /// The cross fallback poll interval, in slots.
    pub expire_fallback_slots: u64,
    /// The least a cross must clear by before the resolver stages it.
    pub min_cross_surplus: u64,
}

#[access_control(
    perp_market_valid(&ctx.accounts.perp_market)
)]
pub fn handle_update_perp_market_clob_quoter(
    ctx: Context<AdminUpdatePerpMarketClobQuoter>,
    args: UpdatePerpMarketClobQuoterArgs,
) -> Result<()> {
    let UpdatePerpMarketClobQuoterArgs {
        crank_cost_units,
        expire_fallback_slots,
        min_cross_surplus,
    } = args;
    let perp_market = &mut load_mut!(ctx.accounts.perp_market)?;
    msg!("perp market {}", perp_market.market_index);

    let (clob, clob_program) = bind_book_slot(
        &ctx.accounts.quoter_slab,
        &ctx.accounts.quoter,
        &ctx.accounts.clob_market,
        &ctx.accounts.clob_program,
        perp_market.market_index,
    )?;
    let keys = crate::instructions::ClobCrankConditionKeys {
        crank_conditions: ctx.accounts.crank_conditions.key(),
        clob_market: ctx.accounts.clob_market.key(),
        clob_program,
        quoter_slab: ctx.accounts.quoter_slab.key(),
        state: ctx.accounts.state.key(),
        oracle: perp_market.oracle,
        quote_spot_market_index: perp_market.quote_spot_market_index,
    };
    let payments = CrankPaymentsV0::derive(
        &ctx.accounts.state.load()?.transaction_fee_rails,
        &crank_cost_units,
    )?;

    // A cross must clear by more than nothing. Cranking one pays
    // `crank_payments.cross` out of the reservoir, so a cheap cross costs the
    // protocol to run, and anyone can create one by resting two orders a tick
    // apart. The caller states the figure because the instruction cannot
    // derive it: it has no SOL oracle to price the lamport payout in quote.
    validate!(
        min_cross_surplus > 0,
        ErrorCode::DefaultError,
        "min_cross_surplus must cover what the reservoir pays for a cross"
    )?;

    mirror_book_placement_rules(
        &clob,
        perp_market,
        &ctx.accounts.quoter_slab,
        &ctx.accounts.quoter,
    )?;

    // The book's own conditions come first. Velocity registers which resolver
    // answers each one and what it pays, and the book keeps their wakes
    // current. The book reports where its condition block sits and which
    // bytes change on a top-of-book move, so nothing here knows its layout.
    let block = clob.set_crank_conditions(crate::instructions::clob_crank_registration(
        &keys, payments,
    )?)?;

    msg!(
        "clob crank conditions at book offset {}",
        block.block_offset
    );

    // The wake level is the treasury's setting resolved against this market's
    // dearest crank. It resolves here rather than at refill time, because it is
    // the threshold the condition itself carries.
    let refill_watermark_lamports = ctx
        .accounts
        .treasury
        .load()?
        .refill_watermark(payments.max_payment())?;

    write_market_crank_conditions(
        &ctx.accounts.crank_conditions,
        &keys,
        perp_market.market_index,
        payments,
        &block,
        min_cross_surplus,
        expire_fallback_slots,
        refill_watermark_lamports,
    )?;

    // A market names its book once, at registration in `initialize_quoter`.
    // The accounts struct's `has_one = clob_market` holds this attach to that
    // designation and nothing here writes it. A path that could repoint the
    // market later would put every user a fill carries behind the admin key.
    Ok(())
}

/// Bind the book the market's slab names, and run the two identity checks
/// `ClobMarket::from_slab` leaves out. The slab's book slot must hold the
/// passed entry, and that entry must name the passed CLOB program. Returns the
/// bound book and the program id the entry registered.
fn bind_book_slot<'a, 'info>(
    quoter_slab: &'a AccountLoader<'info, QuoterSlabV0>,
    quoter: &AccountLoader<'info, QuoterV0>,
    clob_market: &'a AccountInfo<'info>,
    clob_program: &'a AccountInfo<'info>,
    market_index: u16,
) -> Result<(ClobMarket<'a, 'info>, Pubkey)> {
    // This binds the slab's book slot to this market and to the passed book
    // account. The two checks below are the ones it does not run.
    let clob = ClobMarket::from_slab(quoter_slab, market_index, clob_market, clob_program)?;
    let registered_program = {
        let book_slot = quoter_slab.clob_slot(market_index)?;
        validate!(
            book_slot.entry == quoter.key(),
            ErrorCode::DefaultError,
            "the slab's book slot holds entry {}, not the passed one",
            book_slot.entry
        )?;

        book_slot.config.program_id
    };

    validate!(
        registered_program == clob_program.key(),
        ErrorCode::DefaultError,
        "clob program does not match the quoter entry"
    )?;

    Ok((clob, registered_program))
}

/// Hold the book's placement rules to the market's, then mirror them onto the
/// book's slab slot and onto the staging entry.
///
/// The book's own floor on a resting order must sit at or under the market's
/// floor. A fill unwinds a culled remainder by releasing its base from the
/// maker's open-order aggregate, and the book reports the size of that release.
/// The fill path bounds the release by the market's minimum, which is a bound
/// only while the book cannot cull something larger.
fn mirror_book_placement_rules(
    clob: &ClobMarket<'_, '_>,
    perp_market: &PerpMarket,
    quoter_slab: &AccountLoader<QuoterSlabV0>,
    quoter: &AccountLoader<QuoterV0>,
) -> Result<()> {
    let rules = clob.reader().order_rules()?;
    // A market with no minimum of its own has nothing to bound against and
    // nothing to bound. The book's cull fires on a remainder under the book's
    // own minimum. Releasing that from an aggregate the market never reserved
    // against changes nothing.
    validate!(
        perp_market.market_stats.min_order_size == 0
            || rules.min_order_size <= perp_market.market_stats.min_order_size,
        ErrorCode::DefaultError,
        "book minimum order size {} is above the market's {}",
        rules.min_order_size,
        perp_market.market_stats.min_order_size
    )?;

    // The book's place authority is its trust root. Velocity signs every
    // external quoter CPI as the market's quoter slab PDA, so only a book
    // pinned to that PDA can be attached without forging orders. Immutable
    // on the book, so a passing book stays pinned for the attachment's life.
    validate!(
        rules.place_authority == quoter_slab.key().to_bytes(),
        ErrorCode::DefaultError,
        "book place authority is not the market's quoter slab"
    )?;

    // The book's grid must match the market's grid. A remainder aligned to the
    // market can then always rest. An off-tick or off-step remainder would
    // revert the whole fill that carried it.
    validate!(
        rules.tick_size == perp_market.order_tick_size,
        ErrorCode::DefaultError,
        "book tick {} does not match the market tick {}",
        rules.tick_size,
        perp_market.order_tick_size
    )?;
    validate!(
        rules.step_size == perp_market.order_step_size,
        ErrorCode::DefaultError,
        "book step {} does not match the market step {}",
        rules.step_size,
        perp_market.order_step_size
    )?;

    // Mirrors the rules onto the book's slot and the staging entry, so the
    // take gate, the maker-priority skip and the remainder rest read the slot
    // copy instead of calling `order_rules_v0` by CPI on every fill. The
    // staging copy carries forward to a later re-approval.
    {
        let mut slots = quoter_slab.slots_mut()?;
        let index = crate::state::prop_amm::clob_slot_index(&slots)
            .ok_or_else(|| error!(ErrorCode::QuoterNotOnSlab))?;
        slots[index].config.book_tick_size = rules.tick_size;
        slots[index].config.book_min_order_size = rules.min_order_size;
        slots[index].config.book_default_activation_delay_slots =
            rules.default_activation_delay_slots;
    }

    let mut quoter = quoter.load_mut()?;
    quoter.config.book_tick_size = rules.tick_size;
    quoter.config.book_min_order_size = rules.min_order_size;
    quoter.config.book_default_activation_delay_slots = rules.default_activation_delay_slots;
    Ok(())
}

/// The first attach initializes the conditions account. A re-attach rewrites
/// the block in place. The function holds one borrow, because a freshly
/// initialized account's discriminator is not visible to a second load in the
/// same instruction.
fn write_market_crank_conditions(
    crank_conditions: &AccountLoader<ClobCrankConditionsV0>,
    keys: &crate::instructions::ClobCrankConditionKeys,
    market_index: u16,
    payments: CrankPaymentsV0,
    block: &ClobCrankBlockV0,
    min_cross_surplus: u64,
    expire_fallback_slots: u64,
    refill_watermark_lamports: u64,
) -> Result<()> {
    let mut conditions = crank_conditions
        .load_init()
        .or_else(|_| load_mut!(crank_conditions))?;
    conditions.write_crank_conditions(
        keys,
        market_index,
        payments,
        min_cross_surplus,
        expire_fallback_slots,
        refill_watermark_lamports,
    )?;

    // A Custom quoter's cross conditions watch this same region for a cross
    // against the book. `initialize_quoter_cross_conditions` reads the region
    // from here rather than deriving it.
    conditions.clob_block_offset = block.block_offset;
    conditions.top_of_book_offset = block.top_of_book_offset;
    conditions.top_of_book_len = block.top_of_book_len;
    // Write the reservoir's real balance now. The refill condition reads this
    // field. A mirror left at zero on an account that already holds lamports
    // keeps the condition due forever, while the resolver keeps answering that
    // there is no work.
    conditions.spendable_mirror =
        ClobCrankConditionsV0::spendable_lamports(&crank_conditions.to_account_info())?;
    Ok(())
}
