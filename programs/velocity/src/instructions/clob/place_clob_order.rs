//! Place a resting limit order on a registered CLOB. Velocity owns placement
//! policy: it verifies the `User` authority, reserves the order's worst-case
//! open-order aggregates, and gates margin exactly like a DLOB placement —
//! the CLOB trusts its `place_authority` (the quoter CPI signer PDA) and only
//! enforces book-level rules (tick/step/min, capacity, activation delay).

use {
    crate::{
        controller::position::{
            add_new_position, get_position_index, increase_open_bids_and_asks, PositionDirection,
        },
        error::ErrorCode,
        instructions::{
            constraints::*,
            optional_accounts::{load_maps, AccountMaps},
        },
        load_mut,
        math::{margin::meets_place_order_margin_requirement, orders::is_order_position_reducing},
        msg,
        signer::QUOTER_SIGNER_SEED,
        state::{
            market_status::MarketStatus,
            perp_market_map::MarketSet,
            prop_amm::{ClobMarket, ClobPlaceOrderArgsV0, ClobSide, QuoterV0},
            state::State,
            user::User,
        },
        validate,
    },
    anchor_lang::prelude::*,
};

#[derive(Accounts)]
#[instruction(params: PlaceClobOrderParams)]
pub struct PlaceClobOrder<'info> {
    pub state: AccountLoader<'info, State>,
    #[account(
        mut,
        constraint = can_sign_for_user(&user, &authority)?
    )]
    pub user: AccountLoader<'info, User>,
    pub authority: Signer<'info>,
    /// The CLOB's registry entry for this market — placement is only allowed
    /// on a vetted book.
    pub quoter: AccountLoader<'info, QuoterV0>,
    /// CHECK: validated against the quoter entry's registered execute
    /// accounts in the handler (the vetted CPI surface names the book).
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
    /// CHECK: the instructions sysvar, locked by address. Required only for
    /// a faster-than-default activation delay: the handler introspects it
    /// for the flow-authority co-signer (the attestation).
    #[account(address = solana_program::sysvar::instructions::ID)]
    pub instructions_sysvar: Option<UncheckedAccount<'info>>,
}

#[derive(Clone, Copy, AnchorSerialize, AnchorDeserialize)]
pub struct PlaceClobOrderParams {
    pub market_index: u16,
    /// Long rests as a bid, Short as an ask.
    pub direction: PositionDirection,
    pub price: u64,
    pub base_asset_amount: u64,
    /// 0 = good-till-cancelled.
    pub max_ts: i64,
    /// None = the CLOB market's default speed bump. Anything below the
    /// default requires the flow-authority attestation (the transaction
    /// co-signed by `State.hot_flow_authority`, introspected off the
    /// instructions sysvar); the CLOB clamps to its max.
    pub activation_delay_slots: Option<u32>,
    /// Refuse the placement when the order would cross the opposite best
    /// price, rather than resting it crossed. What a post-only order asks for.
    ///
    /// It is not what makes the order a maker. A CLOB order always fills at
    /// its own price on the maker fee schedule — a router taker takes it
    /// there, and a crossed pair settles through the cross crank, which runs
    /// the protocol `User` as the taker on both legs. This is about the order
    /// resting at all: a maker that quotes through the other side has
    /// mispriced and would rather place nothing.
    pub reject_if_crossed: bool,
}

#[access_control(
    exchange_not_paused(&ctx.accounts.state)
)]
pub fn handle_place_clob_order<'c: 'info, 'info>(
    ctx: Context<'info, PlaceClobOrder<'info>>,
    params: PlaceClobOrderParams,
) -> Result<()> {
    let clock = Clock::get()?;
    let state = ctx.accounts.state.load()?;

    let mut remaining_accounts = ctx.remaining_accounts.iter().peekable();
    let AccountMaps {
        perp_market_map,
        spot_market_map,
        mut oracle_map,
    } = load_maps(
        &mut remaining_accounts,
        &MarketSet::new(),
        &MarketSet::new(),
        clock.slot,
        Some(state.oracle_guard_rails),
    )?;

    let clob = {
        let quoter = ctx.accounts.quoter.load()?;
        validate!(
            quoter.is_active && quoter.is_approved,
            ErrorCode::DefaultError,
            "CLOB quoter is not active and approved"
        )?;
        ClobMarket::from_quoter(
            &quoter,
            params.market_index,
            &ctx.accounts.clob_market,
            &ctx.accounts.clob_program,
            &ctx.accounts.quoter_signer,
            ctx.bumps.quoter_signer,
        )?
    };
    {
        let market = perp_market_map.get_ref(&params.market_index)?;
        validate!(
            matches!(market.status, MarketStatus::Active),
            ErrorCode::MarketPlaceOrderPaused,
            "market not active"
        )?;
    }

    // The speed bump is the taker protection that replaced JIT; skipping it
    // is reserved for attested flow — a transaction the flow authority
    // (swift) co-signed after serving the hold window off-chain. Anything
    // at-or-above the book's default needs no attestation.
    if let Some(requested) = params.activation_delay_slots {
        let default_delay = clob.reader().order_rules()?.default_activation_delay_slots;
        if requested < default_delay {
            let flow_authority = state.hot_key(crate::state::state::HotRole::FlowAuthority);
            validate!(
                flow_authority != Pubkey::default(),
                ErrorCode::UnattestedFastActivation,
                "no flow authority is configured; fast activation is disabled"
            )?;
            let sysvar = ctx.accounts.instructions_sysvar.as_ref().ok_or_else(|| {
                msg!("fast activation needs the instructions sysvar for attestation");
                ErrorCode::UnattestedFastActivation
            })?;
            validate!(
                crate::instructions::optional_accounts::tx_co_signed_by(sysvar, &flow_authority)?,
                ErrorCode::UnattestedFastActivation,
                "activation delay {} is below the default {} and the transaction is not \
                 co-signed by the flow authority",
                requested,
                default_delay
            )?;
        }
    }

    // CPI the placement. Identity travels in the args in derivable form —
    // the book stores (authority, sub_account_id), not the User key, so
    // off-chain readers can derive every user-hung PDA from a node.
    let side = match params.direction {
        PositionDirection::Long => ClobSide::Bid,
        PositionDirection::Short => ClobSide::Ask,
    };
    // The order's id comes from the `User`'s own counter, the one that numbers
    // its DLOB orders, so a client names every order it owns the same way
    // wherever the order rests. The book stores it and reports it back on
    // every answer, which is what keeps the two id spaces from ever needing a
    // map between them.
    let (user_ref, client_order_id) = {
        let mut user = load_mut!(ctx.accounts.user)?;
        let client_order_id = crate::get_then_update_id!(user, next_order_id);
        (
            crate::state::prop_amm::ClobUserRefV0 {
                authority: user.authority,
                sub_account_id: user.sub_account_id.into(),
            },
            client_order_id,
        )
    };
    // The CLOB returns the new order's ref; it stays the transaction's return
    // data (clients persist it as the cancel hint) and is decoded here so a
    // malformed response fails the placement.
    let order_ref = clob.place(ClobPlaceOrderArgsV0 {
        side,
        price: params.price,
        base_asset_amount: params.base_asset_amount,
        activation_delay_slots: params.activation_delay_slots,
        max_ts: params.max_ts,
        user: user_ref,
        // A placement whose price its owner chose, not a migrated remainder.
        taker_origin: false,
        client_order_id,
        reject_if_crossed: params.reject_if_crossed,
    })?;

    // Reserve the worst-case aggregates, then gate margin exactly like a
    // DLOB placement (initial margin in the risk scope when risk-increasing,
    // maintenance otherwise). Failure unwinds the CPI with the tx.
    let mut user = load_mut!(ctx.accounts.user)?;
    validate!(
        !user.is_bankrupt(),
        ErrorCode::UserBankrupt,
        "user bankrupt"
    )?;
    let position_index = get_position_index(&user.perp_positions, params.market_index)
        .or_else(|_| add_new_position(&mut user.perp_positions, params.market_index))?;
    let risk_increasing = !is_order_position_reducing(
        &params.direction,
        params.base_asset_amount,
        user.perp_positions[position_index].base_asset_amount,
    )?;
    validate!(
        user.perp_positions[position_index].open_orders < u8::MAX,
        ErrorCode::MaxNumberOfOrders,
        "position open order count at max"
    )?;
    increase_open_bids_and_asks(
        &mut user.perp_positions[position_index],
        &params.direction,
        params.base_asset_amount,
        true,
    )?;
    user.perp_positions[position_index].open_orders += 1;
    user.increment_open_orders(false);
    user.update_last_active_slot(clock.slot);

    let isolated_market_index = (risk_increasing
        && user.perp_positions[position_index].is_isolated())
    .then_some(params.market_index);
    meets_place_order_margin_requirement(
        &user,
        &perp_market_map,
        &spot_market_map,
        &mut oracle_map,
        risk_increasing,
        isolated_market_index,
    )?;

    drop(user);
    super::emit_clob_place_record(
        clock.unix_timestamp,
        &ctx.accounts.user.key(),
        super::ClobOrderFacts {
            order_id: client_order_id,
            market_index: params.market_index,
            direction: params.direction,
            price: params.price,
            base_asset_amount: params.base_asset_amount,
            base_asset_amount_filled: 0,
            max_ts: params.max_ts,
            slot: clock.slot,
            taker_origin: false,
        },
    )?;

    msg!(
        "placed clob order {} (node {}) for user {}",
        order_ref.order_id,
        order_ref.node_index,
        ctx.accounts.user.key()
    );
    Ok(())
}

/// The price an unfilled remainder can rest at on the book, or `None` when it
/// cannot rest at all.
///
/// One rule for both fill routes, because a remainder that rests on one and
/// not the other is a remainder whose fate depends on which keeper reached it.
/// Once the DLOB is gone, "kept DLOB behaviour" is not a fallback — it is an
/// order nothing will ever fill again.
///
/// A `Market` order's own `price` is zero; its bound lives in
/// `auction_end_price`, the worst fill it already agreed to. That is the only
/// price it can rest at, and resting there is safe *because* a migrated
/// remainder is taker-origin: it cannot be taken while a live counterparty
/// crosses it, and a cross settles at the counterparty's price, so a maker
/// that lines up during the activation window competes on price rather than
/// on transaction landing. Without that protection this would be a free
/// option written at the taker's own worst price.
///
/// Everything else stays behind. An oracle-floating price has nothing fixed
/// to rest at, reduce-only has no meaning on the book, and a trigger has its
/// own placement path.
pub fn restable_remainder_price(order: &crate::state::user::Order) -> Option<u64> {
    use crate::state::user::{OrderStatus, OrderType};
    if order.status != OrderStatus::Open || order.has_oracle_price_offset() || order.reduce_only {
        return None;
    }
    let price = match order.order_type {
        OrderType::Limit => order.price,
        OrderType::Market => order.auction_end_price.max(0).unsigned_abs(),
        _ => return None,
    };
    (price != 0).then_some(price)
}

/// Rest an unfilled `place_and_take` remainder on the CLOB: if it can rest
/// and be matched, it lives on the book, not in `User.orders`. Degrades
/// gracefully — a dead quoter entry or a failed margin re-reserve returns
/// `Ok(false)` (the remainder stays cancelled, the fill stands) instead of
/// reverting the whole place-and-take. Only a hard-cap placement rejection on
/// the CLOB side reverts, which is the documented ops-failure state.
///
/// Reached only from `place_and_take_perp_order_v1` — the v0 instruction has
/// no CLOB accounts to pass.
#[allow(clippy::too_many_arguments)]
pub fn try_place_remainder_on_clob<'info>(
    user_loader: &AccountLoader<'info, User>,
    quoter_loader: &AccountLoader<'info, QuoterV0>,
    clob_market: &AccountInfo<'info>,
    clob_program: &AccountInfo<'info>,
    quoter_signer: &AccountInfo<'info>,
    quoter_signer_nonce: u8,
    perp_market_map: &crate::state::perp_market_map::PerpMarketMap,
    spot_market_map: &crate::state::spot_market_map::SpotMarketMap,
    oracle_map: &mut crate::state::oracle_map::OracleMap,
    market_index: u16,
    direction: PositionDirection,
    price: u64,
    base_asset_amount: u64,
    max_ts: i64,
    // The id of the order this remainder came off. It carries across so the
    // order keeps one identity through the migration: the same id names it in
    // the records before and the records after.
    client_order_id: u32,
    clock: &Clock,
) -> Result<bool> {
    let clob = {
        let quoter = quoter_loader.load()?;
        let clob = ClobMarket::from_quoter(
            &quoter,
            market_index,
            clob_market,
            clob_program,
            quoter_signer,
            quoter_signer_nonce,
        )?;
        if !(quoter.is_active && quoter.is_approved) {
            msg!("clob quoter inactive; remainder stays cancelled");
            return Ok(false);
        }
        clob
    };

    // Reserve the worst-case aggregates and re-run the placement margin
    // gate BEFORE the CPI, so a failure can skip resting (remainder stays
    // cancelled) rather than unwind external state.
    let user_ref = {
        let mut user = load_mut!(user_loader)?;
        if user.is_bankrupt() {
            return Ok(false);
        }
        let position_index = get_position_index(&user.perp_positions, market_index)
            .or_else(|_| add_new_position(&mut user.perp_positions, market_index))?;
        if user.perp_positions[position_index].open_orders == u8::MAX {
            return Ok(false);
        }
        let risk_increasing = !is_order_position_reducing(
            &direction,
            base_asset_amount,
            user.perp_positions[position_index].base_asset_amount,
        )?;
        increase_open_bids_and_asks(
            &mut user.perp_positions[position_index],
            &direction,
            base_asset_amount,
            true,
        )?;
        user.perp_positions[position_index].open_orders += 1;
        user.increment_open_orders(false);

        let isolated_market_index = (risk_increasing
            && user.perp_positions[position_index].is_isolated())
        .then_some(market_index);
        if meets_place_order_margin_requirement(
            &user,
            perp_market_map,
            spot_market_map,
            oracle_map,
            risk_increasing,
            isolated_market_index,
        )
        .is_err()
        {
            crate::controller::position::decrease_open_bids_and_asks(
                &mut user.perp_positions[position_index],
                &direction,
                base_asset_amount,
                true,
            )?;
            user.perp_positions[position_index].open_orders = user.perp_positions[position_index]
                .open_orders
                .saturating_sub(1);
            user.decrement_open_orders(false);
            msg!("remainder fails the placement margin gate; stays cancelled");
            return Ok(false);
        }
        user.update_last_active_slot(clock.slot);
        crate::state::prop_amm::ClobUserRefV0 {
            authority: user.authority,
            sub_account_id: user.sub_account_id.into(),
        }
    };

    // CPI the placement (identity travels in the args; the user account is
    // not lent, so holding no borrow is not even required — kept dropped
    // for symmetry with the main placement path).
    let side = match direction {
        PositionDirection::Long => ClobSide::Bid,
        PositionDirection::Short => ClobSide::Ask,
    };
    let order_ref = clob.place(ClobPlaceOrderArgsV0 {
        side,
        price,
        base_asset_amount,
        activation_delay_slots: None,
        max_ts,
        user: user_ref,
        // The whole point of this path: the book must know this order is a
        // migrated taker remainder, so it cannot be taken while a live
        // counterparty crosses it and a cross settles at that
        // counterparty's price.
        taker_origin: true,
        client_order_id,
        // A remainder that refused to rest crossed would strand the taker
        // that came to trade, which is the opposite of what migrating it is
        // for.
        reject_if_crossed: false,
    })?;

    super::emit_clob_place_record(
        clock.unix_timestamp,
        &user_loader.key(),
        super::ClobOrderFacts {
            order_id: client_order_id,
            market_index,
            direction,
            price,
            base_asset_amount,
            base_asset_amount_filled: 0,
            max_ts,
            slot: clock.slot,
            taker_origin: true,
        },
    )?;

    msg!(
        "placed remainder as clob order {} (node {})",
        order_ref.order_id,
        order_ref.node_index
    );
    Ok(true)
}
