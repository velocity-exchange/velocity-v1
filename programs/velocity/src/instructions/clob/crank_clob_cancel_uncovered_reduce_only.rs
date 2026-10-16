//! `crank_clob_cancel_uncovered_reduce_only`. The crank cancels a reduce-only
//! book order whose owner holds no position that the order reduces.
//!
//! The router caps a reduce-only fill to the owner's live position on the
//! order's side, through `position_cover`. When the owner closes that position
//! some other way, the order can never fill, but it stays on the book. Force
//! cancel cannot remove it while the account passes margin. Such orders fill the
//! row windows that the bid/ask TWAP crank and `book_side_rested` read, so a
//! stack of them hides the real book. The crank cancels only at zero cover,
//! because a partial close leaves a larger reduce-only order that still fills in
//! part. The owner pays the removal fee, and a placed trigger's shadow is freed,
//! as in the band crank. The owner's liveness poll stays armed while the order
//! rests, and the liquidation resolver stages this crank in program-keeper mode
//! for an account that passes maintenance and has no other work.

use {
    super::helpers::crank_common::{
        crank_clob_removal, payment_sol_price, program_keeper_mode, ClobRemoval, RemovalAccounts,
    },
    crate::{
        error::ErrorCode,
        instructions::{constraints::*, router::user_caps::position_cover},
        load,
        state::{
            clob_crank::{ClobCrankConditionsV0, CLOB_CRANK_CONDITIONS_PDA_SEED},
            perp_market::PerpMarket,
            prop_amm::{CancelOrderArgsV0, ClobOrderRefV0, ClobReader, OrderViewV0, QuoterSlabV0},
            spot_market::SpotMarket,
            state::State,
            user::{User, UserStats},
        },
        validate,
    },
    anchor_lang::prelude::*,
};

#[derive(Clone, Copy, AnchorSerialize, AnchorDeserialize)]
pub struct CrankClobCancelUncoveredReduceOnlyArgs {
    pub market_index: u16,
    /// The reduce-only order. The crank reads it back from the book.
    pub order_ref: ClobOrderRefV0,
}

#[derive(Accounts)]
#[instruction(args: CrankClobCancelUncoveredReduceOnlyArgs)]
pub struct CrankClobCancelUncoveredReduceOnly<'info> {
    pub state: AccountLoader<'info, State>,
    /// CHECK: in signed-keeper mode this must sign for `filler`, which the
    /// constraint below enforces. In program-keeper mode it is only the lamport
    /// payout target, and no signature is required.
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
    /// The owner of the order. The crank reads its position for the cover.
    #[account(mut)]
    pub user: AccountLoader<'info, User>,
    #[account(
        mut,
        has_one = quoter_slab,
        has_one = clob_market,
        constraint = perp_market.load()?.market_index == args.market_index
            @ ErrorCode::PerpMarketAccountMismatch
    )]
    pub perp_market: AccountLoader<'info, PerpMarket>,
    pub quoter_slab: AccountLoader<'info, QuoterSlabV0>,
    /// CHECK: the perp market's `has_one` binds it to the book the market
    /// designated.
    #[account(mut)]
    pub clob_market: UncheckedAccount<'info>,
    /// CHECK: a Clob slot's program is pinned to velocity's CLOB at
    /// registration. The handler checks it again through the slot.
    #[account(address = crate::ids::clob_program::id())]
    pub clob_program: UncheckedAccount<'info>,
    /// The market's lamport reservoir. Program-keeper mode requires it.
    #[account(
        mut,
        seeds = [
            CLOB_CRANK_CONDITIONS_PDA_SEED,
            args.market_index.to_le_bytes().as_ref(),
        ],

        bump
    )]
    pub crank_conditions: Option<AccountLoader<'info, ClobCrankConditionsV0>>,
    /// The SOL spot market, whose TWAP values the reservoir payment in quote.
    /// Program-keeper mode requires it when `State` names a SOL market.
    #[account(
        seeds = [
            b"spot_market",
            state.load()?.sol_spot_market_index.to_le_bytes().as_ref(),
        ],

        bump
    )]
    pub sol_spot_market: Option<AccountLoader<'info, SpotMarket>>,
}

/// A taker-origin order passes the owner's signed-message record as the one
/// remaining account, so the cancel releases its entry.
#[access_control(
    fill_not_paused(&ctx.accounts.state)
)]
pub fn handle_crank_clob_cancel_uncovered_reduce_only<'info>(
    ctx: Context<'info, CrankClobCancelUncoveredReduceOnly<'info>>,
    args: CrankClobCancelUncoveredReduceOnlyArgs,
) -> Result<()> {
    let CrankClobCancelUncoveredReduceOnlyArgs {
        market_index,
        order_ref,
    } = args;

    let order = ClobReader {
        market: &ctx.accounts.clob_market,
        program: &ctx.accounts.clob_program,
    }
    .orders(vec![order_ref])?
    .first()
    .copied()
    .filter(|order| order.found())
    .ok_or(ErrorCode::OrderDoesNotExist)?;

    require_uncovered_reduce_only(&*load!(ctx.accounts.user)?, market_index, &order)?;

    let accounts = &*ctx.accounts;
    let sol_price = payment_sol_price(
        &*accounts.state.load()?,
        program_keeper_mode(
            &accounts.filler,
            &accounts.state,
            accounts.crank_conditions.is_some(),
        )?,
        accounts.sol_spot_market.as_ref(),
    )?;
    crank_clob_removal(
        &RemovalAccounts {
            state: &accounts.state,
            authority: &accounts.authority,
            filler: &accounts.filler,
            user: &accounts.user,
            perp_market: &accounts.perp_market,
            quoter_slab: &accounts.quoter_slab,
            clob_market: &accounts.clob_market,
            clob_program: &accounts.clob_program,
            crank_conditions: &accounts.crank_conditions,
            signed_msg_record: ctx.remaining_accounts.first(),
            trigger_conditions: None,
        },
        market_index,
        ClobRemoval::UncoveredReduceOnly(CancelOrderArgsV0 {
            order_ref,
            user: order.user,
            force: false,
        }),
        sol_price,
    )
}

/// Refuse the cancel unless `order` is a reduce-only order of `owner` that no
/// part of the owner's position covers. The owner is checked first, because
/// another user's position says nothing about the order.
fn require_uncovered_reduce_only(
    owner: &User,
    market_index: u16,
    order: &OrderViewV0,
) -> Result<()> {
    validate!(
        order.user == owner.clob_user_ref(),
        ErrorCode::InvalidUserAccount,
        "order {} belongs to {}/{}",
        order.order_ref.order_id,
        order.user.authority,
        order.user.sub_account_id
    )?;

    validate!(
        order.reduce_only,
        ErrorCode::ClobOrderNotReduceOnly,
        "order {} is not reduce-only",
        order.order_ref.order_id
    )?;

    let cover = position_cover(owner, market_index, order.side);
    validate!(
        cover == 0,
        ErrorCode::ReduceOnlyOrderStillCovered,
        "the owner's position covers {} of order {}",
        cover,
        order.order_ref.order_id
    )?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use {
        super::require_uncovered_reduce_only,
        crate::{
            error::ErrorCode,
            state::{
                prop_amm::{OrderViewV0, SideV0},
                user::{PerpPosition, User},
            },
        },
        anchor_lang::prelude::Pubkey,
    };

    const MARKET_INDEX: u16 = 0;

    fn owner_with_position(base_asset_amount: i64) -> User {
        let mut owner = User {
            authority: Pubkey::new_unique(),
            ..User::default()
        };
        owner.perp_positions[0] = PerpPosition {
            market_index: MARKET_INDEX,
            base_asset_amount,
            open_orders: 1,
            ..PerpPosition::default()
        };
        owner
    }

    fn order(owner: &User, side: SideV0, reduce_only: bool) -> OrderViewV0 {
        OrderViewV0 {
            user: owner.clob_user_ref(),
            side,
            base_asset_amount: 1_000_000_000,
            reduce_only,
            ..OrderViewV0::NONE
        }
    }

    #[test]
    fn a_reduce_only_bid_of_a_flat_owner_is_cancellable() {
        let owner = owner_with_position(0);
        let bid = order(&owner, SideV0::Bid, true);
        assert!(require_uncovered_reduce_only(&owner, MARKET_INDEX, &bid).is_ok());
    }

    #[test]
    fn an_ordinary_order_is_refused() {
        let owner = owner_with_position(0);
        let bid = order(&owner, SideV0::Bid, false);
        assert_eq!(
            require_uncovered_reduce_only(&owner, MARKET_INDEX, &bid),
            Err(ErrorCode::ClobOrderNotReduceOnly.into())
        );
    }

    /// A short smaller than the bid still lets the bid fill in part.
    #[test]
    fn a_partly_covered_reduce_only_bid_is_refused() {
        let owner = owner_with_position(-1);
        let bid = order(&owner, SideV0::Bid, true);
        assert_eq!(
            require_uncovered_reduce_only(&owner, MARKET_INDEX, &bid),
            Err(ErrorCode::ReduceOnlyOrderStillCovered.into())
        );
    }

    /// A long covers a reduce-only ask and not a reduce-only bid.
    #[test]
    fn a_position_on_the_other_side_covers_nothing() {
        let owner = owner_with_position(1_000_000_000);
        let bid = order(&owner, SideV0::Bid, true);
        assert!(require_uncovered_reduce_only(&owner, MARKET_INDEX, &bid).is_ok());

        let ask = order(&owner, SideV0::Ask, true);
        assert_eq!(
            require_uncovered_reduce_only(&owner, MARKET_INDEX, &ask),
            Err(ErrorCode::ReduceOnlyOrderStillCovered.into())
        );
    }

    #[test]
    fn another_users_order_is_refused() {
        let owner = owner_with_position(0);
        let other = owner_with_position(0);
        let bid = order(&other, SideV0::Bid, true);
        assert_eq!(
            require_uncovered_reduce_only(&owner, MARKET_INDEX, &bid),
            Err(ErrorCode::InvalidUserAccount.into())
        );
    }
}
