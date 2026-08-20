//! Shared plumbing for the CLOB crank instructions and their resolvers.
//!
//! Each crank endpoint lives in its own file next to its resolver
//! (`crank_clob_evict.rs`, `crank_clob_remove_expired.rs`,
//! `crank_cross_match.rs`); what they share lives here: the removal
//! executor's account struct + body (evict and expire differ only in which
//! CLOB removal they CPI and what happens to a placed trigger's shadow
//! slot), the resolvers' common account struct, and the staging helpers.
//!
//! The cranks are dual-mode, selected by which `filler` rides the call:
//!
//! - **Signed keeper** (today's path): the caller signs for its own filler
//!   `User` and earns the flat removal reward from the maker, mirroring DLOB
//!   order-expiry cranks. No lamports move.
//! - **Program keeper** (relay): the filler is the protocol-owned `User`
//!   (authority = the velocity signer PDA, which nobody can sign for), so no
//!   signature is required — relay turners submit executors unsigned. The
//!   maker's reward accrues to the protocol `User`, and the caller's
//!   `authority` account is instead paid `keeper_payment_lamports` from the
//!   market's conditions-account reservoir, giving relay's `assert_paid_v0`
//!   a real fee to measure.
//!
//! Resolvers are advisory: the executor re-verifies everything (the CLOB
//! fails removals that aren't due, and velocity fails the crank if the
//! removal hit a different maker), so a stale or lying simulation filters
//! itself out. Deriving PDAs costs real CU but resolvers only ever run under
//! simulation.

use {
    crate::{
        controller::{
            orders::pay_keeper_flat_reward_for_perps,
            position::{decrease_open_bids_and_asks, get_position_index},
        },
        error::ErrorCode,
        instructions::{constraints::*, relay_harness::StagedCall},
        load_mut, msg,
        signer::QUOTER_SIGNER_SEED,
        state::{
            clob_crank::{ClobCrankConditionsV0, CLOB_CRANK_CONDITIONS_PDA_SEED},
            pdas,
            perp_market::PerpMarket,
            prop_amm::{
                clob_hint_scan, read_clob_node, ClobEvictWorstArgsV0, ClobMarket, ClobNodeView,
                ClobRemoveExpiredArgsV0, ClobRemovedOrderV0, ClobUserRefV0, QuoterV0,
                WireDirectionExt, CLOB_NIL,
            },
            state::State,
            user::{User, UserStats},
        },
        validate,
    },
    anchor_lang::prelude::*,
};

/// Makers one profitable cross touches, bounded so the staged executor
/// stays inside the conditions account's scratch region. The walk stops
/// before admitting a maker past the cap, so the staged size only covers
/// staged makers and the executor's loaded-user set is always sufficient.
pub const MAX_CROSS_MAKERS: usize = 8;

#[derive(Accounts)]
#[instruction(market_index: u16)]
pub struct CrankClobOrderRemoval<'info> {
    pub state: AccountLoader<'info, State>,
    /// CHECK: in signed-keeper mode this must sign for `filler` (enforced by
    /// the constraint below); in program-keeper mode it is only the lamport
    /// payout target — relay's `KEEPER_PLACEHOLDER` slot — and no signature
    /// is required.
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
    /// The owner of the order being removed (the book's tail for evict, the
    /// hinted order for expire). Verified against the CLOB's return data —
    /// a race that removed someone else's order fails the whole crank.
    #[account(mut)]
    pub user: AccountLoader<'info, User>,
    #[account(mut)]
    pub perp_market: AccountLoader<'info, PerpMarket>,
    /// Deliberately not gated on active/approved: dead books still need
    /// their resting orders reclaimed.
    pub quoter: AccountLoader<'info, QuoterV0>,
    /// CHECK: validated against the quoter entry's registered execute
    /// accounts in the handler.
    #[account(mut)]
    pub clob_market: UncheckedAccount<'info>,
    /// CHECK: locked to the registered quoter program.
    #[account(address = quoter.load()?.program_id)]
    pub clob_program: UncheckedAccount<'info>,
    /// CHECK: the quoter CPI signer PDA — what a book's `place_authority` is
    /// set to. Deliberately not the vault authority: signer privilege is
    /// inherited by a callee, so the key velocity hands an external program
    /// must be the authority on nothing.
    #[account(seeds = [QUOTER_SIGNER_SEED], bump)]
    pub quoter_signer: UncheckedAccount<'info>,
    /// The market's relay conditions account: the expiry-hint host and the
    /// lamport reservoir. Optional so signed keepers can crank markets whose
    /// conditions were never initialized; required in program-keeper mode.
    #[account(
        mut,
        seeds = [
            CLOB_CRANK_CONDITIONS_PDA_SEED,
            market_index.to_le_bytes().as_ref(),
        ],
        bump
    )]
    pub crank_conditions: Option<AccountLoader<'info, ClobCrankConditionsV0>>,
}

/// Which removal a crank is: the two differ only in the CLOB call they make
/// and in what happens to a placed trigger's shadow slot — eviction re-arms
/// it (eager, in this same tx), expiry frees it.
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

/// Shared crank body: CPI the removal, verify it hit the passed maker,
/// unwind the aggregates, pay the keeper — quote from the maker in both
/// modes, plus reservoir lamports in program-keeper mode.
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

    let clob = ClobMarket::from_quoter(
        &*ctx.accounts.quoter.load()?,
        market_index,
        &ctx.accounts.clob_market,
        &ctx.accounts.clob_program,
        &ctx.accounts.quoter_signer,
        ctx.bumps.quoter_signer,
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

    // Pay the keeper from the maker first (the same flat reward DLOB order
    // expiry pays; in program-keeper mode the filler is the protocol User),
    // THEN unwind — unwinding an otherwise-empty position frees its slot,
    // and the reward needs to resolve it.
    {
        let mut user = load_mut!(ctx.accounts.user)?;
        let mut filler = load_mut!(ctx.accounts.filler)?;
        let mut market = load_mut!(ctx.accounts.perp_market)?;
        validate!(
            market.market_index == market_index,
            ErrorCode::DefaultError,
            "perp market {} passed for market {}",
            market.market_index,
            market_index
        )?;
        pay_keeper_flat_reward_for_perps(
            &mut user,
            Some(&mut filler),
            &mut market,
            state.perp_fee_structure.flat_filler_fee,
            clock.slot,
        )?;

        let position_index = get_position_index(&user.perp_positions, market_index)?;
        decrease_open_bids_and_asks(
            &mut user.perp_positions[position_index],
            &removed.side.to_position_direction(),
            removed.base_asset_amount,
            true,
        )?;
        user.perp_positions[position_index].open_orders = user.perp_positions[position_index]
            .open_orders
            .saturating_sub(1);
        user.decrement_open_orders(false);

        // A placed trigger's shadow slot follows its CLOB order: eviction
        // re-arms it (with the unfilled remainder, edge-gated on a price
        // recross), expiry frees it.
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
    }

    if let Some(conditions_loader) = &ctx.accounts.crank_conditions {
        // Repair both wake hints against the post-removal book in one scan,
        // so a due hint goes quiet once its work is gone.
        let (min_expiry, min_activation) =
            clob_hint_scan(&ctx.accounts.clob_market.try_borrow_data()?, clock.slot);
        let payment = {
            let mut conditions = load_mut!(conditions_loader)?;
            conditions.repair_expiry(min_expiry)?;
            conditions.repair_activation(min_activation)?;
            conditions.keeper_payment_lamports
        };
        if program_keeper_mode {
            let conditions_info = conditions_loader.to_account_info();
            let rent_minimum = Rent::get()?.minimum_balance(conditions_info.data_len());
            ClobCrankConditionsV0::pay_keeper_lamports(
                &conditions_info,
                &ctx.accounts.authority.to_account_info(),
                payment,
                rent_minimum,
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

/// Account order is the contract with `write_clob_crank_conditions`'s
/// registered `resolver_accounts` — the conditions account first (index 0 is
/// where the response pointer says the payload lives).
#[derive(Accounts)]
pub struct ResolveClobCrank<'info> {
    /// The shared staging account, index 0 by convention — a resolver's
    /// response pointer is interpreted against it.
    #[account(mut, seeds = [crate::state::relay_scratch::RELAY_SCRATCH_PDA_SEED], bump)]
    pub scratch: AccountLoader<'info, crate::state::relay_scratch::RelayScratchV0>,
    /// Read-only: resolvers stage into the shared scratch account, not
    /// into the block they read.
    pub crank_conditions: AccountLoader<'info, ClobCrankConditionsV0>,
    /// CHECK: validated against the quoter entry's registered execute
    /// accounts, same as the executor it stages.
    pub clob_market: UncheckedAccount<'info>,
    pub quoter: AccountLoader<'info, QuoterV0>,
    pub state: AccountLoader<'info, State>,
}

pub fn validate_linkage(ctx: &Context<ResolveClobCrank>) -> Result<()> {
    let quoter = ctx.accounts.quoter.load()?;
    let conditions = ctx.accounts.crank_conditions.load()?;
    quoter.validate_clob_book(conditions.market_index, &ctx.accounts.clob_market.key())?;
    Ok(())
}

/// Derive the `(User, UserStats)` PDAs from a node's derivable identity —
/// the whole point of the book storing `(authority, sub_account_id)`
/// instead of the `User` key.
pub fn derive_user_pdas(user: &ClobUserRefV0) -> (Pubkey, Pubkey) {
    pdas::user_pair(&user.authority, user.sub_account_id)
}

/// The protocol-owned `User` (the signer authority's first sub-account,
/// created through the normal initialize_user path) and its stats PDA.
pub fn derive_protocol_user_pdas(signer: &Pubkey) -> (Pubkey, Pubkey) {
    pdas::user_pair(signer, 0)
}

/// Advance a cursor to the next node that is live and matchable right now.
pub fn next_matchable(
    data: &[u8],
    mut cursor: u32,
    slot: u64,
    now: i64,
) -> Option<(u32, ClobNodeView)> {
    while cursor != CLOB_NIL {
        let node = read_clob_node(data, cursor)?;
        if node.is_matchable(slot, now) {
            return Some((cursor, node));
        }
        cursor = node.next;
    }
    None
}

/// The removal executor's call: `CrankClobOrderRemoval`'s full account
/// list is its `#[derive(Accounts)]` struct (no remaining accounts), so
/// the whole thing is typed.
///
/// `I` names which executor the caller is staging — `crank_clob_evict` and
/// `crank_clob_remove_expired` share this account list but are different
/// instructions, and since the executor identity now comes back from the
/// resolver rather than out of the condition, the resolver has to say which.
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
            quoter: ctx.accounts.quoter.key(),
            clob_market: ctx.accounts.clob_market.key(),
            clob_program: ctx.accounts.quoter.load()?.program_id,
            quoter_signer: pdas::quoter_signer(),
            crank_conditions: Some(ctx.accounts.crank_conditions.key()),
        },
    ))
}

/// Shared tail of the trigger cranks (`trigger_order`,
/// `trigger_clob_order`): release the fired slot on the user's relay
/// trigger conditions so its level-triggered wake goes quiet, and in
/// program-keeper mode pay the caller from the fired market's reservoir.
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
            conditions.keeper_payment_lamports
        };
        let info = reservoir.to_account_info();
        let rent_minimum = Rent::get()?.minimum_balance(info.data_len());
        ClobCrankConditionsV0::pay_keeper_lamports(
            &info,
            &authority.to_account_info(),
            payment,
            rent_minimum,
        )?;
    }
    Ok(())
}

/// Discovery shared by the trigger resolvers: the first armed trigger order
/// on `market` whose condition the oracle satisfies right now and whose
/// synced slot matches the resolver's executor path (`want_clob_path`).
/// Everything is re-verified — a stale sync or moved price just returns
/// `None` and the turner backs off.
pub fn find_fired_trigger(
    conditions: &crate::state::user_conditions::UserConditionsV0,
    user: &User,
    market: &PerpMarket,
    oracle_info: &AccountInfo,
    slot: u64,
    want_clob_path: bool,
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
        // work: it deliberately reads as *untriggered* so every DLOB matching
        // path ignores it, which means the `triggered()` test below does not
        // exclude it. Without this, discovery keeps re-firing an order that is
        // already resting on the book — `trigger_clob_order` then rejects the
        // staged crank with `OrderPlacedOnClob` every round, burning turner
        // work and starving genuinely armed triggers behind it.
        if order.status != crate::state::user::OrderStatus::Open
            || order.market_index != market.market_index
            || order.is_placed_on_clob()
            || !order.must_be_triggered()
            || order.triggered()
        {
            continue;
        }
        if !crate::math::orders::order_satisfies_trigger_condition(order, oracle_price)? {
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
        let is_clob_path = meta.quoter != Pubkey::default();
        if is_clob_path == want_clob_path {
            return Ok(Some(meta));
        }
    }
    Ok(None)
}
