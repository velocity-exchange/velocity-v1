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
        controller::{
            orders::pay_keeper_flat_reward_for_perps,
            position::{
                get_position_index, release_reserved_open_base, release_reserved_open_orders,
            },
        },
        error::ErrorCode,
        instructions::{constraints::*, relay_harness::StagedCall},
        load_mut, msg,
        state::{
            clob_crank::{ClobCrankConditionsV0, CrankPaymentsV0, CLOB_CRANK_CONDITIONS_PDA_SEED},
            events::OrderActionExplanation,
            pdas,
            perp_market::PerpMarket,
            prop_amm::{
                ClobEvictWorstArgsV0, ClobMarket, ClobReader, ClobRemoveExpiredArgsV0,
                ClobRemovedOrderV0, ClobUserRefV0, Direction, L3ArgsV0, L3RowV0, QuoterConfigV0,
                QuoterCpiScratch, QuoterSlabExt, QuoterSlabV0, QuoterType, WireDirectionExt,
            },
            state::State,
            user::{User, UserStats},
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
    Evict(ClobEvictWorstArgsV0),
    Expire(ClobRemoveExpiredArgsV0),
}

impl ClobRemoval {
    fn invoke(self, clob: &ClobMarket) -> Result<ClobRemovedOrderV0> {
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
        ErrorCode::DefaultError,
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
            ErrorCode::DefaultError,
            "clob removed an order for {}/{} but the crank loaded {}",
            removed.user.authority,
            removed.user.sub_account_id,
            ctx.accounts.user.key()
        )?;
    }

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
    // The book evicts the worst-priced order on its side, so the fee falls on a
    // quote that no longer competes and whose slot the book needs back. Pushing
    // an honest maker to that tail costs an attacker a full side of
    // better-priced, takeable quotes, each holding real margin.
    {
        let mut user = load_mut!(ctx.accounts.user)?;
        let mut market = load_mut!(ctx.accounts.perp_market)?;
        validate!(
            market.market_index == market_index,
            ErrorCode::DefaultError,
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

        // The removal report is the book's, so its size and its slot are held
        // to what velocity reserved for this user rather than clamped to it. An
        // over-report would free the margin behind orders that still rest.
        let position_index = get_position_index(&user.perp_positions, market_index)?;
        release_reserved_open_base(
            &mut user.perp_positions[position_index],
            &removed.side.to_position_direction(),
            removed.base_asset_amount,
        )?;
        release_reserved_open_orders(&mut user.perp_positions[position_index], 1)?;
        user.decrement_open_orders(false);
        // The order left the book, so disarm the reduce-only counter it armed.
        if removed.reduce_only {
            user.perp_positions[position_index].disarm_reduce_only_clob();
        }

        // A placed trigger's shadow slot follows its CLOB order. Eviction
        // re-arms it with the unfilled remainder, behind an edge gate on a
        // price recross. Expiry frees it.
        if is_evict {
            user.re_arm_placed_trigger_slot(
                market_index,
                removed.order_id,
                removed.base_asset_amount,
                clock.slot,
            )?;
        } else {
            user.release_placed_trigger_slot(
                market_index,
                removed.order_id,
                crate::state::user::OrderStatus::Canceled,
            );
        }

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
        // An expiry that went unclaimed pays for the wait. The escalation is
        // priced off the order's own `max_ts`, which the removal reports, so
        // the figure is the protocol's and a caller cannot name its own.
        // Eviction answers a capacity limit rather than a deadline, so it does
        // not escalate.
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
    ///
    /// It is writable for the book's response tail. The cross resolver asks the
    /// book for its resting orders through `quote_l3_v0`, which streams the
    /// answer into that tail. Nothing a resolver sends ever lands, and the tail
    /// is a scratch region the book rewrites on every quote.
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
        ErrorCode::DefaultError,
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
pub fn derive_user_pdas(user: &ClobUserRefV0) -> (Pubkey, Pubkey) {
    pdas::user_pair(&user.authority, user.sub_account_id)
}

/// The protocol-owned `User` and its stats PDA. The `User` is the signer
/// authority's first sub-account, created through the normal `initialize_user`
/// path.
pub fn derive_protocol_user_pdas(signer: &Pubkey) -> (Pubkey, Pubkey) {
    pdas::user_pair(signer, 0)
}

/// The removal executor's call. `CrankClobOrderRemoval`'s `#[derive(Accounts)]`
/// struct is its full account list, with no remaining accounts, so the whole
/// call is typed.
///
/// `I` names which executor the caller stages. `crank_clob_evict` and
/// `crank_clob_remove_expired` share this account list but are different
/// instructions. The executor identity comes back from the resolver rather than
/// out of the condition, so the resolver has to name it.
pub fn removal_call<I: anchor_lang::Discriminator>(
    ctx: &Context<ResolveClobCrank>,
    maker: Pubkey,
) -> Result<StagedCall> {
    let market_index = ctx.accounts.crank_conditions.load()?.market_index;
    let (protocol_user, protocol_user_stats) = pdas::protocol_user_pair();
    Ok(StagedCall::new::<I>(
        crate::accounts::CrankClobOrderRemoval {
            state: ctx.accounts.state.key(),
            authority: pdas::keeper_placeholder(),
            filler: protocol_user,
            filler_stats: protocol_user_stats,
            user: maker,
            perp_market: pdas::perp_market(market_index),
            quoter_slab: ctx.accounts.quoter_slab.key(),
            clob_market: ctx.accounts.clob_market.key(),
            clob_program: crate::ids::clob_program::id(),
            crank_conditions: Some(ctx.accounts.crank_conditions.key()),
        },
    ))
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
            ErrorCode::DefaultError,
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
                ErrorCode::DefaultError.into()
            })?;
        let payment = {
            let conditions = reservoir.load()?;
            validate!(
                conditions.market_index == market_index,
                ErrorCode::DefaultError,
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
/// A user can hold several fired triggers on one market at once, and each
/// resolver runs for its own slots only. The order type names the resolver: a
/// trigger-limit rests whole on the book, and every other trigger fires and
/// fills against it. Without this split, a resolver would stage the first
/// fired order of either kind and its executor would then reject it.
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
/// An evicted trigger comes back armed behind an edge gate, which is
/// [`crate::state::user::OrderBitFlag::AwaitingTriggerRecross`]. The crank that
/// clears the flag is the one that observes the price back off the trigger
/// side. A crank that observes the price still through the trigger fails with
/// `OrderAwaitingTriggerRecross`. A re-armed order is therefore due while its
/// condition is not satisfied, and an ordinary order is due while its condition
/// is satisfied. Staging the other half lands nothing and starves fired
/// triggers behind it.
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
        ErrorCode::DefaultError,
        "oracle {} is not market {}'s oracle",
        oracle_info.key(),
        market.market_index
    )?;
    let oracle_price =
        crate::state::oracle::get_oracle_price(&market.oracle_source, oracle_info, slot)?
            .price
            .max(0) as u64;

    for order in user.orders.iter() {
        // A trigger slot already placed on the CLOB is a shadow, not armed
        // work. It reads as untriggered, which means the `triggered()` test
        // below does not exclude it. Without this test, discovery keeps re-firing an order that is
        // already resting on the book. `trigger_limit_order_v1` then rejects the
        // staged crank with `OrderPlacedOnClob` every round, which spends turner
        // work and starves armed triggers behind it.
        //
        // An order past its own `max_ts` starves the queue the same way.
        // `should_expire_order` exempts anything that must be triggered, so
        // the sweep never takes it and it stays armed forever. Both fire paths
        // treat it as no work, so staging it spends a round and accomplishes
        // nothing.
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
        if !trigger_crank_is_due(order, oracle_price)? {
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
    direction: Direction,
    size: u64,
    slot: u64,
    cpi_scratch: &mut QuoterCpiScratch<'info>,
) -> Result<bool> {
    let consulted = quoter_slab.consulted_slots(tail)?;
    consulted
        .iter()
        .try_fold(true, |served, &slot_index| -> Result<bool> {
            // Copy the config out so that no slab borrow lives across the book
            // CPI.
            let config = quoter_slab.slots()?[slot_index].config;
            match config.quoter_type {
                QuoterType::Vamm => Ok(served),
                QuoterType::Custom => Ok(false),
                QuoterType::Clob => {
                    let rows = book_l3_side(
                        &config,
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
/// [`RESTED_ROWS_PER_SIDE`], so a side whose read filled the window without
/// reaching `size` hides rows the fill can still sweep, and those rows may be
/// fresh. A read that came back short of the window is the whole side, and
/// depth below `size` there does not exist.
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
    quoter: &QuoterConfigV0,
    slab: &AccountLoader<'info, QuoterSlabV0>,
    market_index: u16,
    direction: Direction,
    max_rows: u16,
    accounts: &[AccountInfo<'info>],
    scratch: &mut QuoterCpiScratch<'info>,
    consume_reservation: bool,
    map: impl Fn(&L3RowV0) -> T,
) -> Result<Option<Vec<T>>> {
    let located = quoter.quote_l3(
        market_index,
        L3ArgsV0 {
            direction,
            size: 0,
            max_rows,
            consume_reservation,
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
    quoter: &QuoterConfigV0,
    slab: &AccountLoader<'info, QuoterSlabV0>,
    market_index: u16,
    max_rows: u16,
    accounts: &[AccountInfo<'info>],
    scratch: &mut QuoterCpiScratch<'info>,
    consume_reservation: bool,
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
        Direction::Long,
        max_rows,
        accounts,
        scratch,
        consume_reservation,
        from_row,
    )?
    else {
        return Ok(None);
    };
    let Some(bids) = book_l3_side(
        quoter,
        slab,
        market_index,
        Direction::Short,
        max_rows,
        accounts,
        scratch,
        consume_reservation,
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
