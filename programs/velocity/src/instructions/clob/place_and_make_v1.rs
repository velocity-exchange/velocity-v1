//! `place_and_make_perp_order_v1` — rest a maker order on the CLOB.
//!
//! A maker posts a limit order that rests on the market's CLOB. The order never
//! enters `User.orders`: it is built, margin-checked, and placed straight on the
//! book as a maker quote. Whoever wants that liquidity takes it off the book.
//!
//! Just-in-time matching against a named taker order is gone. A maker provides
//! liquidity that rests; it does not consume liquidity and quotes no external
//! books, so there is no taker to name and nothing to route.

use {
    crate::{
        controller,
        error::ErrorCode,
        instructions::{
            constraints::*,
            optional_accounts::{load_maps, AccountMaps},
            try_place_remainder_on_clob,
        },
        load, load_mut,
        state::{
            order_params::{OrderParams, PlaceOrderOptions, PostOnlyParam},
            perp_market_map::{get_writable_perp_market_set, MarketSet},
            prop_amm::QuoterV0,
            state::State,
            user::{OrderType, User, UserStats},
        },
        validate,
    },
    anchor_lang::prelude::*,
};

#[derive(Accounts)]
pub struct PlaceAndMakeV1<'info> {
    pub state: AccountLoader<'info, State>,
    #[account(
        mut,
        constraint = can_sign_for_user(&user, &authority)?
    )]
    pub user: AccountLoader<'info, User>,
    #[account(
        mut,
        constraint = is_stats_for_user(&user, &user_stats)?
    )]
    pub user_stats: AccountLoader<'info, UserStats>,
    pub authority: Signer<'info>,
    /// The market's CLOB registry entry — the maker only ever rests on a vetted
    /// book.
    pub quoter: AccountLoader<'info, QuoterV0>,
    /// CHECK: validated against the quoter entry's registered accounts
    /// (`ClobMarket::from_quoter`), so a valid entry can't be pointed at an
    /// arbitrary account.
    #[account(mut)]
    pub clob_market: UncheckedAccount<'info>,
    /// CHECK: locked to the registered quoter program.
    #[account(address = quoter.load()?.program_id)]
    pub clob_program: UncheckedAccount<'info>,
    /// CHECK: the CLOB place authority PDA — what a book's `place_authority`
    /// is set to, and nothing a third-party quoter is ever handed.
    #[account(address = crate::signer::CLOB_AUTHORITY)]
    pub clob_authority: UncheckedAccount<'info>,
    /// The flow authority, signing this transaction as a named account.
    /// Required only for a faster-than-default activation delay — presence
    /// is the attestation. The zero key cannot sign, so an unset flow
    /// authority admits nobody.
    #[account(
        constraint = flow_authority.key()
            == state.load()?.hot_key(crate::state::state::HotRole::FlowAuthority)
            @ crate::error::ErrorCode::UnattestedFastActivation
    )]
    pub flow_authority: Option<Signer<'info>>,
}

#[access_control(
    fill_not_paused(&ctx.accounts.state)
)]
pub fn handle_place_and_make_perp_order_v1<'c: 'info, 'info>(
    ctx: Context<'info, PlaceAndMakeV1<'info>>,
    params: OrderParams,
    // The book speed bump the maker rests behind. `None` takes the book's
    // default. A value below the default needs the flow-authority attestation.
    activation_delay_slots: Option<u32>,
) -> Result<()> {
    let clock = Clock::get()?;
    let state = ctx.accounts.state.load()?;
    let user_key = ctx.accounts.user.key();

    let remaining_accounts_iter = &mut ctx.remaining_accounts.iter().peekable();
    let AccountMaps {
        perp_market_map,
        spot_market_map,
        mut oracle_map,
    } = load_maps(
        remaining_accounts_iter,
        &get_writable_perp_market_set(params.market_index),
        &MarketSet::new(),
        clock.slot,
        state.slot_clock(),
        Some(state.oracle_guard_rails),
    )?;

    validate!(
        params.order_type == OrderType::Limit,
        ErrorCode::InvalidOrderIOCPostOnly,
        "place_and_make rests a limit order on the book"
    )?;
    // A post-only order refuses to rest crossed; a plain limit rests crossed and
    // the cross crank matches it at the counterparty's price. Either way the
    // order rests as a maker, so `post_only` chooses only whether a crossed
    // placement is refused, not the fee schedule.
    let reject_if_crossed = params.post_only != PostOnlyParam::None;

    // Build and margin-check the maker order without persisting it to
    // `User.orders`. The build reserves nothing that lasts; the rest below makes
    // the order's own reservation on the book.
    let placed = {
        let mut user = load_mut!(ctx.accounts.user)?;
        // Sweep expired slot orders first: their reservations release, which
        // can be what lets the new order pass the margin gate. The create
        // never touches `user.orders`, so the sweep is the caller's.
        controller::orders::expire_orders(
            &mut user,
            &user_key,
            &perp_market_map,
            &spot_market_map,
            &mut oracle_map,
            clock.unix_timestamp,
            clock.slot,
        )?;
        controller::orders::create_ephemeral_perp_order(
            &state,
            &mut user,
            user_key,
            &perp_market_map,
            &spot_market_map,
            &mut oracle_map,
            &clock,
            params,
            // The order rests straight on the CLOB; its CLOB placement record
            // is the one statement about it, so suppress the ephemeral place
            // record that would be a redundant second one.
            PlaceOrderOptions {
                emit_place_record: false,
                ..PlaceOrderOptions::default()
            },
            &mut None,
        )?
    };
    let Some(order) = placed else {
        // The order soft-skipped its build. Nothing to rest.
        return Ok(());
    };

    // A maker order rests at a fixed price. The CLOB has no oracle-offset or
    // reduce-only semantics, so refuse those. Unlike a taker remainder a maker
    // is post-only, so `restable_remainder_price` is the wrong gate here.
    validate!(
        order.order_type == OrderType::Limit
            && order.oracle_price_offset == 0
            && !order.reduce_only,
        ErrorCode::InvalidOrderIOCPostOnly,
        "place_and_make order cannot rest on the book"
    )?;
    let rest_price = order.price;

    let position_base = load!(ctx.accounts.user)?
        .get_perp_position(params.market_index)
        .map(|position| position.base_asset_amount)
        .unwrap_or(0);
    let unfilled = order
        .get_base_asset_amount_unfilled(Some(position_base))
        .unwrap_or(order.base_asset_amount);

    // A below-default activation delay is reserved for attested flow.
    crate::instructions::attest_activation_delay(
        &ctx.accounts.quoter,
        activation_delay_slots,
        ctx.accounts.flow_authority.is_some(),
    )?;
    try_place_remainder_on_clob(
        &ctx.accounts.user,
        &ctx.accounts.quoter,
        &ctx.accounts.clob_market.to_account_info(),
        &ctx.accounts.clob_program.to_account_info(),
        &ctx.accounts.clob_authority.to_account_info(),
        &perp_market_map,
        &spot_market_map,
        &mut oracle_map,
        params.market_index,
        order.direction,
        rest_price,
        unfilled,
        order.max_ts,
        order.order_id,
        // A maker quote, not a taker remainder: it may be taken at its own price.
        false,
        reject_if_crossed,
        order.reduce_only,
        activation_delay_slots,
        &clock,
    )?;
    Ok(())
}
