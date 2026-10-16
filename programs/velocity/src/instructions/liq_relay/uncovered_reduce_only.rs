//! The last stage of the liquidation resolver. It stages
//! `crank_clob_cancel_uncovered_reduce_only` for a reduce-only book order that
//! no part of its owner's position covers. A flat account that still rests
//! book orders keeps its liveness poll armed, so the poll reaches such an order
//! after the owner closes the position elsewhere. The resolver reads the top
//! rows of each uncovered side and stages one cancel per wake. An order deeper
//! than the read waits for a later wake, when the orders in front of it are gone.

use {
    super::resolve_liquidate_perp_with_fill::{heap_copy, StoredBook},
    crate::{
        error::ErrorCode,
        instructions::{
            clob::helpers::crank_common::{
                is_bound_remainder, row_order_ref, sol_spot_market_ref, staged_row_cancel,
            },
            router::user_caps::position_cover,
            CrankClobCancelUncoveredReduceOnlyArgs, StagedCall,
        },
        state::{
            pdas,
            prop_amm::{DirectionV0, L3RowV0, SideV0, L3_ROW_FLAG_REDUCE_ONLY},
            state::{ExchangeStatus, State},
            user::User,
        },
    },
    anchor_lang::prelude::*,
};

/// Rows read off one side. The count matches the window of the bid/ask TWAP
/// crank, which is the reader these orders crowd.
const REDUCE_ONLY_SCAN_ROWS: u16 = 32;

/// One book side where the user rests reduce-only orders that nothing covers.
struct UncoveredSide {
    market_index: u16,
    side: SideV0,
}

/// Stage the cancel of one uncovered reduce-only order of the user, or `None`
/// when the read finds none. A fill pause refuses the crank, so it stages
/// nothing then.
pub(super) fn stage_uncovered_reduce_only_cancel<'info>(
    state: &State,
    state_key: Pubkey,
    user_loader: &AccountLoader<'info, User>,
    remaining_accounts: &'info [AccountInfo<'info>],
) -> Result<Option<StagedCall>> {
    if state
        .get_exchange_status()?
        .contains(ExchangeStatus::FillPaused)
    {
        return Ok(None);
    }

    let user = heap_copy(&*crate::load!(user_loader)?);
    for UncoveredSide { market_index, side } in uncovered_sides(&user) {
        let Some(book) = StoredBook::find(remaining_accounts, market_index)? else {
            continue;
        };

        let rows = book
            .read_side(
                market_index,
                sweeping_direction(side),
                REDUCE_ONLY_SCAN_ROWS,
                |row| *row,
            )?
            .unwrap_or_default();
        if let Some(row) = uncovered_reduce_only_row(&user, &rows) {
            return stage_cancel(
                state,
                state_key,
                user_loader.key(),
                &book,
                market_index,
                &row,
            )
            .map(Some);
        }
    }

    Ok(None)
}

/// The sides where the user rests reduce-only book orders and its position
/// covers none of them.
fn uncovered_sides(user: &User) -> Vec<UncoveredSide> {
    user.perp_positions
        .iter()
        .filter(|position| position.has_reduce_only_clob())
        .flat_map(|position| {
            [SideV0::Bid, SideV0::Ask].map(|side| UncoveredSide {
                market_index: position.market_index,
                side,
            })
        })
        .filter(|candidate| position_cover(user, candidate.market_index, candidate.side) == 0)
        .collect()
}

/// The first reduce-only row of `user` in `rows` that a keeper may cancel. The
/// caller passes only a side that [`uncovered_sides`] returned.
fn uncovered_reduce_only_row(user: &User, rows: &[L3RowV0]) -> Option<L3RowV0> {
    let owner = user.clob_user_ref();
    rows.iter()
        .find(|row| {
            row.user == owner
                && row.flags & L3_ROW_FLAG_REDUCE_ONLY != 0
                && !is_bound_remainder(row)
        })
        .copied()
}

/// The taker direction whose read returns the orders on `side`. A seller
/// sweeps the bids.
fn sweeping_direction(side: SideV0) -> DirectionV0 {
    match side {
        SideV0::Bid => DirectionV0::Short,
        SideV0::Ask => DirectionV0::Long,
    }
}

/// The crank in program-keeper mode.
fn stage_cancel(
    state: &State,
    state_key: Pubkey,
    user_key: Pubkey,
    book: &StoredBook,
    market_index: u16,
    row: &L3RowV0,
) -> Result<StagedCall> {
    let (protocol_user, protocol_user_stats) = pdas::protocol_user_pair();
    staged_row_cancel::<crate::instruction::CrankClobCancelUncoveredReduceOnly>(
        crate::accounts::CrankClobCancelUncoveredReduceOnly {
            state: state_key,
            authority: pdas::keeper_placeholder(),
            filler: protocol_user,
            filler_stats: protocol_user_stats,
            user: user_key,
            perp_market: pdas::perp_market(market_index),
            quoter_slab: book.slab_key(),
            clob_market: book.book_key(),
            clob_program: crate::ids::clob_program::id(),
            crank_conditions: Some(pdas::clob_crank_conditions(market_index)),
            sol_spot_market: sol_spot_market_ref(state),
        },
        row,
    )
    .arg(CrankClobCancelUncoveredReduceOnlyArgs {
        market_index,
        order_ref: row_order_ref(row),
    })
}

#[cfg(test)]
mod tests {
    use {
        super::{uncovered_reduce_only_row, uncovered_sides},
        crate::state::{
            prop_amm::{L3RowV0, SideV0, L3_ROW_FLAG_REDUCE_ONLY, L3_ROW_FLAG_TAKER_ORIGIN},
            user::{PerpPosition, User},
        },
        anchor_lang::prelude::Pubkey,
        quoter_spec::L3_ROW_FLAG_CLAIM_LAPSED,
    };

    fn user_with_position(base_asset_amount: i64) -> User {
        let mut user = User {
            authority: Pubkey::new_unique(),
            ..User::default()
        };
        user.perp_positions[0] = PerpPosition {
            market_index: 0,
            base_asset_amount,
            open_orders: 1,
            reduce_only_clob_orders: 1,
            ..PerpPosition::default()
        };
        user
    }

    fn row(owner: &User, order_id: u64, flags: u8) -> L3RowV0 {
        L3RowV0 {
            order_id,
            user: owner.clob_user_ref(),
            flags,
            ..L3RowV0::default()
        }
    }

    #[test]
    fn the_first_uncovered_reduce_only_row_of_the_user_is_found() {
        let user = user_with_position(0);
        let other = user_with_position(0);
        let rows = [
            row(&other, 1, L3_ROW_FLAG_REDUCE_ONLY),
            row(&user, 2, 0),
            row(&user, 3, L3_ROW_FLAG_REDUCE_ONLY),
            row(&user, 4, L3_ROW_FLAG_REDUCE_ONLY),
        ];

        let found = uncovered_reduce_only_row(&user, &rows);
        assert_eq!(found.map(|row| row.order_id), Some(3));
    }

    #[test]
    fn a_bound_taker_remainder_is_skipped() {
        let user = user_with_position(0);
        let bound = L3_ROW_FLAG_REDUCE_ONLY | L3_ROW_FLAG_TAKER_ORIGIN;
        let rows = [
            row(&user, 1, bound),
            row(&user, 2, bound | L3_ROW_FLAG_CLAIM_LAPSED),
        ];

        let found = uncovered_reduce_only_row(&user, &rows);
        assert_eq!(found.map(|row| row.order_id), Some(2));
    }

    #[test]
    fn rows_of_other_users_and_plain_rows_are_skipped() {
        let user = user_with_position(0);
        let other = user_with_position(0);
        let rows = [row(&other, 1, L3_ROW_FLAG_REDUCE_ONLY), row(&user, 2, 0)];
        assert_eq!(uncovered_reduce_only_row(&user, &rows), None);
    }

    /// A long covers the asks, so only the bid side is read.
    #[test]
    fn only_the_sides_a_position_does_not_cover_are_read() {
        let sides = |user: &User| {
            uncovered_sides(user)
                .into_iter()
                .map(|side| side.side)
                .collect::<Vec<_>>()
        };

        assert_eq!(sides(&user_with_position(1)), vec![SideV0::Bid]);
        assert_eq!(
            sides(&user_with_position(0)),
            vec![SideV0::Bid, SideV0::Ask]
        );

        let mut plain = user_with_position(0);
        plain.perp_positions[0].reduce_only_clob_orders = 0;
        assert!(sides(&plain).is_empty());
    }
}
