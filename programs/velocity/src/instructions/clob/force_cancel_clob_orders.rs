//! Force-cancel a deteriorated account's CLOB orders — the CLOB arm of the
//! `force_cancel_orders` keeper flow, and how placed-trigger shadows on a
//! failing account get reclaimed (the DLOB-side sweep deliberately skips
//! them: their live orders rest on the book).
//!
//! Same gates as the DLOB force-cancel: the account must fail initial
//! margin or sit below its equity floor (pre-liquidation cleanup), and
//! risk-*reducing* orders are skipped — cancelling those would only make
//! the account worse. The keeper reads the user's orders off the book and
//! passes their `OrderRef`s; each hint fails closed on the CLOB side if it
//! no longer belongs to this user. The keeper earns the same flat fee per
//! cancelled order, charged to the user's quote deposit in one transfer at
//! the end.
//!
//! Deliberately not gated on the quoter entry's active/approved flags —
//! dead books still need failing makers' orders reclaimed — and not
//! relay-wired: discovering deteriorated accounts is a sweep over an
//! unbounded user set, which stays bespoke-keeper territory by design.

use {
    crate::{
        controller::{
            orders::pay_keeper_flat_reward_for_spot,
            position::{decrease_open_bids_and_asks, get_position_index},
        },
        error::ErrorCode,
        instructions::{
            constraints::*,
            optional_accounts::{load_maps, AccountMaps},
        },
        load_mut,
        math::{
            constants::QUOTE_SPOT_MARKET_INDEX,
            margin::{
                calculate_margin_requirement_and_total_collateral_and_liability_info,
                calculate_net_equity_for_floor, MarginRequirementType,
            },
            orders::is_order_position_reducing,
            safe_math::SafeMath,
        },
        msg,
        signer::get_signer_seeds,
        state::{
            clob_crank::{ClobCrankConditionsV0, CLOB_CRANK_CONDITIONS_PDA_SEED},
            margin_calculation::MarginContext,
            perp_market_map::MarketSet,
            prop_amm::{
                clob_hint_scan, read_clob_node, ClobCancelOrderArgsV0, ClobOrderRefV0,
                ClobRemovedOrderV0, ClobUserRefV0, QuoterType, QuoterV0,
                CLOB_CANCEL_ORDER_V0_DISCRIMINATOR,
            },
            spot_market_map::get_writable_spot_market_set,
            state::State,
            user::{OrderStatus, User, UserStats},
        },
        validate,
    },
    anchor_lang::prelude::*,
    solana_program::{
        instruction::{AccountMeta, Instruction},
        program::{get_return_data, invoke_signed},
    },
    std::ops::DerefMut,
};

/// Refs per call, bounding CPI count and compute.
pub const MAX_FORCE_CANCEL_CLOB_ORDERS: usize = 8;

#[derive(Accounts)]
#[instruction(market_index: u16)]
pub struct ForceCancelClobOrders<'info> {
    pub state: AccountLoader<'info, State>,
    pub authority: Signer<'info>,
    #[account(
        mut,
        constraint = can_sign_for_user(&filler, &authority)?
    )]
    pub filler: AccountLoader<'info, User>,
    #[account(
        mut,
        constraint = is_stats_for_user(&filler, &filler_stats)?
    )]
    pub filler_stats: AccountLoader<'info, UserStats>,
    /// The deteriorated account whose CLOB orders are being reclaimed.
    #[account(mut)]
    pub user: AccountLoader<'info, User>,
    /// Deliberately not gated on active/approved: dead books still need
    /// failing makers' orders reclaimed.
    pub quoter: AccountLoader<'info, QuoterV0>,
    /// CHECK: validated against the quoter entry's registered execute
    /// accounts in the handler.
    #[account(mut)]
    pub clob_market: UncheckedAccount<'info>,
    /// CHECK: locked to the registered quoter program.
    #[account(address = quoter.load()?.program_id)]
    pub clob_program: UncheckedAccount<'info>,
    /// CHECK: the protocol signer PDA — the CLOB's `place_authority`.
    #[account(address = state.load()?.signer)]
    pub velocity_signer: UncheckedAccount<'info>,
    /// Wake-hint host; optional like every other CLOB path.
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

pub fn handle_force_cancel_clob_orders<'c: 'info, 'info>(
    ctx: Context<'info, ForceCancelClobOrders<'info>>,
    market_index: u16,
    order_refs: Vec<ClobOrderRefV0>,
) -> Result<()> {
    let clock = Clock::get()?;
    let state = ctx.accounts.state.load()?;

    validate!(
        !order_refs.is_empty() && order_refs.len() <= MAX_FORCE_CANCEL_CLOB_ORDERS,
        ErrorCode::DefaultError,
        "pass 1..={} order refs, got {}",
        MAX_FORCE_CANCEL_CLOB_ORDERS,
        order_refs.len()
    )?;

    let AccountMaps {
        perp_market_map,
        spot_market_map,
        mut oracle_map,
    } = load_maps(
        &mut ctx.remaining_accounts.iter().peekable(),
        &MarketSet::new(),
        &get_writable_spot_market_set(QUOTE_SPOT_MARKET_INDEX),
        clock.slot,
        None,
    )?;

    {
        let quoter = ctx.accounts.quoter.load()?;
        validate!(
            quoter.quoter_type == QuoterType::Clob,
            ErrorCode::DefaultError,
            "quoter entry is not a CLOB"
        )?;
        validate!(
            quoter.market == market_index,
            ErrorCode::DefaultError,
            "quoter entry is for market {}, cancel is for market {}",
            quoter.market,
            market_index
        )?;
        let registered = &quoter.execute_accounts[..quoter.execute_accounts_count as usize];
        validate!(
            registered
                .iter()
                .any(|meta| meta.pubkey == ctx.accounts.clob_market.key()),
            ErrorCode::DefaultError,
            "clob market is not registered on the quoter entry"
        )?;
    }

    // ---- Gate: the account must actually be failing, same as the DLOB
    // force-cancel, and the refs must be this user's risk-increasing
    // orders. ----
    let (user_ref, cancellable): (ClobUserRefV0, Vec<ClobOrderRefV0>) = {
        let user = &mut load_mut!(ctx.accounts.user)?;
        validate!(
            !user.is_being_liquidated(),
            ErrorCode::UserIsBeingLiquidated
        )?;
        validate!(!user.is_bankrupt(), ErrorCode::UserBankrupt)?;

        let margin_calc = calculate_margin_requirement_and_total_collateral_and_liability_info(
            user,
            &perp_market_map,
            &spot_market_map,
            &mut oracle_map,
            MarginContext::standard(MarginRequirementType::Initial),
        )?;
        let below_equity_floor = calculate_net_equity_for_floor(
            user,
            &perp_market_map,
            &spot_market_map,
            &mut oracle_map,
        )?
        .is_some_and(|net_equity| user.is_below_equity_floor(net_equity));
        validate!(
            !margin_calc.meets_margin_requirement() || below_equity_floor,
            ErrorCode::SufficientCollateral
        )?;
        // Per-market arm of the DLOB sweep's skip logic: an isolated
        // position answers to its own requirement, cross positions to the
        // cross requirement.
        let market_isolated = user
            .get_perp_position(market_index)
            .map(|position| position.is_isolated())
            .unwrap_or(false);
        let market_recoverable = if market_isolated {
            margin_calc.meets_isolated_margin_requirement(market_index)?
        } else {
            margin_calc.meets_cross_margin_requirement() && !below_equity_floor
        };
        validate!(
            !market_recoverable,
            ErrorCode::SufficientCollateral,
            "market {} meets its margin requirement",
            market_index
        )?;

        let user_ref = ClobUserRefV0 {
            authority: user.authority,
            sub_account_id: user.sub_account_id,
        };

        // Read each hinted node off the book: a hint that no longer holds a
        // live order is dropped (raced by a fill/cancel — normal), but a
        // hint pointing at someone else's order is a keeper error and fails
        // loudly. The risk-reducing check runs post-CPI on the returned
        // removal (which carries the side); a reducing ref reverts the
        // whole call, so the keeper's contract is to not pass them.
        let book = ctx.accounts.clob_market.try_borrow_data()?;
        let cancellable = order_refs
            .iter()
            .filter_map(|order_ref| {
                let node = read_clob_node(&book, order_ref.node_index)?;
                if !node.is_open || node.order_id != order_ref.order_id {
                    return None;
                }
                Some((order_ref, node))
            })
            .map(|(order_ref, node)| {
                validate!(
                    node.user_ref() == user_ref,
                    ErrorCode::DefaultError,
                    "order {} belongs to {}/{}, not the passed user",
                    order_ref.order_id,
                    node.user_ref().authority,
                    node.user_ref().sub_account_id
                )?;
                Ok(*order_ref)
            })
            .collect::<Result<Vec<_>>>()?;
        (user_ref, cancellable)
    };

    validate!(
        !cancellable.is_empty(),
        ErrorCode::DefaultError,
        "no passed refs are live orders of this user"
    )?;

    // ---- Cancel CPIs while no user borrows are held. ----
    let mut removed_orders: Vec<ClobRemovedOrderV0> = Vec::with_capacity(cancellable.len());
    for order_ref in &cancellable {
        let mut data = CLOB_CANCEL_ORDER_V0_DISCRIMINATOR.to_vec();
        ClobCancelOrderArgsV0 {
            order_ref: *order_ref,
            user: user_ref,
        }
        .serialize(&mut data)
        .map_err(|_| ErrorCode::DefaultError)?;
        invoke_signed(
            &Instruction {
                program_id: ctx.accounts.clob_program.key(),
                accounts: vec![
                    AccountMeta::new(ctx.accounts.clob_market.key(), false),
                    AccountMeta::new_readonly(ctx.accounts.velocity_signer.key(), true),
                ],
                data,
            },
            &[
                ctx.accounts.clob_market.to_account_info(),
                ctx.accounts.velocity_signer.to_account_info(),
                ctx.accounts.clob_program.to_account_info(),
            ],
            &[&get_signer_seeds(&state.signer_nonce)],
        )?;
        let (writer, removed_data) =
            get_return_data().ok_or_else(|| -> anchor_lang::error::Error {
                msg!("clob cancel returned no removed order");
                ErrorCode::DefaultError.into()
            })?;
        validate!(
            writer == ctx.accounts.clob_program.key(),
            ErrorCode::DefaultError,
            "clob cancel return data written by {}",
            writer
        )?;
        removed_orders.push(
            ClobRemovedOrderV0::deserialize(&mut removed_data.as_slice()).map_err(|_| {
                msg!("clob cancel returned undecodable removed order");
                ErrorCode::DefaultError
            })?,
        );
    }

    // ---- Unwind, skip-filter risk-reducing, fee. ----
    let mut total_fee = 0u64;
    {
        let user = &mut load_mut!(ctx.accounts.user)?;
        let mut filler = load_mut!(ctx.accounts.filler)?;
        let position_index = get_position_index(&user.perp_positions, market_index)?;
        for removed in &removed_orders {
            validate!(
                removed.user == user_ref,
                ErrorCode::DefaultError,
                "clob cancelled an order for a different user"
            )?;
            let direction = removed.side.to_position_direction();
            let is_position_reducing = is_order_position_reducing(
                &direction,
                removed.base_asset_amount,
                user.perp_positions[position_index].base_asset_amount,
            )?;
            validate!(
                !is_position_reducing,
                ErrorCode::InvalidOrderNotRiskReducing,
                "order {} is risk-reducing; force-cancel skips those — don't pass it",
                removed.order_id
            )?;
            decrease_open_bids_and_asks(
                &mut user.perp_positions[position_index],
                &direction,
                removed.base_asset_amount,
                true,
            )?;
            user.perp_positions[position_index].open_orders = user.perp_positions[position_index]
                .open_orders
                .saturating_sub(1);
            user.decrement_open_orders(false);
            // A placed trigger's shadow frees for good — a failing account
            // must not re-arm.
            user.release_placed_trigger_slot(market_index, removed.order_id, OrderStatus::Canceled);
            total_fee = total_fee.safe_add(state.perp_fee_structure.flat_filler_fee)?;
        }

        pay_keeper_flat_reward_for_spot(
            user,
            Some(&mut filler),
            spot_market_map.get_quote_spot_market_mut()?.deref_mut(),
            total_fee,
            clock.slot,
        )?;
        user.update_last_active_slot(clock.slot);
    }

    // Repair the wake hints from the post-cancel book.
    if let Some(conditions_loader) = &ctx.accounts.crank_conditions {
        let (min_expiry, min_activation) =
            clob_hint_scan(&ctx.accounts.clob_market.try_borrow_data()?, clock.slot);
        let mut conditions = load_mut!(conditions_loader)?;
        conditions.repair_expiry(min_expiry)?;
        conditions.repair_activation(min_activation)?;
    }

    msg!(
        "force-cancelled {} clob orders for user {}",
        removed_orders.len(),
        ctx.accounts.user.key()
    );
    Ok(())
}
