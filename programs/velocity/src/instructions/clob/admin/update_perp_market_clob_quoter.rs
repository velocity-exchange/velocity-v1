//! Attach (or re-price) a market's canonical CLOB quoter: name the book's
//! slab entry as the mandatory route baseline, mirror its placement rules,
//! and stand up the market's relay crank conditions and keeper reservoir.

use {
    crate::{
        auth::check_warm,
        error::ErrorCode,
        instructions::constraints::perp_market_valid,
        load_mut, msg,
        state::{
            clob_crank::{CrankCostUnitsV0, CrankPaymentsV0},
            perp_market::PerpMarket,
            prop_amm::QuoterSlabExt,
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
    /// Writable: the attach mirrors the book's placement rules onto the
    /// staging entry, so a later re-approval copies them forward.
    #[account(mut)]
    pub quoter: AccountLoader<'info, crate::state::prop_amm::QuoterV0>,
    /// Writable: the attach mirrors the book's placement rules onto the
    /// approved copy in the book's slot, so the hot paths read a loaded
    /// field instead of CPI'ing `order_rules_v0`. Bound by the market's
    /// `has_one`.
    #[account(mut)]
    pub quoter_slab: AccountLoader<'info, crate::state::prop_amm::QuoterSlabV0>,
    /// CHECK: the perp market's `has_one` binds it to the book the market
    /// designated. Writable because the attach registers velocity's
    /// resolvers on the book itself: the wakes for an expiry, an activation,
    /// a side at its cap and a crossed book are facts about this account, so
    /// the conditions that watch for them live on it.
    #[account(mut)]
    pub clob_market: UncheckedAccount<'info>,
    /// CHECK: a Clob slot's program is pinned to velocity's CLOB at
    /// registration; the handler re-checks through the slot.
    #[account(address = crate::ids::clob_program::id())]
    pub clob_program: UncheckedAccount<'info>,
    /// The market's relay conditions + keeper reservoir, stood up (or
    /// re-priced) as part of the attach so a new market needs no separate
    /// crank ceremony.
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
    /// Read-only: the levels a reservoir is held between are the treasury's
    /// setting, and the low one is resolved onto this market here.
    #[account(
        seeds = [crate::state::crank_treasury::CRANK_TREASURY_PDA_SEED],
        bump
    )]
    pub treasury: AccountLoader<'info, crate::state::crank_treasury::CrankTreasuryV0>,
    pub rent: Sysvar<'info, Rent>,
    pub system_program: Program<'info, System>,
}

/// Name the market's canonical CLOB quoter entry: once set, every router
/// fill must carry it in its quoter section (mandatory baseline — a route
/// can't exclude the public book). A dead entry is still passed but skipped
/// at quote time, so deactivating the book never bricks fills; there is no
/// clear path for the same reason — kill the entry instead.
///
/// The attach also stands up (or, on re-attach, rewrites) the market's relay
/// crank conditions: the evict/expire condition block plus the lamport
/// reservoir that pays relay keepers per crank. This is the earliest point the
/// full reference graph (book + registry entry) exists, so a new market needs
/// no separate conditions ceremony, and re-pricing the cranks is just
/// re-running the attach.
///
/// `crank_cost_units` is what each crank requests, measured by simulating it.
/// The lamport payments are derived here from `State.transaction_fee_rails`,
/// so a change in what the network charges is one write to the rails plus a
/// re-run of this instruction per market — not a fresh round of guesswork per
/// market.
#[derive(Clone, Copy, AnchorSerialize, AnchorDeserialize)]
pub struct UpdatePerpMarketClobQuoterArgs {
    /// What each crank requests, measured by simulating it. The lamport
    /// payments derive from these and `State.transaction_fee_rails`.
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

    // Binds the slab's book slot to this market and to the passed book
    // account; the two identity checks below are the ones it does not run.
    let clob = crate::state::prop_amm::ClobMarket::from_slab(
        &ctx.accounts.quoter_slab,
        perp_market.market_index,
        &ctx.accounts.clob_market,
        &ctx.accounts.clob_program,
    )?;
    let clob_program = {
        let book_slot = ctx
            .accounts
            .quoter_slab
            .clob_slot(perp_market.market_index)?;
        validate!(
            book_slot.entry == ctx.accounts.quoter.key(),
            ErrorCode::DefaultError,
            "the slab's book slot holds entry {}, not the passed one",
            book_slot.entry
        )?;
        book_slot.config.program_id
    };
    validate!(
        clob_program == ctx.accounts.clob_program.key(),
        ErrorCode::DefaultError,
        "clob program does not match the quoter entry"
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

    // A cross has to clear by more than nothing. Cranking one pays
    // `crank_payments.cross` out of the reservoir, so a cross that clears by a
    // cent is a cross the protocol pays to run — and anyone can manufacture one
    // by resting two orders a tick apart. The figure has to be stated because
    // this instruction cannot derive it: converting the lamport payout into
    // quote needs a SOL price, and the cross crank carries no SOL oracle.
    validate!(
        min_cross_surplus > 0,
        ErrorCode::DefaultError,
        "min_cross_surplus must cover what the reservoir pays for a cross"
    )?;

    // The book's own floor on a resting order has to sit at or under the
    // market's. A fill unwinds a culled remainder by releasing its base from
    // the maker's open-order aggregate, and the size of that release is the
    // book's word — bounded on the fill path by the market's minimum, which is
    // only a bound at all while the book cannot cull something larger.
    {
        let rules = clob.reader().order_rules()?;
        // A market with no minimum of its own has nothing to bound against, and
        // nothing to bound: the book's cull fires on a remainder under *its*
        // minimum, and releasing that from an aggregate the market never
        // reserved against is a no-op.
        validate!(
            perp_market.market_stats.min_order_size == 0
                || rules.min_order_size <= perp_market.market_stats.min_order_size,
            ErrorCode::DefaultError,
            "book minimum order size {} is above the market's {}",
            rules.min_order_size,
            perp_market.market_stats.min_order_size
        )?;
        // The book's place authority is its trust root: it settles for whoever
        // it names as a maker, and velocity signs its CPIs as this key. Pin it
        // to the market's quoter slab PDA — the one identity velocity signs
        // every external quoter CPI as — so a book whose place authority is a
        // stranger cannot be attached and, through it, forge orders for any
        // loaded user. place_authority is immutable on the book, so a book that
        // passes here stays pinned for the life of the attachment.
        validate!(
            rules.place_authority == ctx.accounts.quoter_slab.key().to_bytes(),
            ErrorCode::DefaultError,
            "book place authority is not the market's quoter slab"
        )?;
        // The book's grid must match the market's. A remainder aligned to the
        // market can then always rest; an off-tick or off-step remainder would
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
        // Mirror the placement rules onto the book's slot (the copy the
        // take gate, the route's maker-priority skip and the remainder rest
        // read instead of CPI'ing `order_rules_v0` per fill) and onto the
        // staging entry, so a later re-approval copies them forward. The
        // attach is the supported way to change an attached book's rules, so
        // this write is where the mirror stays current.
        {
            let mut slots = ctx.accounts.quoter_slab.slots_mut()?;
            let index = crate::state::prop_amm::clob_slot_index(&slots)
                .ok_or_else(|| error!(ErrorCode::QuoterNotOnSlab))?;
            slots[index].config.book_tick_size = rules.tick_size;
            slots[index].config.book_min_order_size = rules.min_order_size;
            slots[index].config.book_default_activation_delay_slots =
                rules.default_activation_delay_slots;
        }
        let mut quoter = ctx.accounts.quoter.load_mut()?;
        quoter.config.book_tick_size = rules.tick_size;
        quoter.config.book_min_order_size = rules.min_order_size;
        quoter.config.book_default_activation_delay_slots = rules.default_activation_delay_slots;
    }

    // The book's own conditions first: velocity registers which resolver
    // answers each one and what it pays, and the book keeps their wakes
    // current. What comes back — where its condition block sits, and which of
    // its bytes change when its top of book moves — is reported rather than
    // derived, so nothing here knows the market account's layout.
    let block = clob.set_crank_conditions(crate::instructions::clob_crank_registration(
        &keys, payments,
    )?)?;
    msg!(
        "clob crank conditions at book offset {}",
        block.block_offset
    );

    // The wake level is the treasury's setting resolved against this market's
    // dearest crank. Resolved here rather than at refill time because it is
    // the threshold the condition carries, and a condition holds its own.
    let refill_watermark_lamports = ctx
        .accounts
        .treasury
        .load()?
        .refill_watermark(payments.max_payment())?;

    // First attach initializes the conditions account; a re-attach rewrites
    // the block in place. One borrow: a freshly initialized account's
    // discriminator is not visible to a second load in the same instruction.
    {
        let mut conditions = ctx
            .accounts
            .crank_conditions
            .load_init()
            .or_else(|_| load_mut!(ctx.accounts.crank_conditions))?;
        conditions.write_crank_conditions(
            &keys,
            perp_market.market_index,
            payments,
            min_cross_surplus,
            expire_fallback_slots,
            refill_watermark_lamports,
        )?;
        // A Custom quoter's cross conditions watch this same region for a
        // cross against the book (`initialize_quoter_cross_conditions`), and
        // read it from here rather than deriving it.
        conditions.clob_block_offset = block.block_offset;
        conditions.top_of_book_offset = block.top_of_book_offset;
        conditions.top_of_book_len = block.top_of_book_len;
        // State the reservoir's real balance now. The refill condition reads
        // this field, and a mirror left at zero on an account that already
        // holds lamports keeps the condition permanently due while the
        // resolver keeps answering that there is no work.
        conditions.spendable_mirror = ctx
            .accounts
            .crank_conditions
            .to_account_info()
            .lamports()
            .saturating_sub(
                Rent::get()?.minimum_balance(crate::state::clob_crank::ClobCrankConditionsV0::SIZE),
            );
    }

    // A market names its book once, at registration (`initialize_quoter`),
    // and the accounts struct's `has_one = clob_market` holds this attach to
    // that designation. Nothing here writes it: a book settles for whoever
    // rests on it, so a path that could point the market at a second one
    // later would put every user a fill carries behind whoever holds the
    // admin key.
    Ok(())
}
