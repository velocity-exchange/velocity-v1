//! Attach a market's canonical CLOB quoter, or re-price one that is already
//! attached. The instruction names the book's slab entry as the mandatory
//! route baseline, mirrors the book's placement rules, and writes the market's
//! relay crank conditions and keeper reservoir.

use {
    crate::{
        auth::check_warm,
        error::ErrorCode,
        instructions::constraints::perp_market_valid,
        load_mut,
        math::safe_math::SafeMath,
        msg,
        state::{
            clob_crank::{ClobCrankConditionsV0, CrankCostUnitsV0, CrankPaymentsV0},
            perp_market::PerpMarket,
            prop_amm::{ClobMarket, CrankBlockV0, QuoterSlabExt, QuoterSlabV0, QuoterV0},
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
        ErrorCode::InvalidQuoterConfig,
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
pub(super) fn bind_book_slot<'a, 'info>(
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
            ErrorCode::QuoterNotOnSlab,
            "the slab's book slot holds entry {}, not the passed one",
            book_slot.entry
        )?;

        book_slot.config.program_id
    };

    validate!(
        registered_program == clob_program.key(),
        ErrorCode::InvalidQuoterConfig,
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
pub(super) fn mirror_book_placement_rules(
    clob: &ClobMarket<'_, '_>,
    perp_market: &PerpMarket,
    quoter_slab: &AccountLoader<QuoterSlabV0>,
    quoter: &AccountLoader<QuoterV0>,
) -> Result<()> {
    let rules = clob.reader().order_rules()?;
    validate_book_fits_market(&rules, perp_market)?;

    // The book's place authority is its trust root. Velocity signs every
    // external quoter CPI as the market's quoter slab PDA, so only a book
    // pinned to that PDA can be attached without forging orders. Immutable
    // on the book, so a passing book stays pinned for the attachment's life.
    validate!(
        rules.place_authority == quoter_slab.key().to_bytes(),
        ErrorCode::InvalidQuoterConfig,
        "book place authority is not the market's quoter slab"
    )?;

    // The slab must also hold the book's config authority. Every rule change
    // then runs through `update_perp_market_clob_book_config`, which refreshes
    // the mirror below in the same instruction.
    validate!(
        rules.authority == quoter_slab.key().to_bytes(),
        ErrorCode::InvalidQuoterConfig,
        "book config authority is not the market's quoter slab"
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

pub const BOOK_BLOCKING_FLOOR_MIN_ORDERS: u64 = 10;

/// Hold the book's size floors and grid to the market's.
///
/// The book's minimum must sit at or under the market's, because the fill
/// bounds a culled remainder's release by the market's minimum. A market with
/// no minimum of its own has nothing to bound. The book's tick and step must
/// equal the market's, so a remainder aligned to the market can always rest.
/// An off-tick or off-step remainder reverts the whole fill that carried it.
///
/// The blocking floor is at least [`BOOK_BLOCKING_FLOOR_MIN_ORDERS`] minimum
/// orders. A walk ends at an order at or over the floor whose owner the caller
/// does not carry, and a caller carries at most 48 owners. So 49 such orders
/// keep the book out of every fill, and each of them locks ten orders of margin.
pub(crate) fn validate_book_fits_market(
    rules: &crate::state::prop_amm::OrderRulesV0,
    perp_market: &PerpMarket,
) -> Result<()> {
    let min_order_size = rules
        .min_order_size
        .max(perp_market.market_stats.min_order_size);
    validate!(
        rules.blocking_min_size >= min_order_size.safe_mul(BOOK_BLOCKING_FLOOR_MIN_ORDERS)?,
        ErrorCode::InvalidQuoterConfig,
        "book blocking floor {} is under {} minimum orders of {}",
        rules.blocking_min_size,
        BOOK_BLOCKING_FLOOR_MIN_ORDERS,
        min_order_size
    )?;
    validate!(
        perp_market.market_stats.min_order_size == 0
            || rules.min_order_size <= perp_market.market_stats.min_order_size,
        ErrorCode::InvalidQuoterConfig,
        "book minimum order size {} is above the market's {}",
        rules.min_order_size,
        perp_market.market_stats.min_order_size
    )?;
    validate!(
        rules.tick_size == perp_market.order_tick_size,
        ErrorCode::InvalidQuoterConfig,
        "book tick {} does not match the market tick {}",
        rules.tick_size,
        perp_market.order_tick_size
    )?;
    validate!(
        rules.step_size == perp_market.order_step_size,
        ErrorCode::InvalidQuoterConfig,
        "book step {} does not match the market step {}",
        rules.step_size,
        perp_market.order_step_size
    )?;

    Ok(())
}

/// Hold a perp market's changed grid or minimum to the book attached to it.
///
/// A market that designated a book passes its quoter slab as the first
/// remaining account, and the book and the CLOB program after it once the
/// book is attached. An attached book mirrors a non-zero tick onto its slot.
pub fn validate_attached_book_grid<'info>(
    perp_market: &PerpMarket,
    remaining_accounts: &'info [AccountInfo<'info>],
) -> Result<()> {
    if perp_market.clob_market == Pubkey::default() {
        return Ok(());
    }

    let slab_info = remaining_accounts
        .first()
        .filter(|info| info.key() == perp_market.quoter_slab)
        .ok_or_else(|| {
            msg!("market designated a book; pass its quoter slab as the first remaining account");
            error!(ErrorCode::InvalidQuoterConfig)
        })?;

    let quoter_slab = AccountLoader::<QuoterSlabV0>::try_from(slab_info)?;
    let book_attached = quoter_slab
        .clob_slot(perp_market.market_index)
        .is_ok_and(|slot| slot.config.book_tick_size != 0);
    if !book_attached {
        return Ok(());
    }

    let [_, clob_market, clob_program, ..] = remaining_accounts else {
        msg!("market has an attached book; pass the book and the clob program after the slab");
        return Err(ErrorCode::InvalidQuoterConfig.into());
    };

    validate!(
        clob_program.key() == crate::ids::clob_program::id(),
        ErrorCode::InvalidQuoterConfig,
        "clob program {} is not velocity's clob",
        clob_program.key()
    )?;

    let clob = ClobMarket::from_slab(
        &quoter_slab,
        perp_market.market_index,
        clob_market,
        clob_program,
    )?;

    validate_book_fits_market(&clob.reader().order_rules()?, perp_market)
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
    block: &CrankBlockV0,
    min_cross_surplus: u64,
    expire_fallback_slots: u64,
    refill_watermark_lamports: u64,
) -> Result<()> {
    // Read before the loader borrow, because the read borrows the data too.
    let spendable_lamports =
        ClobCrankConditionsV0::spendable_lamports(&crank_conditions.to_account_info())?;

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
    conditions.spendable_mirror = spendable_lamports;
    Ok(())
}

#[cfg(test)]
mod book_grid_tests {
    use {
        super::{validate_attached_book_grid, validate_book_fits_market},
        crate::{error::ErrorCode, state::perp_market::PerpMarket},
        anchor_lang::prelude::Pubkey,
    };

    fn rules() -> crate::state::prop_amm::OrderRulesV0 {
        crate::state::prop_amm::OrderRulesV0 {
            min_order_size: 100,
            blocking_min_size: 5_000,
            default_activation_delay_slots: 0,
            max_activation_delay_slots: 0,
            place_authority: [0; 32],
            tick_size: 10,
            step_size: 100,
            side_order_counts: [0, 0],
            arena_capacity: 512,
            evict_threshold_per_side: 200,
            authority: [0; 32],
        }
    }

    fn market(min_order_size: u64, tick_size: u64, step_size: u64) -> PerpMarket {
        let mut market = PerpMarket::default_test();
        market.market_stats.min_order_size = min_order_size;
        market.order_tick_size = tick_size;
        market.order_step_size = step_size;
        market
    }

    #[test]
    fn a_market_that_matches_its_book_passes() {
        assert!(validate_book_fits_market(&rules(), &market(100, 10, 100)).is_ok());
        assert!(validate_book_fits_market(&rules(), &market(500, 10, 100)).is_ok());
    }

    /// A market minimum under the book's lets a fill cull a remainder that the
    /// market's minimum does not bound.
    #[test]
    fn a_market_minimum_under_the_books_is_refused() {
        assert_eq!(
            validate_book_fits_market(&rules(), &market(99, 10, 100)).unwrap_err(),
            ErrorCode::InvalidQuoterConfig.into()
        );
    }

    /// The floor counts orders of the larger of the book's and the market's minimum.
    #[test]
    fn a_blocking_floor_under_ten_minimum_orders_is_refused() {
        let mut rules = rules();
        rules.blocking_min_size = 999;
        assert_eq!(
            validate_book_fits_market(&rules, &market(0, 10, 100)).unwrap_err(),
            ErrorCode::InvalidQuoterConfig.into()
        );

        rules.blocking_min_size = 1_000;
        assert!(validate_book_fits_market(&rules, &market(0, 10, 100)).is_ok());
        assert!(validate_book_fits_market(&rules, &market(500, 10, 100)).is_err());
    }

    #[test]
    fn a_grid_off_the_books_is_refused() {
        assert!(validate_book_fits_market(&rules(), &market(100, 20, 100)).is_err());
        assert!(validate_book_fits_market(&rules(), &market(100, 10, 50)).is_err());
    }

    #[test]
    fn a_market_with_no_designated_book_has_nothing_to_check() {
        assert!(validate_attached_book_grid(&market(1, 1, 1), &[]).is_ok());
    }

    /// The admin change cannot skip the book by leaving its accounts out.
    #[test]
    fn a_market_with_a_designated_book_must_pass_it() {
        let mut market = market(1, 1, 1);
        market.clob_market = Pubkey::new_unique();
        assert_eq!(
            validate_attached_book_grid(&market, &[]).unwrap_err(),
            ErrorCode::InvalidQuoterConfig.into()
        );
    }
}
