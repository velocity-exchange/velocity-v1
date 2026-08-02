//! Place a resting limit order on a registered CLOB. Velocity owns placement
//! policy: it verifies the `User` authority, reserves the order's worst-case
//! open-order aggregates, and gates margin exactly like a DLOB placement —
//! the CLOB trusts its `place_authority` (the velocity signer PDA) and only
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
        signer::get_signer_seeds,
        state::{
            clob_crank::{ClobCrankConditionsV0, CLOB_CRANK_CONDITIONS_PDA_SEED},
            market_status::MarketStatus,
            perp_market_map::MarketSet,
            prop_amm::{
                ClobOrderRefV0, ClobPlaceOrderArgsV0, ClobSide, QuoterType, QuoterV0,
                CLOB_PLACE_ORDER_V0_DISCRIMINATOR,
            },
            state::State,
            user::User,
        },
        validate,
    },
    anchor_lang::prelude::*,
    solana_program::{
        instruction::{AccountMeta, Instruction},
        program::{get_return_data, invoke_signed},
    },
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
    /// CHECK: the protocol signer PDA — the CLOB's `place_authority`.
    #[account(address = state.load()?.signer)]
    pub velocity_signer: UncheckedAccount<'info>,
    /// The market's relay conditions account, so an expiring placement
    /// min-folds its `max_ts` into the expire condition's `wake_ts` hint.
    /// Optional — placement must not brick on a market whose conditions were
    /// never initialized, and a missed hint is caught by the fallback poll
    /// condition (latency, not liveness).
    #[account(
        mut,
        seeds = [
            CLOB_CRANK_CONDITIONS_PDA_SEED,
            params.market_index.to_le_bytes().as_ref(),
        ],
        bump
    )]
    pub crank_conditions: Option<AccountLoader<'info, ClobCrankConditionsV0>>,
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

    {
        let quoter = ctx.accounts.quoter.load()?;
        validate!(
            quoter.quoter_type == QuoterType::Clob,
            ErrorCode::DefaultError,
            "quoter entry is not a CLOB"
        )?;
        validate!(
            quoter.is_active && quoter.is_approved,
            ErrorCode::DefaultError,
            "CLOB quoter is not active and approved"
        )?;
        validate!(
            quoter.market == params.market_index,
            ErrorCode::DefaultError,
            "quoter entry is for market {}, order is for market {}",
            quoter.market,
            params.market_index
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
        let default_delay = crate::state::prop_amm::read_clob_u32(
            &ctx.accounts.clob_market.try_borrow_data()?,
            crate::state::prop_amm::CLOB_DEFAULT_ACTIVATION_DELAY_OFFSET,
        )
        .ok_or(ErrorCode::DefaultError)?;
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
    let user_ref = {
        let user = crate::load!(ctx.accounts.user)?;
        crate::state::prop_amm::ClobUserRefV0 {
            authority: user.authority,
            sub_account_id: user.sub_account_id,
        }
    };
    let mut data = CLOB_PLACE_ORDER_V0_DISCRIMINATOR.to_vec();
    ClobPlaceOrderArgsV0 {
        side,
        price: params.price,
        base_asset_amount: params.base_asset_amount,
        activation_delay_slots: params.activation_delay_slots,
        max_ts: params.max_ts,
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
    // The CLOB returns the new order's ref; leave it as the transaction's
    // return data (clients persist it as the cancel hint) but decode it here
    // so a malformed response fails the placement.
    let (writer, ref_data) = get_return_data().ok_or_else(|| -> anchor_lang::error::Error {
        msg!("clob place returned no order ref");
        ErrorCode::DefaultError.into()
    })?;
    validate!(
        writer == ctx.accounts.clob_program.key(),
        ErrorCode::DefaultError,
        "clob place return data written by {}",
        writer
    )?;
    let order_ref = ClobOrderRefV0::deserialize(&mut ref_data.as_slice()).map_err(|_| {
        msg!("clob place returned undecodable order ref");
        ErrorCode::DefaultError
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

    // Wake the cranks no later than this order matters: min-fold its expiry
    // into the expire hint, and — when it rests behind a speed bump — its
    // activation slot into the cross-activation hint, so a cross that makers
    // lined up for fires the moment the order becomes matchable. Best-effort:
    // the fallback poll covers placements that omit the account.
    if let Some(conditions) = &ctx.accounts.crank_conditions {
        let mut conditions = load_mut!(conditions)?;
        if params.max_ts != 0 {
            conditions.note_expiry(params.max_ts)?;
        }
        let delay = match params.activation_delay_slots {
            Some(delay) => delay,
            // Mirror the CLOB's default (`slot + default_delay`), read
            // straight off the book's header bytes.
            None => crate::state::prop_amm::read_clob_u32(
                &ctx.accounts.clob_market.try_borrow_data()?,
                crate::state::prop_amm::CLOB_DEFAULT_ACTIVATION_DELAY_OFFSET,
            )
            .ok_or(ErrorCode::DefaultError)?,
        };
        if delay > 0 {
            conditions.note_activation(clock.slot.saturating_add(delay as u64))?;
        }
    }

    msg!(
        "placed clob order {} (node {}) for user {}",
        order_ref.order_id,
        order_ref.node_index,
        ctx.accounts.user.key()
    );
    Ok(())
}

/// Rest an unfilled `place_and_take` remainder on the CLOB — the S5 rule
/// ("if it can rest and be matched, it lives on the CLOB") applied to the
/// taker flow's leftover. Degrades gracefully: a dead quoter entry or a
/// failed margin re-reserve returns `Ok(false)` (the remainder stays
/// cancelled, the fill stands) instead of reverting the whole
/// place-and-take. Only a hard-cap placement rejection on the CLOB side
/// reverts, which is the documented ops-failure state.
#[allow(clippy::too_many_arguments)]
pub fn try_place_remainder_on_clob<'info>(
    state: &crate::state::state::State,
    user_loader: &AccountLoader<'info, User>,
    quoter_loader: &AccountLoader<'info, QuoterV0>,
    clob_market: &AccountInfo<'info>,
    clob_program: &AccountInfo<'info>,
    velocity_signer: &AccountInfo<'info>,
    crank_conditions: Option<&AccountLoader<'info, ClobCrankConditionsV0>>,
    perp_market_map: &crate::state::perp_market_map::PerpMarketMap,
    spot_market_map: &crate::state::spot_market_map::SpotMarketMap,
    oracle_map: &mut crate::state::oracle_map::OracleMap,
    market_index: u16,
    direction: PositionDirection,
    price: u64,
    base_asset_amount: u64,
    max_ts: i64,
    clock: &Clock,
) -> Result<bool> {
    {
        let quoter = quoter_loader.load()?;
        validate!(
            quoter.quoter_type == QuoterType::Clob && quoter.market == market_index,
            ErrorCode::DefaultError,
            "quoter entry does not serve market {}",
            market_index
        )?;
        let registered = &quoter.execute_accounts[..quoter.execute_accounts_count as usize];
        validate!(
            registered
                .iter()
                .any(|meta| meta.pubkey == clob_market.key()),
            ErrorCode::DefaultError,
            "clob market is not registered on the quoter entry"
        )?;
        if !(quoter.is_active && quoter.is_approved) {
            msg!("clob quoter inactive; remainder stays cancelled");
            return Ok(false);
        }
    }

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
            sub_account_id: user.sub_account_id,
        }
    };

    // CPI the placement (identity travels in the args; the user account is
    // not lent, so holding no borrow is not even required — kept dropped
    // for symmetry with the main placement path).
    let side = match direction {
        PositionDirection::Long => ClobSide::Bid,
        PositionDirection::Short => ClobSide::Ask,
    };
    let mut data = CLOB_PLACE_ORDER_V0_DISCRIMINATOR.to_vec();
    ClobPlaceOrderArgsV0 {
        side,
        price,
        base_asset_amount,
        activation_delay_slots: None,
        max_ts,
        user: user_ref,
    }
    .serialize(&mut data)
    .map_err(|_| ErrorCode::DefaultError)?;
    invoke_signed(
        &Instruction {
            program_id: clob_program.key(),
            accounts: vec![
                AccountMeta::new(clob_market.key(), false),
                AccountMeta::new_readonly(velocity_signer.key(), true),
            ],
            data,
        },
        &[
            clob_market.clone(),
            velocity_signer.clone(),
            clob_program.clone(),
        ],
        &[&get_signer_seeds(&state.signer_nonce)],
    )?;
    let (writer, ref_data) = get_return_data().ok_or_else(|| -> anchor_lang::error::Error {
        msg!("clob place returned no order ref");
        ErrorCode::DefaultError.into()
    })?;
    validate!(
        writer == clob_program.key(),
        ErrorCode::DefaultError,
        "clob place return data written by {}",
        writer
    )?;
    let order_ref = ClobOrderRefV0::deserialize(&mut ref_data.as_slice()).map_err(|_| {
        msg!("clob place returned undecodable order ref");
        ErrorCode::DefaultError
    })?;

    // Wake the cranks no later than this order matters.
    if let Some(conditions) = crank_conditions {
        let mut conditions = load_mut!(conditions)?;
        if max_ts != 0 {
            conditions.note_expiry(max_ts)?;
        }
        let delay = crate::state::prop_amm::read_clob_u32(
            &clob_market.try_borrow_data()?,
            crate::state::prop_amm::CLOB_DEFAULT_ACTIVATION_DELAY_OFFSET,
        )
        .ok_or(ErrorCode::DefaultError)?;
        if delay > 0 {
            conditions.note_activation(clock.slot.saturating_add(delay as u64))?;
        }
    }

    msg!(
        "placed remainder as clob order {} (node {})",
        order_ref.order_id,
        order_ref.node_index
    );
    Ok(true)
}
