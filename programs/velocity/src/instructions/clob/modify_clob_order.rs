//! `modify_clob_order` — reprice/resize a resting CLOB order.
//!
//! `modify_order` only ever touched `User.orders`, so a maker whose order
//! lives on the book had no modify route at all: they had to send
//! `cancel_clob_order` and `place_clob_order` as two instructions, which
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
//! Semantics deliberately mirror `place_clob_order` for the replacement leg
//! (same margin gate, same activation-delay attestation rule, same wake
//! hints) and `cancel_clob_order` for the removal leg (not gated on the
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
            increase_open_bids_and_asks,
        },
        error::ErrorCode,
        instructions::{
            constraints::*,
            optional_accounts::{load_maps, AccountMaps},
        },
        load_mut,
        math::{margin::meets_place_order_margin_requirement, orders::is_order_position_reducing},
        msg,
        signer::CLOB_AUTHORITY_SEED,
        state::{
            market_status::MarketStatus,
            perp_market_map::MarketSet,
            prop_amm::{
                ClobCancelOrderArgsV0, ClobMarket, ClobOrderRefV0, ClobPlaceOrderArgsV0,
                ClobUserRefV0, QuoterV0, WireDirectionExt,
            },
            state::State,
            user::User,
        },
        validate,
    },
    anchor_lang::prelude::*,
};

#[derive(Accounts)]
#[instruction(params: ModifyClobOrderParams)]
pub struct ModifyClobOrder<'info> {
    pub state: AccountLoader<'info, State>,
    #[account(
        mut,
        constraint = can_sign_for_user(&user, &authority)?
    )]
    pub user: AccountLoader<'info, User>,
    pub authority: Signer<'info>,
    /// The book's registry entry. The replacement leg additionally requires it
    /// to be active and approved.
    pub quoter: AccountLoader<'info, QuoterV0>,
    /// CHECK: validated against the quoter entry's registered execute
    /// accounts in the handler.
    #[account(mut)]
    pub clob_market: UncheckedAccount<'info>,
    /// CHECK: locked to the registered quoter program.
    #[account(address = quoter.load()?.program_id)]
    pub clob_program: UncheckedAccount<'info>,
    /// CHECK: the CLOB place authority PDA — what a book's `place_authority`
    /// is set to. Its own key, distinct from the per-entry signer a
    /// third-party quoter is handed: signer privilege is inherited by a
    /// callee, and this one may place and cancel on any book, for any user.
    #[account(seeds = [CLOB_AUTHORITY_SEED], bump)]
    pub clob_authority: UncheckedAccount<'info>,
    /// CHECK: the instructions sysvar, locked by address. Required only for a
    /// faster-than-default activation delay on the replacement.
    #[account(address = solana_program::sysvar::instructions::ID)]
    pub instructions_sysvar: Option<UncheckedAccount<'info>>,
}

#[derive(Clone, Copy, AnchorSerialize, AnchorDeserialize)]
pub struct ModifyClobOrderParams {
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
    /// Same rule as `place_clob_order`: `None` takes the book's default speed
    /// bump, anything below it needs the flow-authority attestation.
    pub activation_delay_slots: Option<u32>,
    /// Same rule as `place_clob_order`: refuse the replacement rather than
    /// rest it crossed. The original is already off the book when this fires,
    /// so a refused replacement leaves the maker with no order — which is what
    /// a maker repricing into a crossed book is asking for.
    pub reject_if_crossed: bool,
}

#[access_control(
    exchange_not_paused(&ctx.accounts.state)
)]
pub fn handle_modify_clob_order<'c: 'info, 'info>(
    ctx: Context<'info, ModifyClobOrder<'info>>,
    params: ModifyClobOrderParams,
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

    let clob = {
        let quoter = ctx.accounts.quoter.load()?;
        // The replacement adds flow to the book, so it answers to the same
        // gate a fresh placement does.
        validate!(
            quoter.is_active && quoter.is_approved,
            ErrorCode::DefaultError,
            "CLOB quoter is not active and approved; cancel the order instead"
        )?;
        ClobMarket::from_quoter(
            &quoter,
            params.market_index,
            &ctx.accounts.clob_market,
            &ctx.accounts.clob_program,
            &ctx.accounts.clob_authority,
            ctx.bumps.clob_authority,
        )?
    };
    validate!(
        matches!(
            perp_market_map.get_ref(&params.market_index)?.status,
            MarketStatus::Active
        ),
        ErrorCode::MarketPlaceOrderPaused,
        "market not active"
    )?;

    // The attestation rule is the replacement's, not the original's: a modify
    // that asks for a faster-than-default bump is a new fast placement.
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

    let user_ref = {
        let user = crate::load!(ctx.accounts.user)?;
        ClobUserRefV0 {
            authority: user.authority,
            sub_account_id: user.sub_account_id.into(),
        }
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

    // ---- Re-reserve the aggregates net of the cancel, then gate margin
    // exactly like a placement. Both legs are inside this transaction, so a
    // failure unwinds the cancel with it — the maker never ends up flat. ----
    {
        let mut user = load_mut!(ctx.accounts.user)?;
        validate!(
            !user.is_bankrupt(),
            ErrorCode::UserBankrupt,
            "user bankrupt"
        )?;
        let position_index = get_position_index(&user.perp_positions, params.market_index)
            .or_else(|_| add_new_position(&mut user.perp_positions, params.market_index))?;
        decrease_open_bids_and_asks(
            &mut user.perp_positions[position_index],
            &direction,
            removed.base_asset_amount,
            true,
        )?;
        increase_open_bids_and_asks(
            &mut user.perp_positions[position_index],
            &direction,
            base_asset_amount,
            true,
        )?;
        // The order count is unchanged — one order out, one order in — so
        // neither the position counter nor `User.open_orders` moves. A placed
        // trigger's shadow slot is likewise untouched: the shadow keeps the
        // trigger params and only its CLOB ref changes, which the re-stamp
        // below does.
        let risk_increasing = !is_order_position_reducing(
            &direction,
            base_asset_amount,
            user.perp_positions[position_index].base_asset_amount,
        )?;
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
        user.update_last_active_slot(clock.slot);
    }

    let order_ref = clob.place(ClobPlaceOrderArgsV0 {
        side: removed.side,
        price,
        base_asset_amount,
        activation_delay_slots: params.activation_delay_slots,
        max_ts,
        user: user_ref,
        // Not a migrated taker remainder: this price is its owner's choice.
        taker_origin: false,
        // A modify keeps the order's identity: same id before and after, so a
        // reprice is one order that moved rather than two orders. A placed
        // trigger forces it — its shadow slot keeps the id it armed under, and
        // a new one here would leave the slot naming an order nobody holds.
        client_order_id: removed.client_order_id,
        reject_if_crossed: params.reject_if_crossed,
    })?;

    // A placed trigger's shadow follows its live order to the new handle;
    // without the re-stamp the shadow would point at a dead node and every
    // later removal path would fail to find it. The size is restated as the
    // replacement's total with `base_asset_amount_filled` cleared, keeping the
    // shadow's unfilled amount equal to the live order's size — that is what
    // an eviction re-arms on.
    {
        let mut user = load_mut!(ctx.accounts.user)?;
        if let Some(index) = user.find_placed_trigger_slot(params.market_index, removed.order_id) {
            let order = &mut user.orders[index];
            order.set_clob_order_ref(order_ref.node_index, order_ref.order_id);
            order.base_asset_amount = base_asset_amount;
            order.base_asset_amount_filled = 0;
            order.price = price;
            order.max_ts = max_ts;
        }
    }

    // One record, not a cancel and a place: the order kept its id, so to a
    // reader it is the same order at new terms.
    super::emit_clob_place_record(
        clock.unix_timestamp,
        &ctx.accounts.user.key(),
        super::ClobOrderFacts {
            order_id: removed.client_order_id,
            market_index: params.market_index,
            direction: removed.side.to_position_direction(),
            price,
            base_asset_amount,
            base_asset_amount_filled: 0,
            max_ts,
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
