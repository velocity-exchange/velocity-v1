//! Shared plumbing for the CLOB crank instructions and their resolvers.
//!
//! Each crank endpoint lives in its own file next to its resolver
//! (`crank_clob_evict.rs`, `crank_clob_remove_expired.rs`,
//! `crank_cross_match.rs`). What they share lives here: the removal executor's
//! account struct and body, the resolvers' common account struct, and the
//! staging helpers. The evict and expire cranks differ only in which CLOB
//! removal they CPI and in what happens to a placed trigger's shadow slot.
//!
//! The cranks have two modes. The `filler` the caller passes selects one.
//!
//! - Signed keeper. The caller signs for its own filler `User` and earns the
//!   flat removal reward from the maker.
//!   No lamports move.
//! - Program keeper, used by relay. The filler is the protocol-owned `User`,
//!   whose authority is the velocity signer PDA that nobody can sign for. No
//!   signature is required, because relay turners submit executors unsigned.
//!   The maker's reward accrues to the protocol `User`. The caller's
//!   `authority` account is paid `keeper_payment_lamports` from the market's
//!   conditions-account reservoir instead, which gives relay's `assert_paid_v0`
//!   a real fee to measure.
//!
//! Resolvers are advisory. The executor verifies everything again, because the
//! CLOB fails removals that are not due and velocity fails the crank when the
//! removal hit a different maker. A stale or lying simulation therefore filters
//! itself out. Deriving PDAs costs real compute, and resolvers run only under
//! simulation.

use {
    crate::{
        controller::{orders::pay_keeper_flat_reward_for_perps, position::PositionDirection},
        error::ErrorCode,
        instructions::{constraints::*, relay_harness::StagedCall},
        load_mut, msg,
        state::{
            clob_crank::{ClobCrankConditionsV0, CrankPaymentsV0, CLOB_CRANK_CONDITIONS_PDA_SEED},
            events::OrderActionExplanation,
            pdas,
            perp_market::PerpMarket,
            prop_amm::{
                ClobMarket, ClobReader, DirectionV0, EvictWorstArgsV0, L3ArgsV0, L3RowV0,
                OrderViewV0, QuoterCpiScratch, QuoterSlabExt, QuoterSlabV0, QuoterSlotV0,
                QuoterType, RemoveExpiredArgsV0, RemovedOrderV0, UserRefV0,
            },
            signed_msg_user::release_removed_remainders,
            state::State,
            user::{OrderReservation, ReleaseCheck, User, UserStats},
        },
        validate,
    },
    anchor_lang::prelude::*,
};

/// Makers one profitable cross touches. The bound keeps the staged executor
/// inside the conditions account's scratch region. The walk stops before it
/// admits a maker past the cap, so the staged size covers staged makers only
/// and the executor's loaded-user set is always enough.
pub const MAX_CROSS_MAKERS: usize = 8;

#[derive(Accounts)]
pub struct CrankClobOrderRemoval<'info> {
    pub state: AccountLoader<'info, State>,
    /// CHECK: in signed-keeper mode this must sign for `filler`, which the
    /// constraint below enforces. In program-keeper mode it is only the lamport
    /// payout target, relay's `KEEPER_PLACEHOLDER` slot, and no signature is
    /// required.
    #[account(mut)]
    pub authority: UncheckedAccount<'info>,
    #[account(
        mut,
        constraint = can_crank_for_filler(&filler, &authority, &state)?
    )]
    pub filler: AccountLoader<'info, User>,
    #[account(
        mut,
        constraint = is_stats_for_user(&filler, &filler_stats)?
    )]
    pub filler_stats: AccountLoader<'info, UserStats>,
    /// The owner of the order the crank removes. Eviction removes the book's
    /// tail and expiry removes the hinted order. The CLOB's return data is
    /// checked against this account, so a race that removed someone else's
    /// order fails the whole crank.
    #[account(mut)]
    pub user: AccountLoader<'info, User>,
    #[account(mut, has_one = quoter_slab, has_one = clob_market)]
    pub perp_market: AccountLoader<'info, PerpMarket>,
    /// Not gated on active or approved, because a dead book still needs its
    /// resting orders reclaimed.
    pub quoter_slab: AccountLoader<'info, QuoterSlabV0>,
    /// CHECK: the perp market's `has_one` binds it to the book the market
    /// designated.
    #[account(mut)]
    pub clob_market: UncheckedAccount<'info>,
    /// CHECK: a Clob slot's program is pinned to velocity's CLOB at
    /// registration. The handler checks it again through the slot.
    #[account(address = crate::ids::clob_program::id())]
    pub clob_program: UncheckedAccount<'info>,
    /// The market's relay conditions account: the expiry-hint host and the
    /// lamport reservoir. It is optional so that a signed keeper can crank a
    /// market whose conditions were never initialized. Program-keeper mode
    /// requires it.
    #[account(
        mut,
        // Derived from the slab's market rather than from instruction args.
        // This struct serves both removal cranks, whose args differ.
        seeds = [
            CLOB_CRANK_CONDITIONS_PDA_SEED,
            quoter_slab.load()?.market.to_le_bytes().as_ref(),
        ],

        bump
    )]
    pub crank_conditions: Option<AccountLoader<'info, ClobCrankConditionsV0>>,
}

/// Which removal a crank runs. The two differ only in the CLOB call they make
/// and in what happens to a placed trigger's shadow slot. Eviction re-arms the
/// slot in the same transaction. Expiry frees it.
pub enum ClobRemoval {
    Evict(EvictWorstArgsV0),
    Expire(RemoveExpiredArgsV0),
}

impl ClobRemoval {
    fn invoke(self, clob: &ClobMarket) -> Result<RemovedOrderV0> {
        match self {
            ClobRemoval::Evict(args) => clob.evict(args),
            ClobRemoval::Expire(args) => clob.remove_expired(args),
        }
    }

    fn is_evict(&self) -> bool {
        matches!(self, ClobRemoval::Evict(_))
    }
}

/// The shared crank body. It CPIs the removal, checks that the removal hit the
/// passed maker, unwinds the aggregates, and pays the keeper. Both modes pay
/// quote from the maker. Program-keeper mode adds reservoir lamports.
pub fn crank_clob_removal(
    ctx: Context<CrankClobOrderRemoval>,
    market_index: u16,
    removal: ClobRemoval,
) -> Result<()> {
    let clock = Clock::get()?;
    let state = ctx.accounts.state.load()?;
    let program_keeper_mode = is_protocol_user(&ctx.accounts.filler, &ctx.accounts.state)?;
    validate!(
        !program_keeper_mode || ctx.accounts.crank_conditions.is_some(),
        ErrorCode::CrankConditionsAccountRequired,
        "program-keeper crank requires the market's conditions account"
    )?;

    let clob = ClobMarket::from_slab(
        &ctx.accounts.quoter_slab,
        market_index,
        &ctx.accounts.clob_market,
        &ctx.accounts.clob_program,
    )?;

    // CPI while no user borrows are held.
    let is_evict = removal.is_evict();
    let removed = removal.invoke(&clob)?;
    {
        let user = crate::load!(ctx.accounts.user)?;
        validate!(
            removed.user.authority == user.authority
                && removed.user.sub_account_id == user.sub_account_id,
            ErrorCode::InvalidUserAccount,
            "clob removed an order for {}/{} but the crank loaded {}",
            removed.user.authority,
            removed.user.sub_account_id,
            ctx.accounts.user.key()
        )?;
    }

    release_removed_remainders(ctx.remaining_accounts.first(), market_index, &[removed]);

    // Both removals charge the maker the flat removal reward before they
    // unwind. In program-keeper mode the filler is the protocol `User`. Unwinding an
    // otherwise-empty position frees its slot, and the reward needs that slot
    // to resolve.
    //
    // An eviction charges the same fee as an expiry, because a caller can
    // manufacture a free eviction. In program-keeper mode the crank pays the
    // caller reservoir lamports and needs no signature, so a caller that fills
    // a side to its threshold with its own dust and then evicts its own tail
    // draws lamports for nothing. The fee prices that loop, because every
    // iteration costs the evictee one flat reward and the evictee is the
    // caller. A free eviction also left an honest signed keeper no reason to
    // clear a full side.
    //
    // The book evicts the worst-priced order on its side that is not a bound
    // taker remainder, so the fee falls on a quote that no longer competes and
    // whose slot the book needs back. Pushing
    // an honest maker to that tail costs an attacker a full side of
    // better-priced, takeable quotes, each holding real margin.
    {
        let mut user = load_mut!(ctx.accounts.user)?;
        let mut market = load_mut!(ctx.accounts.perp_market)?;
        validate!(
            market.market_index == market_index,
            ErrorCode::PerpMarketAccountMismatch,
            "perp market {} passed for market {}",
            market.market_index,
            market_index
        )?;

        // A keeper that removes its own order is already loaded as the maker
        // and cannot be loaded a second time as the filler. It pays itself
        // nothing.
        let mut filler = (ctx.accounts.filler.key() != ctx.accounts.user.key())
            .then(|| load_mut!(ctx.accounts.filler))
            .transpose()?;
        let removal_fee = pay_keeper_flat_reward_for_perps(
            &mut user,
            filler.as_deref_mut(),
            &mut market,
            state.perp_fee_structure.flat_filler_fee,
            clock.slot,
        )?;

        drop(filler);

        // The removal report is the book's, so it is held to what velocity
        // reserved for this user rather than clamped to it. An over-report
        // would free the margin behind orders that still rest.
        let removed_order = OrderReservation::book_order(
            market_index,
            PositionDirection::from(removed.side),
            removed.base_asset_amount,
            removed.reduce_only,
        );

        // An evicted placed trigger re-arms with the unfilled remainder, behind
        // an edge gate on a price recross. Its slot takes the order back.
        let re_arms = is_evict
            && user
                .find_placed_trigger_slot(market_index, removed.order_id)
                .is_some();
        let position_index = if re_arms {
            user.re_arm_placed_trigger_slot(
                market_index,
                removed.order_id,
                removed.base_asset_amount,
                clock.slot,
            )?;
            user.replace_reservation(
                &removed_order,
                &OrderReservation::armed_trigger(market_index),
            )?
        } else {
            user.close_book_order(
                &removed_order,
                ReleaseCheck::HeldToReservation,
                removed.order_id,
                crate::state::user::OrderStatus::Canceled,
            )?
        };

        // An eviction reads differently from a cancel. The order left the book
        // because the book ran out of room, and a placed trigger re-arms rather
        // than ends. It therefore carries its own explanation.
        super::emit_clob_cancel_record(
            clock.unix_timestamp,
            market.market_stats.historical_oracle_data.last_oracle_price,
            &ctx.accounts.user.key(),
            super::ClobOrderFacts::from_removed(&removed, market_index, clock.slot),
            if is_evict {
                OrderActionExplanation::ClobOrderEvicted
            } else {
                OrderActionExplanation::OrderExpired
            },
            Some(ctx.accounts.filler.key()),
            Some(removal_fee),
            user.perp_positions[position_index].is_isolated(),
        )?;
    }

    if let Some(conditions_loader) = &ctx.accounts.crank_conditions {
        // An expiry that went unclaimed pays escalation, priced off the
        // order's own `max_ts` so a caller cannot name its own figure.
        // Eviction is a capacity limit, not a deadline, so it does not
        // escalate.
        let escalation = if is_evict {
            0
        } else {
            CrankPaymentsV0::expiry_escalation(removed.max_ts, clock.unix_timestamp)
        };
        let payment = u64::from(load_mut!(conditions_loader)?.crank_payments.removal)
            .saturating_add(u64::from(escalation));
        if program_keeper_mode {
            ClobCrankConditionsV0::pay_keeper(
                conditions_loader,
                &ctx.accounts.authority.to_account_info(),
                payment,
            )?;
        }
    }

    msg!(
        "cranked clob removal of order {} for user {}",
        removed.order_id,
        ctx.accounts.user.key()
    );

    Ok(())
}

/// The account order is the contract with `write_clob_crank_conditions`'s
/// registered `resolver_accounts`. The conditions account comes first, because
/// the response pointer says the payload lives at index 0.
#[derive(Accounts)]
pub struct ResolveClobCrank<'info> {
    /// The shared staging account, at index 0 by convention. A resolver's
    /// response pointer is read against it.
    #[account(mut, seeds = [crate::state::relay_scratch::RELAY_SCRATCH_PDA_SEED], bump)]
    pub scratch: AccountLoader<'info, crate::state::relay_scratch::RelayScratchV0>,
    /// Read-only, because resolvers stage into the shared scratch account
    /// rather than into the block they read.
    pub crank_conditions: AccountLoader<'info, ClobCrankConditionsV0>,
    /// CHECK: the slab's `has_one` binds it to the book the admin approved.
    /// It is writable for the book's response tail, scratch the book rewrites
    /// every quote as the cross resolver streams resting orders into it
    /// through `quote_l3_v0`. Nothing a resolver sends lands there.
    #[account(mut)]
    pub clob_market: UncheckedAccount<'info>,
    #[account(
        has_one = clob_market,
        constraint = quoter_slab.load()?.market == crank_conditions.load()?.market_index,
    )]
    pub quoter_slab: AccountLoader<'info, QuoterSlabV0>,
    pub state: AccountLoader<'info, State>,
    /// CHECK: a Clob slot's program is pinned to velocity's CLOB at
    /// registration. The linkage check verifies it again through the slot.
    #[account(address = crate::ids::clob_program::id())]
    pub clob_program: UncheckedAccount<'info>,
    /// Read-only. The refill resolver reads the levels a reservoir is held
    /// between, which the treasury sets rather than the market.
    #[account(
        seeds = [crate::state::crank_treasury::CRANK_TREASURY_PDA_SEED],
        bump
    )]
    pub treasury: AccountLoader<'info, crate::state::crank_treasury::CrankTreasuryV0>,
}

pub fn validate_linkage(ctx: &Context<ResolveClobCrank>) -> Result<()> {
    let market_index = ctx.accounts.crank_conditions.load()?.market_index;
    let slot = ctx.accounts.quoter_slab.clob_slot(market_index)?;
    slot.config
        .validate_clob_book(market_index, &ctx.accounts.clob_market.key())?;
    validate!(
        slot.config.program_id == ctx.accounts.clob_program.key(),
        ErrorCode::InvalidQuoterConfig,
        "clob program does not match the book slot"
    )?;

    Ok(())
}

/// The book, bound for the read-only questions a resolver asks it.
pub fn clob_reader<'a, 'info>(
    ctx: &'a Context<'_, ResolveClobCrank<'info>>,
) -> ClobReader<'a, 'info> {
    ClobReader {
        market: &ctx.accounts.clob_market,
        program: &ctx.accounts.clob_program,
    }
}

/// Derive the `(User, UserStats)` PDAs from a node's derivable identity. The
/// book stores `(authority, sub_account_id)` instead of the `User` key so that
/// this derivation is possible.
pub fn derive_user_pdas(user: &UserRefV0) -> (Pubkey, Pubkey) {
    pdas::user_pair(&user.authority, user.sub_account_id)
}

/// The protocol-owned `User` and its stats PDA. The `User` is the signer
/// authority's first sub-account, created through the normal `initialize_user`
/// path.
pub fn derive_protocol_user_pdas(signer: &Pubkey) -> (Pubkey, Pubkey) {
    pdas::user_pair(signer, 0)
}

/// The removal executor's call for the order `found` names.
/// `CrankClobOrderRemoval`'s `#[derive(Accounts)]` struct is the typed account
/// list. A taker-origin order adds its owner's signed-message record as the one
/// remaining account, so the removal releases the entry.
///
/// `I` names which executor the caller stages. `crank_clob_evict` and
/// `crank_clob_remove_expired` share this account list but are different
/// instructions. The executor identity comes back from the resolver rather than
/// out of the condition, so the resolver has to name it.
pub fn removal_call<I: anchor_lang::Discriminator>(
    ctx: &Context<ResolveClobCrank>,
    found: &OrderViewV0,
) -> Result<StagedCall> {
    let market_index = ctx.accounts.crank_conditions.load()?.market_index;
    let (protocol_user, protocol_user_stats) = pdas::protocol_user_pair();
    let call = StagedCall::new::<I>(crate::accounts::CrankClobOrderRemoval {
        state: ctx.accounts.state.key(),
        authority: pdas::keeper_placeholder(),
        filler: protocol_user,
        filler_stats: protocol_user_stats,
        user: derive_user_pdas(&found.user).0,
        perp_market: pdas::perp_market(market_index),
        quoter_slab: ctx.accounts.quoter_slab.key(),
        clob_market: ctx.accounts.clob_market.key(),
        clob_program: crate::ids::clob_program::id(),
        crank_conditions: Some(ctx.accounts.crank_conditions.key()),
    });

    if !found.taker_origin {
        return Ok(call);
    }

    Ok(call.account(pdas::signed_msg_user_orders(&found.user.authority), true))
}

/// The shared tail of the trigger cranks `trigger_order` and
/// `trigger_limit_order_v1`. It releases the fired slot on the user's relay
/// trigger conditions so that its level-triggered wake goes quiet. In
/// program-keeper mode it also pays the caller from the fired market's
/// reservoir.
#[allow(clippy::too_many_arguments)]
pub fn finish_trigger_crank<'info>(
    state: &AccountLoader<'info, State>,
    filler: &AccountLoader<'info, User>,
    authority: &UncheckedAccount<'info>,
    user: &AccountLoader<'info, User>,
    trigger_conditions: &Option<
        AccountLoader<'info, crate::state::user_conditions::UserConditionsV0>,
    >,
    crank_conditions: &Option<AccountLoader<'info, ClobCrankConditionsV0>>,
    market_index: u16,
    order_id: u32,
) -> Result<()> {
    if let Some(conditions) = trigger_conditions {
        let mut conditions = load_mut!(conditions)?;
        validate!(
            conditions.user == user.key(),
            ErrorCode::InvalidUserAccount,
            "trigger conditions are for user {}, crank is for {}",
            conditions.user,
            user.key()
        )?;

        conditions.release_slot(market_index, order_id);
    }

    let program_keeper_mode = is_protocol_user(filler, state)?;
    if program_keeper_mode {
        let reservoir = crank_conditions
            .as_ref()
            .ok_or_else(|| -> anchor_lang::error::Error {
                msg!("program-keeper trigger crank requires the market's conditions account");
                ErrorCode::CrankConditionsAccountRequired.into()
            })?;
        let payment = {
            let conditions = reservoir.load()?;
            validate!(
                conditions.market_index == market_index,
                ErrorCode::CrankConditionsMarketMismatch,
                "conditions are for market {}, the fired order is market {}",
                conditions.market_index,
                market_index
            )?;

            u64::from(conditions.crank_payments.trigger)
        };

        ClobCrankConditionsV0::pay_keeper(reservoir, &authority.to_account_info(), payment)?;
    }

    Ok(())
}

/// Which resolver a fired trigger slot belongs to.
///
/// The order type decides it: a trigger-limit rests whole on the book,
/// every other trigger fires and fills against it. Staging the wrong kind fails.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum TriggerResolverKind {
    /// `trigger_limit_order_v1` rests the whole trigger-limit on the book.
    ClobRest,
    /// `trigger_market_order_v1` fires the trigger and fills it against the
    /// book.
    ClobFill,
}

/// Whether a trigger crank on `order` can land at this oracle price.
///
/// A re-armed order (flagged `AwaitingTriggerRecross`) is due while unsatisfied. An
/// ordinary order is due while satisfied. The wrong half fails with `OrderAwaitingTriggerRecross`.
pub fn trigger_crank_is_due(order: &crate::state::user::Order, oracle_price: u64) -> Result<bool> {
    let satisfied = crate::math::orders::order_satisfies_trigger_condition(order, oracle_price)?;
    let awaiting_recross =
        order.is_bit_flag_set(crate::state::user::OrderBitFlag::AwaitingTriggerRecross);
    Ok(satisfied != awaiting_recross)
}

/// The first armed trigger order on `market` whose crank can land now and whose
/// synced slot matches the resolver's executor path `want`.
///
/// An ordinary order is due when the oracle satisfies its condition. An order
/// re-armed after an eviction is due when the oracle does not satisfy it,
/// because the crank that clears its edge gate is the one that observes the
/// recross. The executor checks everything again, so a stale sync or a moved
/// price returns `None` and the turner backs off.
pub fn find_fired_trigger(
    conditions: &crate::state::user_conditions::UserConditionsV0,
    user: &User,
    market: &PerpMarket,
    oracle_info: &AccountInfo,
    slot: u64,
    now: i64,
    want: TriggerResolverKind,
) -> Result<Option<crate::state::user_conditions::TriggerSlotMetaV0>> {
    validate!(
        oracle_info.key() == market.oracle,
        ErrorCode::InvalidOracle,
        "oracle {} is not market {}'s oracle",
        oracle_info.key(),
        market.market_index
    )?;

    // The executors judge the median trigger price when `State` sets the flag.
    // A resolver cannot read `State`, so an order due at either price is
    // staged, and the executor's simulation refuses the wrong one.
    let oracle_price =
        crate::state::oracle::get_oracle_price(&market.oracle_source, oracle_info, slot)?.price;
    let raw_price = oracle_price.max(0) as u64;
    let median_price = market
        .get_trigger_price(oracle_price, now, true)
        .unwrap_or(raw_price);

    for order in user.orders.iter() {
        // A trigger slot already placed on the CLOB reads as untriggered, so
        // `triggered()` does not exclude it. Staging it anyway makes
        // `trigger_limit_order_v1` reject the crank with `OrderPlacedOnClob`,
        // which spends turner work and starves triggers behind it. An order
        // past its own `max_ts` is exempt from `should_expire_order` too, and stays armed the same way.
        let expired = order.max_ts != 0 && now > order.max_ts;
        if order.status != crate::state::user::OrderStatus::Open
            || order.market_index != market.market_index
            || order.is_placed_on_clob()
            || !order.must_be_triggered()
            || order.triggered()
            || expired
        {
            continue;
        }

        if !trigger_crank_is_due(order, raw_price)? && !trigger_crank_is_due(order, median_price)? {
            continue;
        }

        let Some(meta) = conditions
            .trigger_slots
            .iter()
            .find(|meta| meta.market_index == order.market_index && meta.order_id == order.order_id)
            .copied()
        else {
            continue;
        };

        // A synced slot always names a book: `sync_trigger_conditions` refuses
        // to stage a trigger on a market with no CLOB, because such a trigger
        // has nowhere to fire.
        if meta.quoter_slab == Pubkey::default() {
            continue;
        }

        let kind = if order.order_type == crate::state::user::OrderType::TriggerLimit {
            TriggerResolverKind::ClobRest
        } else {
            TriggerResolverKind::ClobFill
        };

        if kind == want {
            return Ok(Some(meta));
        }
    }

    Ok(None)
}

/// Rows read from one side when measuring whether its depth has rested. The
/// window covers the size any one crank takes off a side. A crossing size that
/// needs more rows than this reports unrested, because the rows past the window
/// were never measured.
pub(crate) const RESTED_ROWS_PER_SIDE: u16 = 32;

/// Whether every book order a fill could consume has measurably rested.
///
/// This is how a crank vouches for protected flow (`taker_served_window`)
///
/// This measures book slots and nothing else. A `Custom` quoter prices during
/// the call and keeps no resting order, so it will always be false.
pub(crate) fn book_side_rested<'info>(
    quoter_slab: &AccountLoader<'info, QuoterSlabV0>,
    tail: &'info [AccountInfo<'info>],
    market_index: u16,
    direction: DirectionV0,
    size: u64,
    slot: u64,
    cpi_scratch: &mut QuoterCpiScratch<'info>,
) -> Result<bool> {
    let consulted = quoter_slab.consulted_slots(tail)?;
    consulted
        .iter()
        .try_fold(true, |served, &slot_index| -> Result<bool> {
            // Copy the slot out so that no slab borrow lives across the book
            // CPI.
            let quoter_slot = quoter_slab.slots()?[slot_index];
            match quoter_slot.config.quoter_type {
                QuoterType::Vamm => Ok(served),
                QuoterType::Custom => Ok(false),
                QuoterType::Clob => {
                    let rows = book_l3_side(
                        &quoter_slot,
                        quoter_slab,
                        market_index,
                        direction,
                        RESTED_ROWS_PER_SIDE,
                        tail,
                        cpi_scratch,
                        false,
                        |row| (row.size, row.placed_slot),
                    )?
                    .unwrap_or_default();
                    Ok(served && rows_rested(&rows, size, slot))
                }
            }
        })
}

/// Whether the first `size` base of one side has measurably rested, over the
/// `(size, placed_slot)` rows the read returned.
///
/// Unmeasured depth counts as unrested. The read stops at
/// [`RESTED_ROWS_PER_SIDE`], so a side that filled the window without reaching
/// `size` hides rows the fill can still sweep, and those rows may be fresh.
/// A read short of the window is the whole side. Depth below `size` there does not exist.
pub(crate) fn rows_rested(rows: &[(u64, u64)], size: u64, slot: u64) -> bool {
    let (depth, rested) =
        rows.iter()
            .fold((0u64, true), |(depth, rested), (row_size, placed_slot)| {
                if depth >= size {
                    return (depth, rested);
                }

                (
                    depth.saturating_add(*row_size),
                    rested && crate::math::crosses::served_window(*placed_slot, slot),
                )
            });
    rested && (depth >= size || rows.len() < RESTED_ROWS_PER_SIDE as usize)
}

/// One side of a book, best price first, through the same `quote_l3_v0` every
/// source answers on. Each row is mapped in place, because `map` copies what it
/// keeps and the response then needs no owned intermediate on a heap that never
/// reclaims.
///
/// A `size` of 0 describes the side up to `max_rows`. A cross is found by
/// comparing the two sides, so neither has a size to stop at until the other has
/// been read. Reading short costs throughput and nothing else, because rows come
/// best price first and the edge truncates worse prices rather than a better
/// counterparty. Returns `None` when the entry declares no L3 leg.
pub(crate) fn book_l3_side<'info, T>(
    quoter: &QuoterSlotV0,
    slab: &AccountLoader<'info, QuoterSlabV0>,
    market_index: u16,
    direction: DirectionV0,
    max_rows: u16,
    accounts: &[AccountInfo<'info>],
    scratch: &mut QuoterCpiScratch<'info>,
    include_taker_origin_reservations: bool,
    map: impl Fn(&L3RowV0) -> T,
) -> Result<Option<Vec<T>>> {
    let located = quoter.quote_l3(
        market_index,
        L3ArgsV0 {
            direction,
            size: 0,
            max_rows,
            include_taker_origin_reservations,
        },
        slab,
        accounts,
        scratch,
    )?;
    let Some(located) = located else {
        return Ok(None);
    };
    let data = located.borrow()?;
    Ok(Some(
        located.l3_response(&data)?.rows.iter().map(map).collect(),
    ))
}

/// Both sides of a book as `(bids, asks)`, in the form a cross resolution works
/// from.
///
/// It takes two calls, one per side, because both responses land in the same
/// region of the book's response tail. The first is copied out before the second
/// CPI overwrites it. A cross needs both sides, because what crosses an order is
/// on the other side, and a reader that saw one side could not tell a resolvable
/// cross from a stuck one. A taker of `Long` sweeps asks, so that read names the
/// ask side. Returns `None` when the entry declares no L3 leg.
#[allow(clippy::type_complexity)]
pub(crate) fn book_l3_sides<'info>(
    quoter: &QuoterSlotV0,
    slab: &AccountLoader<'info, QuoterSlabV0>,
    market_index: u16,
    max_rows: u16,
    accounts: &[AccountInfo<'info>],
    scratch: &mut QuoterCpiScratch<'info>,
    include_taker_origin_reservations: bool,
) -> Result<
    Option<(
        Vec<crate::math::crosses::RestingOrder>,
        Vec<crate::math::crosses::RestingOrder>,
    )>,
> {
    let from_row = crate::math::crosses::RestingOrder::from_row;
    let Some(asks) = book_l3_side(
        quoter,
        slab,
        market_index,
        DirectionV0::Long,
        max_rows,
        accounts,
        scratch,
        include_taker_origin_reservations,
        from_row,
    )?
    else {
        return Ok(None);
    };

    let Some(bids) = book_l3_side(
        quoter,
        slab,
        market_index,
        DirectionV0::Short,
        max_rows,
        accounts,
        scratch,
        include_taker_origin_reservations,
        from_row,
    )?
    else {
        return Ok(None);
    };

    Ok(Some((bids, asks)))
}

#[cfg(test)]
mod rows_rested_tests {
    use super::{rows_rested, RESTED_ROWS_PER_SIDE};

    /// A row rests once `SERVED_WINDOW_MIN_SLOTS` have passed since it was
    /// placed. Slot 100 against slot 0 is rested. Against slot 100 it is not.
    const NOW: u64 = 100;

    #[test]
    fn measured_depth_that_covers_the_size_and_rested_reports_rested() {
        let rows = vec![(50u64, 0u64), (50, 0)];
        assert!(rows_rested(&rows, 100, NOW));
    }

    #[test]
    fn a_fresh_row_inside_the_size_reports_unrested() {
        let rows = vec![(50u64, 0u64), (50, NOW)];
        assert!(!rows_rested(&rows, 100, NOW));
    }

    #[test]
    fn a_fresh_row_past_the_size_is_not_measured() {
        // The fill stops at 50 base, so the fresh row behind it is depth the
        // fill does not reach.
        let rows = vec![(50u64, 0u64), (50, NOW)];
        assert!(rows_rested(&rows, 50, NOW));
    }

    #[test]
    fn a_full_read_window_short_of_the_size_reports_unrested() {
        // The read filled its window without reaching the size, so rows past it
        // were never measured.
        let rows = vec![(1u64, 0u64); RESTED_ROWS_PER_SIDE as usize];
        assert!(!rows_rested(&rows, 1_000, NOW));
    }

    #[test]
    fn a_short_read_short_of_the_size_is_the_whole_side() {
        // The read came back under its window, so the side holds nothing more.
        let rows = vec![(1u64, 0u64); RESTED_ROWS_PER_SIDE as usize - 1];
        assert!(rows_rested(&rows, 1_000, NOW));
    }

    #[test]
    fn an_empty_side_rests() {
        assert!(rows_rested(&[], 1_000, NOW));
    }
}

#[cfg(test)]
mod trigger_crank_is_due_tests {
    use {
        super::trigger_crank_is_due,
        crate::state::user::{Order, OrderBitFlag, OrderTriggerCondition},
    };

    fn stop_below(trigger_price: u64, awaiting_recross: bool) -> Order {
        Order {
            trigger_condition: OrderTriggerCondition::Below,
            trigger_price,
            bit_flags: if awaiting_recross {
                OrderBitFlag::AwaitingTriggerRecross as u8
            } else {
                0
            },
            ..Order::default()
        }
    }

    #[test]
    fn an_ordinary_trigger_is_due_when_the_price_satisfies_it() {
        let order = stop_below(100, false);
        assert!(trigger_crank_is_due(&order, 99).unwrap());
        assert!(!trigger_crank_is_due(&order, 101).unwrap());
    }

    #[test]
    fn a_re_armed_trigger_is_due_only_when_the_price_crossed_back() {
        // The crank that clears the edge gate is the one that observes the
        // price off the trigger side. While the price stays through the trigger
        // the crank cannot land, so there is no work to stage.
        let order = stop_below(100, true);
        assert!(!trigger_crank_is_due(&order, 99).unwrap());
        assert!(trigger_crank_is_due(&order, 101).unwrap());
    }
}
