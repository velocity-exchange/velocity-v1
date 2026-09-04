//! `modify_order_v1` — reprice/resize a resting CLOB order.
//!
//! `modify_order` only ever touched `User.orders`, so a maker whose order
//! lives on the book had no modify route at all: they had to send
//! `cancel_order_v1` and `place_and_make_perp_order_v1` as two instructions, which
//! surrenders queue position between them and can leave the maker flat if the
//! second one fails.
//!
//! This is cancel-and-replace in one instruction, in that order — the CLOB
//! has no in-place mutation, and a modify is a new order at the back of its
//! price level either way. What the single instruction buys is atomicity and
//! one margin gate over the *net* change: the cancelled size is unwound from
//! the open-order aggregates before the replacement reserves its own, so a
//! same-size reprice never has to pass margin for double the exposure the way
//! place-then-cancel would.
//!
//! Semantics deliberately mirror `place_and_make_perp_order_v1` for the
//! replacement leg
//! (same margin gate, same activation-delay attestation rule, same wake
//! hints) and `cancel_order_v1` for the removal leg (not gated on the
//! quoter entry's active/approved flags — but the *replacement* is, so a
//! killed book can only be modified in the shrinking direction… which is to
//! say: on a killed book, modify fails and cancel is the way out).
//!
//! `None` fields keep the resting order's value, so a pure reprice doesn't
//! have to restate the size. The replacement's size is always the *new* total,
//! not a delta, and is measured against what the cancel returned — so a
//! partially-filled order modifies against its remaining size, never its
//! original.

use {
    crate::{
        controller::position::{
            add_new_position, decrease_open_bids_and_asks, get_position_index,
            increase_open_bids_and_asks, PositionDirection,
        },
        error::ErrorCode,
        instructions::{
            constraints::*,
            optional_accounts::{load_maps, AccountMaps},
        },
        load_mut,
        math::{margin::meets_place_order_margin_requirement, orders::is_order_position_reducing},
        msg,
        state::{
            market_status::MarketStatus,
            oracle_map::OracleMap,
            perp_market_map::{MarketSet, PerpMarketMap},
            prop_amm::{
                ClobCancelOrderArgsV0, ClobMarket, ClobOrderRefV0, ClobPlaceOrderArgsV0,
                ClobRemovedOrderV0, QuoterSlabExt, QuoterSlabV0, WireDirectionExt,
            },
            spot_market_map::SpotMarketMap,
            state::State,
            user::User,
        },
        validate,
    },
    anchor_lang::prelude::*,
};

#[derive(Accounts)]
#[instruction(params: ModifyOrderV1Params)]
pub struct ModifyOrderV1<'info> {
    pub state: AccountLoader<'info, State>,
    #[account(
        mut,
        constraint = can_sign_for_user(&user, &authority)?
    )]
    pub user: AccountLoader<'info, User>,
    pub authority: Signer<'info>,
    /// The book's registry entry. The replacement leg additionally requires it
    /// to be active and approved.
    /// The market's quoter slab; the book's config is its `Clob` slot.
    #[account(
        has_one = clob_market,
        constraint = quoter_slab.load()?.market == params.market_index,
    )]
    pub quoter_slab: AccountLoader<'info, QuoterSlabV0>,
    /// CHECK: the slab's `has_one` binds it to the book the admin approved.
    #[account(mut)]
    pub clob_market: UncheckedAccount<'info>,
    /// CHECK: a Clob slot's program is pinned to velocity's CLOB at
    /// registration; the handler re-checks through the slot.
    #[account(address = crate::ids::clob_program::id())]
    pub clob_program: UncheckedAccount<'info>,
    /// The flow authority, signing this transaction as a named account.
    /// Required only for a faster-than-default activation delay on the
    /// replacement — presence is the attestation. The zero key cannot sign,
    /// so an unset flow authority admits nobody.
    #[account(
        constraint = flow_authority.key()
            == state.load()?.hot_key(crate::state::state::HotRole::FlowAuthority)
            @ crate::error::ErrorCode::UnattestedFastActivation
    )]
    pub flow_authority: Option<Signer<'info>>,
}

#[derive(Clone, Copy, AnchorSerialize, AnchorDeserialize)]
pub struct ModifyOrderV1Params {
    pub market_index: u16,
    /// Handle for the order being modified; the CLOB fails closed on a stale
    /// hint, and velocity fails the whole call if the removal hit anyone else.
    pub order_ref: ClobOrderRefV0,
    /// `None` keeps the resting price.
    pub price: Option<u64>,
    /// `None` keeps the *remaining* size of the resting order (not its
    /// original size).
    pub base_asset_amount: Option<u64>,
    /// `None` keeps the resting expiry (read off the book node before the
    /// cancel — the CLOB's removal response doesn't carry it). `Some(0)` makes
    /// the replacement good-till-cancelled.
    pub max_ts: Option<i64>,
    /// Same rule as `place_and_make_perp_order_v1`: `None` takes the book's default speed
    /// bump, anything below it needs the flow-authority attestation.
    pub activation_delay_slots: Option<u32>,
    /// Same rule as `place_and_make_perp_order_v1`: refuse the replacement rather than
    /// rest it crossed. The original is already off the book when this fires,
    /// so a refused replacement leaves the maker with no order — which is what
    /// a maker repricing into a crossed book is asking for.
    pub reject_if_crossed: bool,
}

#[access_control(
    exchange_not_paused(&ctx.accounts.state)
)]
pub fn handle_modify_order_v1<'c: 'info, 'info>(
    ctx: Context<'info, ModifyOrderV1<'info>>,
    params: ModifyOrderV1Params,
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
        state.slot_clock(),
        Some(state.oracle_guard_rails),
    )?;

    let clob = bind_book_for_replacement(
        &ctx.accounts.quoter_slab,
        &ctx.accounts.clob_market,
        &ctx.accounts.clob_program,
        &perp_market_map,
        params.market_index,
    )?;

    // The attestation rule is the replacement's, not the original's: a modify
    // that asks for a faster-than-default bump is a new fast placement.
    crate::instructions::attest_activation_delay(
        &ctx.accounts.quoter_slab,
        params.market_index,
        params.activation_delay_slots,
        ctx.accounts.flow_authority.is_some(),
    )?;

    let user_ref = {
        let user = crate::load!(ctx.accounts.user)?;
        user.clob_user_ref()
    };

    // ---- Cancel first, so the margin gate below sees the net change. ----
    let removed = clob.cancel(ClobCancelOrderArgsV0 {
        order_ref: params.order_ref,
        user: user_ref,
        force: false,
    })?;
    validate!(
        removed.user == user_ref,
        ErrorCode::DefaultError,
        "clob cancelled an order for {}/{} instead of the passed user",
        removed.user.authority,
        removed.user.sub_account_id
    )?;

    let terms = resolve_replacement_terms(&params, &removed)?;

    reserve_replacement_margin(
        &ctx.accounts.user,
        &perp_market_map,
        &spot_market_map,
        &mut oracle_map,
        params.market_index,
        &terms,
        removed.base_asset_amount,
        clock.slot,
    )?;

    let order_ref = clob.place(ClobPlaceOrderArgsV0 {
        side: removed.side,
        price: terms.price,
        base_asset_amount: terms.base_asset_amount,
        activation_delay_slots: params.activation_delay_slots,
        max_ts: terms.max_ts,
        user: user_ref,
        // A normal order rests at its owner's chosen price, so a modify makes
        // it an ordinary maker quote. A reduce-only order stays taker-origin:
        // reduce-only CLOB orders are taker-origin by construction, and every
        // path that fills or removes one relies on that to keep the owner's
        // reduce-only counter balanced. A modify must not break the invariant.
        taker_origin: removed.reduce_only,
        // A modify keeps the order's identity: same id before and after, so a
        // reprice is one order that moved rather than two orders. A placed
        // trigger forces it — its shadow slot keeps the id it armed under, and
        // a new one here would leave the slot naming an order nobody holds.
        client_order_id: removed.client_order_id,
        reject_if_crossed: params.reject_if_crossed,
        // A modify keeps the order's reduce-only status, or the replacement
        // would rest uncapped on a book that clamps only reduce-only fills.
        reduce_only: removed.reduce_only,
    })?;

    restamp_placed_trigger_shadow(
        &ctx.accounts.user,
        params.market_index,
        removed.order_id,
        order_ref,
        &terms,
    )?;

    // One record, not a cancel and a place: the order kept its id, so to a
    // reader it is the same order at new terms.
    super::emit_clob_place_record(
        clock.unix_timestamp,
        &ctx.accounts.user.key(),
        super::ClobOrderFacts {
            order_id: removed.client_order_id,
            market_index: params.market_index,
            direction: removed.side.to_position_direction(),
            price: terms.price,
            base_asset_amount: terms.base_asset_amount,
            base_asset_amount_filled: 0,
            max_ts: terms.max_ts,
            slot: clock.slot,
            taker_origin: removed.taker_origin,
        },
    )?;

    msg!(
        "modified clob order {} into {} (node {}) for user {}",
        removed.order_id,
        order_ref.order_id,
        order_ref.node_index,
        ctx.accounts.user.key()
    );
    Ok(())
}

/// Bind the book the market's slab names, and check that the book and the
/// market both take new flow. The replacement leg is a placement, so it must
/// pass the gates a placement passes.
fn bind_book_for_replacement<'a, 'info>(
    quoter_slab: &'a AccountLoader<'info, QuoterSlabV0>,
    clob_market: &'a AccountInfo<'info>,
    clob_program: &'a AccountInfo<'info>,
    perp_market_map: &PerpMarketMap<'_>,
    market_index: u16,
) -> Result<ClobMarket<'a, 'info>> {
    let clob = {
        let slot = quoter_slab.clob_slot(market_index)?;
        // The replacement adds flow to the book, so it answers to the same
        // gate a fresh placement does.
        validate!(
            slot.quotes(),
            ErrorCode::DefaultError,
            "CLOB quoter is not active and approved; cancel the order instead"
        )?;
        drop(slot);
        ClobMarket::from_slab(quoter_slab, market_index, clob_market, clob_program)?
    };
    validate!(
        matches!(
            perp_market_map.get_ref(&market_index)?.status,
            MarketStatus::Active
        ),
        ErrorCode::MarketPlaceOrderPaused,
        "market not active"
    )?;
    Ok(clob)
}

/// The terms the replacement order rests with. Each field is either the
/// parameter the caller sent or the value the removed order carried.
struct ReplacementTerms {
    direction: PositionDirection,
    price: u64,
    base_asset_amount: u64,
    max_ts: i64,
}

/// Read the replacement's terms from the parameters and the removed order. A
/// `None` parameter keeps the removed order's value.
fn resolve_replacement_terms(
    params: &ModifyOrderV1Params,
    removed: &ClobRemovedOrderV0,
) -> Result<ReplacementTerms> {
    // The side is not modifiable: flipping a bid to an ask is a different
    // order (and a different risk decision), so it goes through cancel +
    // place. Carrying the removed order's side also means the replacement
    // can't be tricked onto the wrong book side by a stale hint.
    let direction = removed.side.to_position_direction();
    let price = params.price.unwrap_or(removed.price);
    let base_asset_amount = params
        .base_asset_amount
        .unwrap_or(removed.base_asset_amount);
    // `None` means keep the expiry the order was resting with, which the
    // removal reports — the only moment it is still knowable, and the reason
    // it is on that response rather than read off the node.
    let max_ts = params.max_ts.unwrap_or(removed.max_ts);
    validate!(
        base_asset_amount > 0 && price > 0,
        ErrorCode::InvalidOrder,
        "modify must leave a live order: price {} size {}",
        price,
        base_asset_amount
    )?;
    Ok(ReplacementTerms {
        direction,
        price,
        base_asset_amount,
        max_ts,
    })
}

/// Re-reserve the aggregates net of the cancel, then gate margin exactly like
/// a placement. Both legs are inside this transaction, so a failure unwinds
/// the cancel with it — the maker never ends up flat.
#[allow(clippy::too_many_arguments)]
fn reserve_replacement_margin<'info>(
    user_loader: &AccountLoader<'info, User>,
    perp_market_map: &PerpMarketMap<'_>,
    spot_market_map: &SpotMarketMap<'_>,
    oracle_map: &mut OracleMap<'_>,
    market_index: u16,
    terms: &ReplacementTerms,
    cancelled_base_asset_amount: u64,
    slot: u64,
) -> Result<()> {
    let mut user = load_mut!(user_loader)?;
    validate!(
        !user.is_bankrupt(),
        ErrorCode::UserBankrupt,
        "user bankrupt"
    )?;
    let position_index = get_position_index(&user.perp_positions, market_index)
        .or_else(|_| add_new_position(&mut user.perp_positions, market_index))?;
    decrease_open_bids_and_asks(
        &mut user.perp_positions[position_index],
        &terms.direction,
        cancelled_base_asset_amount,
        true,
    )?;
    increase_open_bids_and_asks(
        &mut user.perp_positions[position_index],
        &terms.direction,
        terms.base_asset_amount,
        true,
    )?;
    // The order count is unchanged — one order out, one order in — so
    // neither the position counter nor `User.open_orders` moves. A placed
    // trigger's shadow slot is likewise untouched: the shadow keeps the
    // trigger params and only its CLOB ref changes, which the re-stamp
    // below does.
    let risk_increasing = !is_order_position_reducing(
        &terms.direction,
        terms.base_asset_amount,
        user.perp_positions[position_index].base_asset_amount,
    )?;
    let isolated_market_index = (risk_increasing
        && user.perp_positions[position_index].is_isolated())
    .then_some(market_index);
    meets_place_order_margin_requirement(
        &user,
        perp_market_map,
        spot_market_map,
        oracle_map,
        risk_increasing,
        isolated_market_index,
    )?;
    user.update_last_active_slot(slot);
    Ok(())
}

/// A placed trigger's shadow follows its live order to the new handle;
/// without the re-stamp the shadow would point at a dead node and every
/// later removal path would fail to find it. The size is restated as the
/// replacement's total with `base_asset_amount_filled` cleared, keeping the
/// shadow's unfilled amount equal to the live order's size — that is what
/// an eviction re-arms on.
fn restamp_placed_trigger_shadow<'info>(
    user_loader: &AccountLoader<'info, User>,
    market_index: u16,
    cancelled_order_id: u64,
    order_ref: ClobOrderRefV0,
    terms: &ReplacementTerms,
) -> Result<()> {
    let mut user = load_mut!(user_loader)?;
    if let Some(index) = user.find_placed_trigger_slot(market_index, cancelled_order_id) {
        let order = &mut user.orders[index];
        order.set_clob_order_ref(order_ref.node_index, order_ref.order_id);
        order.base_asset_amount = terms.base_asset_amount;
        order.base_asset_amount_filled = 0;
        order.price = terms.price;
        order.max_ts = terms.max_ts;
    }
    Ok(())
}
