//! `place_and_make_perp_order_v1`, which rests a maker order on the CLOB.
//!
//! A maker posts a limit order that rests on the market's CLOB. The order never
//! enters `User.orders`. The handler builds it, checks margin, and places it
//! straight on the book as a maker quote. A later taker removes that liquidity
//! from the book.
//!
//! There is no just-in-time matching against a named taker order. A maker
//! provides liquidity that rests. It consumes no liquidity and quotes no
//! external book, so there is no taker to name and nothing to route. An
//! immediate-or-cancel order therefore has nothing to do here and is refused.
//!
//! A maker has no fill to protect, so a book refusal is an error. The soft
//! refusal of `rest_remainder_on_clob` is for a taker remainder only. The two
//! soft skips of a placement stay soft: an expired `max_ts`, and a
//! `TryPostOnly` order that would cross the vAMM or the book.
//!
//! A reduce-only maker rests at most the position it reduces. The book clamps
//! its fills to that position, so a `ReduceOnly` market or a reduce-only user
//! can still post an order that closes.
//!
//! A maker order carries no builder code. A book order has no
//! `RevenueShareOrder` row, so the builder could never be paid. The order is
//! refused rather than rested without the fee.

use {
    crate::{
        controller::{self, position::PositionDirection},
        error::ErrorCode,
        instructions::{
            constraints::*,
            optional_accounts::{load_maps, AccountMaps},
            rest_on_clob, ClobRestAccounts, ClobRestOrder, RestOutcome,
        },
        load, load_mut, msg,
        state::{
            order_params::{OrderParams, PlaceOrderOptions, PostOnlyParam},
            perp_market_map::{get_writable_perp_market_set, MarketSet},
            prop_amm::{ClobMarket, QuoterSlabExt, QuoterSlabV0},
            state::State,
            user::{Order, OrderType, User, UserStats},
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
}

#[derive(Clone, Copy, AnchorSerialize, AnchorDeserialize)]
pub struct PlaceAndMakePerpOrderV1Args {
    pub params: OrderParams,
}

#[access_control(
    fill_not_paused(&ctx.accounts.state)
)]
pub fn handle_place_and_make_perp_order_v1<'c: 'info, 'info>(
    ctx: Context<'info, PlaceAndMakeV1<'info>>,
    args: PlaceAndMakePerpOrderV1Args,
) -> Result<()> {
    let PlaceAndMakePerpOrderV1Args { params } = args;
    let clock = Clock::get()?;
    let state = ctx.accounts.state.load()?;

    let remaining_accounts_iter = &mut ctx.remaining_accounts.iter().peekable();
    let mut maps = load_maps(
        remaining_accounts_iter,
        &get_writable_perp_market_set(params.market_index),
        &MarketSet::new(),
        clock.slot,
        state.slot_clock(),
        Some(state.oracle_guard_rails),
    )?;

    validate_maker_params(&params)?;

    let Some(order) = build_maker_order(&ctx, &state, &mut maps, &clock, params)? else {
        // The build skipped the order. There is nothing to rest.
        return Ok(());
    };

    // Only the signed-message route carries a flow attestation, so a maker
    // cannot ask for an activation delay below the book's default.
    crate::instructions::attest_activation_delay(
        &ctx.accounts.quoter_slab,
        params.market_index,
        params.activation_delay_slots,
        false,
    )?;

    let position_base = load!(ctx.accounts.user)?
        .get_perp_position(params.market_index)
        .map(|position| position.base_asset_amount)
        .unwrap_or(0);
    let base_asset_amount = maker_rest_size(&order, position_base)?;

    let Some(price) = price_against_book(&ctx, params.post_only, &order)? else {
        msg!("TryPostOnly order would cross the book; skipping order");
        return Ok(());
    };

    let rest = maker_rest(&params, &order, price, base_asset_amount);
    let placed = match rest_on_clob(&clob_rest_accounts(&ctx), &mut maps, &rest, &clock)? {
        RestOutcome::Placed(placed) => placed,
        RestOutcome::Refused(reason) => {
            msg!("book refuses the maker order ({:?})", reason);
            return Err(reason.error_code().into());
        }
    };

    let oracle_price = {
        let market = maps.perp_market_map.get_ref(&params.market_index)?;
        maps.oracle_map.get_price_data(&market.oracle_id())?.price
    };

    super::emit_clob_maker_place_records(
        clock.unix_timestamp,
        oracle_price,
        &ctx.accounts.user.key(),
        &super::resting_maker_order(&order, placed.price, base_asset_amount),
    )?;

    Ok(())
}

/// Refuse the order shapes a resting maker cannot carry.
fn validate_maker_params(params: &OrderParams) -> Result<()> {
    validate!(
        params.order_type == OrderType::Limit,
        ErrorCode::InvalidOrderIOCPostOnly,
        "place_and_make rests a limit order on the book"
    )?;

    validate!(
        !params.is_immediate_or_cancel(),
        ErrorCode::InvalidOrderIOC,
        "a maker order rests on the book, so it cannot be immediate or cancel"
    )?;

    validate!(
        params.builder_idx.is_none() && params.builder_fee_tenth_bps.is_none(),
        ErrorCode::InvalidOrder,
        "a maker order on the book cannot carry a builder code"
    )?;

    Ok(())
}

/// Build and margin-check the maker order without writing it to
/// `User.orders`. The build reserves nothing that lasts. The placement makes
/// the order's own reservation on the book.
fn build_maker_order<'info>(
    ctx: &Context<'info, PlaceAndMakeV1<'info>>,
    state: &State,
    maps: &mut AccountMaps,
    clock: &Clock,
    params: OrderParams,
) -> Result<Option<Order>> {
    let user_key = ctx.accounts.user.key();
    let mut user = load_mut!(ctx.accounts.user)?;

    // Expired slot orders release their reservations, which can be what lets
    // the new order pass the margin gate. The create call never touches
    // `user.orders`, so the caller runs the sweep.
    controller::orders::expire_orders(
        &mut user,
        &user_key,
        maps,
        clock.unix_timestamp,
        clock.slot,
    )?;

    controller::orders::validate_user_order_id_unused(&user, params.user_order_id)?;

    Ok(controller::orders::create_detached_perp_order(
        state,
        &mut user,
        user_key,
        maps,
        clock,
        params,
        // The handler writes the place records once the book holds the order,
        // at the price and size the book holds.
        PlaceOrderOptions {
            emit_place_record: false,
            ..PlaceOrderOptions::default()
        },
        &mut None,
    )?)
}

/// The size a maker order rests with. A reduce-only order rests at most the
/// position it reduces, and one with nothing to reduce is refused.
fn maker_rest_size(order: &Order, position_base: i64) -> Result<u64> {
    let size = order.get_base_asset_amount_unfilled(Some(position_base))?;
    validate!(
        size > 0,
        ErrorCode::InvalidOrderNotRiskReducing,
        "reduce-only maker order has no position to reduce: position {}",
        position_base
    )?;

    Ok(size)
}

/// The maker's price after the book's best opposite order is considered.
/// `None` is a `TryPostOnly` order that would cross, which the placement skips.
///
/// Only `TryPostOnly` and `Slide` ask the book. A `MustPostOnly` order that
/// crosses is refused by the book itself.
fn price_against_book<'info>(
    ctx: &Context<'info, PlaceAndMakeV1<'info>>,
    post_only: PostOnlyParam,
    order: &Order,
) -> Result<Option<u64>> {
    if !matches!(post_only, PostOnlyParam::TryPostOnly | PostOnlyParam::Slide) {
        return Ok(Some(order.price));
    }

    let market_index = order.market_index;
    let tick_size = ctx
        .accounts
        .quoter_slab
        .clob_slot(market_index)?
        .config
        .book_tick_size;
    let clob = ClobMarket::from_slab(
        &ctx.accounts.quoter_slab,
        market_index,
        &ctx.accounts.clob_market,
        &ctx.accounts.clob_program,
    )?;
    let heads = clob.reader().next_cross()?;
    let opposite = match order.direction {
        PositionDirection::Long => heads.ask,
        PositionDirection::Short => heads.bid,
    };

    Ok(post_only_book_price(
        post_only,
        order.direction,
        order.price,
        opposite.found().then_some(opposite.price),
        tick_size,
    ))
}

/// allow-verbose: states the one gap between this test and the book's own
/// refusal, which a caller must know to predict a revert.
///
/// Apply `TryPostOnly` or `Slide` to `price` against the best opposite book
/// price. A crossing `TryPostOnly` order returns `None`. A crossing `Slide`
/// order moves one tick behind the opposite best. The book reports its best
/// activated order. Its own cross test also counts an order inside its
/// activation delay, so a better-priced order of that kind can still make the
/// placement fail with `OrderWouldCross`.
fn post_only_book_price(
    post_only: PostOnlyParam,
    direction: PositionDirection,
    price: u64,
    opposite_best: Option<u64>,
    tick_size: u64,
) -> Option<u64> {
    let Some(opposite_best) = opposite_best else {
        return Some(price);
    };

    let crosses = match direction {
        PositionDirection::Long => price >= opposite_best,
        PositionDirection::Short => price <= opposite_best,
    };

    match (crosses, post_only) {
        (false, _) => Some(price),
        (true, PostOnlyParam::TryPostOnly) => None,
        (true, _) => Some(match direction {
            PositionDirection::Long => opposite_best.saturating_sub(tick_size.max(1)),
            PositionDirection::Short => opposite_best.saturating_add(tick_size.max(1)),
        }),
    }
}

/// The maker order in the terms the book takes.
fn maker_rest(
    params: &OrderParams,
    order: &Order,
    price: u64,
    base_asset_amount: u64,
) -> ClobRestOrder {
    ClobRestOrder {
        market_index: params.market_index,
        direction: order.direction,
        price,
        base_asset_amount,
        max_ts: order.max_ts,
        client_order_id: order.order_id,
        // A maker quote rests as maker-origin, so a later order takes it at
        // its own price.
        taker_origin: false,
        // A plain limit rests crossed, and the cross crank matches it at the
        // counterparty's price. `post_only` does not choose the fee schedule.
        reject_if_crossed: params.post_only != PostOnlyParam::None,
        reduce_only: order.reduce_only,
        activation_delay_slots: params.activation_delay_slots,
    }
}

fn clob_rest_accounts<'a, 'info>(
    ctx: &'a Context<'info, PlaceAndMakeV1<'info>>,
) -> ClobRestAccounts<'a, 'info> {
    ClobRestAccounts {
        user: &ctx.accounts.user,
        quoter_slab: &ctx.accounts.quoter_slab,
        clob_market: &ctx.accounts.clob_market,
        clob_program: &ctx.accounts.clob_program,
    }
}

#[cfg(test)]
mod tests {
    use {
        super::{maker_rest_size, post_only_book_price, validate_maker_params},
        crate::{
            controller::position::PositionDirection,
            error::ErrorCode,
            state::{
                order_params::{OrderParams, OrderParamsBitFlag, PostOnlyParam},
                user::{Order, OrderType},
            },
        },
    };

    fn limit_params() -> OrderParams {
        OrderParams {
            order_type: OrderType::Limit,
            ..OrderParams::default()
        }
    }

    #[test]
    fn a_plain_limit_maker_is_admitted() {
        assert!(validate_maker_params(&limit_params()).is_ok());
    }

    #[test]
    fn an_immediate_or_cancel_maker_is_refused() {
        let params = OrderParams {
            bit_flags: OrderParamsBitFlag::ImmediateOrCancel as u8,
            post_only: PostOnlyParam::MustPostOnly,
            ..limit_params()
        };

        assert_eq!(
            validate_maker_params(&params),
            Err(ErrorCode::InvalidOrderIOC.into())
        );
    }

    #[test]
    fn a_maker_with_a_builder_code_is_refused() {
        let params = OrderParams {
            builder_idx: Some(0),
            builder_fee_tenth_bps: Some(10),
            ..limit_params()
        };

        assert_eq!(
            validate_maker_params(&params),
            Err(ErrorCode::InvalidOrder.into())
        );
    }

    fn reduce_only_ask(base_asset_amount: u64) -> Order {
        Order {
            order_type: OrderType::Limit,
            direction: PositionDirection::Short,
            base_asset_amount,
            reduce_only: true,
            ..Order::default()
        }
    }

    #[test]
    fn a_reduce_only_maker_rests_at_most_its_position() {
        assert_eq!(maker_rest_size(&reduce_only_ask(10), 4).unwrap(), 4);
        assert_eq!(maker_rest_size(&reduce_only_ask(3), 4).unwrap(), 3);
    }

    #[test]
    fn a_reduce_only_maker_with_nothing_to_reduce_is_refused() {
        assert_eq!(
            maker_rest_size(&reduce_only_ask(10), -4),
            Err(ErrorCode::InvalidOrderNotRiskReducing.into())
        );
        assert_eq!(
            maker_rest_size(&reduce_only_ask(10), 0),
            Err(ErrorCode::InvalidOrderNotRiskReducing.into())
        );
    }

    #[test]
    fn try_post_only_skips_a_bid_that_crosses_the_book() {
        let price = post_only_book_price(
            PostOnlyParam::TryPostOnly,
            PositionDirection::Long,
            100,
            Some(100),
            1,
        );

        assert_eq!(price, None);
    }

    #[test]
    fn try_post_only_keeps_a_bid_behind_the_book() {
        let price = post_only_book_price(
            PostOnlyParam::TryPostOnly,
            PositionDirection::Long,
            99,
            Some(100),
            1,
        );

        assert_eq!(price, Some(99));
    }

    #[test]
    fn slide_moves_a_crossing_bid_one_tick_under_the_best_ask() {
        let price = post_only_book_price(
            PostOnlyParam::Slide,
            PositionDirection::Long,
            120,
            Some(100),
            5,
        );

        assert_eq!(price, Some(95));
    }

    #[test]
    fn slide_moves_a_crossing_ask_one_tick_over_the_best_bid() {
        let price = post_only_book_price(
            PostOnlyParam::Slide,
            PositionDirection::Short,
            80,
            Some(100),
            5,
        );

        assert_eq!(price, Some(105));
    }

    #[test]
    fn an_empty_opposite_side_keeps_the_price() {
        let price =
            post_only_book_price(PostOnlyParam::Slide, PositionDirection::Short, 80, None, 5);
        assert_eq!(price, Some(80));
    }
}
