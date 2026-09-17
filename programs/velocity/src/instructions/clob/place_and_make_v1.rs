//! `place_and_make_perp_order_v1`, which rests a maker order on the CLOB.
//!
//! A maker posts a limit order that rests on the market's CLOB. The order never
//! enters `User.orders`. The handler builds it, checks margin, and places it
//! straight on the book as a maker quote. A later taker removes that liquidity
//! from the book.
//!
//! There is no just-in-time matching against a named taker order. A maker
//! provides liquidity that rests. It consumes no liquidity and quotes no
//! external book, so there is no taker to name and nothing to route.

use {
    crate::{
        controller,
        error::ErrorCode,
        instructions::{constraints::*, optional_accounts::load_maps, try_place_remainder_on_clob},
        load, load_mut,
        state::{
            order_params::{OrderParams, PlaceOrderOptions, PostOnlyParam},
            perp_market_map::{get_writable_perp_market_set, MarketSet},
            prop_amm::QuoterSlabV0,
            state::State,
            user::{OrderType, User, UserStats},
        },
        validate,
    },
    anchor_lang::prelude::*,
};

#[derive(Accounts)]
#[instruction(args: PlaceAndMakePerpOrderV1Args)]
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
    /// The market's quoter slab. The maker only ever rests on the vetted book
    /// that its `Clob` slot names.
    #[account(
        has_one = clob_market,
        constraint = quoter_slab.load()?.market == args.params.market_index,
    )]
    pub quoter_slab: AccountLoader<'info, QuoterSlabV0>,
    /// CHECK: `ClobMarket::from_slab` checks this against the book slot's
    /// registered response account. A valid slot cannot be pointed at an
    /// arbitrary account.
    #[account(mut)]
    pub clob_market: UncheckedAccount<'info>,
    /// CHECK: a Clob slot's program is pinned to velocity's CLOB at
    /// registration. The handler checks it again through the slot.
    #[account(address = crate::ids::clob_program::id())]
    pub clob_program: UncheckedAccount<'info>,
    /// The flow authority, signing this transaction as a named account.
    /// It is required only for an activation delay below the default. The
    /// signature is the attestation. The zero key cannot sign, so an unset flow
    /// authority admits nobody.
    #[account(
        constraint = flow_authority.key()
            == state.load()?.hot_key(crate::state::state::HotRole::FlowAuthority)
            @ crate::error::ErrorCode::UnattestedFastActivation
    )]
    pub flow_authority: Option<Signer<'info>>,
}

#[derive(Clone, Copy, AnchorSerialize, AnchorDeserialize)]
pub struct PlaceAndMakePerpOrderV1Args {
    pub params: OrderParams,
    /// The book speed bump the maker rests behind. `None` takes the book's
    /// default. A value below the default needs the flow-authority
    /// attestation.
    pub activation_delay_slots: Option<u32>,
}

#[access_control(
    fill_not_paused(&ctx.accounts.state)
)]
pub fn handle_place_and_make_perp_order_v1<'c: 'info, 'info>(
    ctx: Context<'info, PlaceAndMakeV1<'info>>,
    args: PlaceAndMakePerpOrderV1Args,
) -> Result<()> {
    let PlaceAndMakePerpOrderV1Args {
        params,
        activation_delay_slots,
    } = args;
    let clock = Clock::get()?;
    let state = ctx.accounts.state.load()?;
    let user_key = ctx.accounts.user.key();

    let remaining_accounts_iter = &mut ctx.remaining_accounts.iter().peekable();
    let mut maps = load_maps(
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
    // A post-only order refuses to rest crossed. A plain limit rests crossed,
    // and the cross crank matches it at the counterparty's price. Either way the
    // order rests as a maker. So `post_only` chooses only whether a crossed
    // placement is refused. It does not choose the fee schedule.
    let reject_if_crossed = params.post_only != PostOnlyParam::None;

    // Build and margin-check the maker order without writing it to
    // `User.orders`. The build reserves nothing that lasts. The placement below
    // makes the order's own reservation on the book.
    let placed = {
        let mut user = load_mut!(ctx.accounts.user)?;
        // Sweep expired slot orders first. Their reservations release, which
        // can be what lets the new order pass the margin gate. The create call
        // never touches `user.orders`, so the caller runs the sweep.
        controller::orders::expire_orders(
            &mut user,
            &user_key,
            &mut maps,
            clock.unix_timestamp,
            clock.slot,
        )?;
        controller::orders::create_ephemeral_perp_order(
            &state,
            &mut user,
            user_key,
            &mut maps,
            &clock,
            params,
            // The order rests straight on the CLOB, and its CLOB placement
            // record is the one statement about it. Suppress the ephemeral
            // place record, which would be a second copy.
            PlaceOrderOptions {
                emit_place_record: false,
                ..PlaceOrderOptions::default()
            },
            &mut None,
        )?
    };
    let Some(order) = placed else {
        // The build skipped the order. There is nothing to rest.
        return Ok(());
    };

    // A maker order rests at a fixed price. The CLOB has no oracle-offset and
    // no reduce-only semantics, so refuse both. A maker is post-only, unlike a
    // taker remainder, so `restable_remainder_price` is the wrong gate here.
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
        &ctx.accounts.quoter_slab,
        params.market_index,
        activation_delay_slots,
        ctx.accounts.flow_authority.is_some(),
    )?;
    try_place_remainder_on_clob(
        &ctx.accounts.user,
        &ctx.accounts.quoter_slab,
        &ctx.accounts.clob_market.to_account_info(),
        &ctx.accounts.clob_program.to_account_info(),
        &mut maps,
        params.market_index,
        order.direction,
        rest_price,
        unfilled,
        order.max_ts,
        order.order_id,
        // A maker quote rests as maker-origin, so a later order takes it at
        // its own price.
        false,
        reject_if_crossed,
        order.reduce_only,
        activation_delay_slots,
        &clock,
    )?;
    Ok(())
}
