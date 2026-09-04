//! `fill_legacy_dlob_order` — the keeper fill for live orders in
//! `User.orders`.
//!
//! Same fill as `fill_perp_order`, plus the market's CLOB accounts: the
//! router prices the book beside the vAMM and the DLOB makers, and a
//! restable remainder migrates to the book instead of resting in
//! `User.orders`.
//!
//! Only the legacy endpoints still create live orders in `User.orders`:
//! v0 `place_perp_order` rests and auctions, and stops that v0
//! `trigger_order` fired. The v1 taker flows fill ephemeral orders and rest
//! remainders on the book directly, so their orders never need this crank.
//! Each fill here moves a remainder off the DLOB, so this instruction
//! drains the legacy book. It is deleted together with the legacy placement
//! and trigger endpoints.
//!
//! Cheap in accounts, which is what makes it viable on the most
//! account-pressured instruction in the program: a router fill already
//! carries the quoter slab, the book, the clob program and the quoter signer,
//! because the market's canonical CLOB is a mandatory baseline.
//!
//! A market-order remainder migrates too, resting at `auction_end_price` —
//! the worst fill it already agreed to, and the only price it has. That is
//! only safe because a migrated remainder is taker-origin: it cannot be taken
//! while a live counterparty crosses it, and a cross settles at the
//! counterparty's price, so a maker arriving during the activation window
//! competes on price instead of on transaction landing. Resting at a slippage
//! bound without that is a free option written at the taker's worst price.
//!
//! An `OrderType::Oracle` remainder does not migrate — an oracle-floating
//! price has nothing fixed to rest at. See `docs/taker-remainder-auction.md`.

use {
    crate::{
        error::ErrorCode,
        instructions::{constraints::*, keeper::FillAccounts, ClobRemainderRoute},
        load,
        state::{
            prop_amm::QuoterSlabV0,
            state::State,
            user::{User, UserStats},
        },
        validate,
    },
    anchor_lang::prelude::*,
};

#[derive(Accounts)]
pub struct FillLegacyDlobOrder<'info> {
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
    #[account(mut)]
    pub user: AccountLoader<'info, User>,
    #[account(
        mut,
        constraint = is_stats_for_user(&user, &user_stats)?
    )]
    pub user_stats: AccountLoader<'info, UserStats>,
    /// The market's quoter slab — a remainder only ever rests on the vetted
    /// book its `Clob` slot names.
    pub quoter_slab: AccountLoader<'info, QuoterSlabV0>,
    /// CHECK: validated against the book slot's registered response account
    /// (`ClobMarket::from_slab`), so a valid slot cannot be pointed at an
    /// arbitrary account.
    #[account(mut)]
    pub clob_market: UncheckedAccount<'info>,
    /// CHECK: a Clob slot's program is pinned to velocity's CLOB at
    /// registration; the handler re-checks through the slot.
    #[account(address = crate::ids::clob_program::id())]
    pub clob_program: UncheckedAccount<'info>,
    /// CHECK: address-locked to the instructions sysvar. Read for two facts a
    /// fill cannot get anywhere else: whether the taker signed this
    /// transaction, and how many accounts the transaction locks.
    ///
    /// Optional, and it costs one of those locks. A fill needs it only when a
    /// book withholds depth for an owner the transaction does not carry, and
    /// only when the taker did not sign. A fill that meets neither condition
    /// passes `None` and spends nothing. A fill that meets both and passes
    /// `None` is refused, because the obligation cannot be checked.
    #[account(address = ::solana_program::sysvar::instructions::ID)]
    pub instructions_sysvar: Option<UncheckedAccount<'info>>,
}

#[derive(Clone, AnchorSerialize, AnchorDeserialize)]
pub struct FillLegacyDlobOrderArgs {
    pub market_index: u16,
    /// The DLOB order to fill. `None` fills the user's most recent order.
    pub order_id: Option<u32>,
    /// The taker's signed route, when the fill claims one. Empty claims the
    /// market baseline.
    pub signed_route: Vec<Pubkey>,
}

/// `market_index` is checked against the order's own market below, so a
/// mismatch is a malformed transaction rather than a wrong book.
#[access_control(
    fill_not_paused(&ctx.accounts.state)
)]
pub fn handle_fill_legacy_dlob_order<'c: 'info, 'info>(
    ctx: Context<'info, FillLegacyDlobOrder<'info>>,
    args: FillLegacyDlobOrderArgs,
) -> Result<()> {
    let FillLegacyDlobOrderArgs {
        market_index,
        order_id,
        signed_route,
    } = args;
    let (order_id, order_market_index) = {
        let user = &load!(ctx.accounts.user)?;
        let order_id = order_id.unwrap_or_else(|| user.get_last_order_id());
        match user.get_order(order_id) {
            Some(order) => (order_id, order.market_index),
            None => {
                msg!("Order does not exist {}", order_id);
                return Ok(());
            }
        }
    };
    validate!(
        order_market_index == market_index,
        ErrorCode::DefaultError,
        "fill is for market {} but the order is on market {}",
        market_index,
        order_market_index
    )?;

    // Who built this transaction, and what room it had. A taker that signs
    // chose its own account list; a taker that does not is trusting the filler.
    let obligation = {
        let taker = load!(ctx.accounts.user)?;
        crate::math::router::FillerObligation {
            taker_signed: taker.authority == ctx.accounts.authority.key()
                || (taker.delegate == ctx.accounts.authority.key()
                    && taker.delegate != Pubkey::default()),
            tx_accounts: ctx
                .accounts
                .instructions_sysvar
                .as_ref()
                .map(|sysvar| {
                    crate::instructions::optional_accounts::tx_writable_lock_count(sysvar)
                })
                .transpose()?,
            // Set after the route is assembled: only then is it known which
            // entries the transaction carried.
            unrouted_quoters: 0,
        }
    };

    let user_key = &ctx.accounts.user.key();
    // A keeper fill is never attested flow: the attestation transports are
    // the flow authority signing a swift-built transaction, or a detached
    // attestation bound to a signed-message order — a legacy slot order has
    // neither. On a bumped book the route quotes the book as empty and the
    // restable remainder migrates into the auction.
    let taker_served_window = false;
    crate::instructions::keeper::fill_legacy_dlob_order_entry(
        FillAccounts {
            state: &ctx.accounts.state,
            filler: &ctx.accounts.filler,
            filler_stats: &ctx.accounts.filler_stats,
            user: &ctx.accounts.user,
            user_stats: &ctx.accounts.user_stats,
        },
        ctx.remaining_accounts,
        order_id,
        market_index,
        signed_route,
        obligation,
        taker_served_window,
        Some(ClobRemainderRoute {
            quoter_slab: &ctx.accounts.quoter_slab,
            clob_market: &ctx.accounts.clob_market,
            clob_program: &ctx.accounts.clob_program,
        }),
    )
    .inspect_err(|_e| {
        msg!(
            "Err filling order id {} for user {} for market index {}",
            order_id,
            user_key,
            market_index
        );
    })?;

    Ok(())
}
