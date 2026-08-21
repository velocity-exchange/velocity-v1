//! Reading a CLOB book from off-chain, through the program's own walk.
//!
//! A fill can only settle for users whose accounts the transaction carries,
//! and the book stores its makers as `(authority, sub_account_id)` rather than
//! account keys, so whoever assembles a fill has to read the book to know
//! whose accounts to bring. Doing that with a hand-written walk is how the
//! answer drifts: the program passes over unactivated, expired, self-traded
//! and crossed taker-origin orders, and a copy that forgets one of those
//! names the wrong makers.
//!
//! So this calls the program's `clob_resting_prefix` rather than reimplement
//! it. The crate already depends on the velocity program as a host library,
//! and the walk is a pure function of the account's bytes.

use program::{
    controller::position::PositionDirection,
    state::prop_amm::{clob_resting_prefix, ClobSide, ClobUserRefV0, QuoterUserCapsV0},
};

/// The run of resting orders a taker of `size` would sweep, best price first,
/// straight from the program's walk.
///
/// The skip rules are the program's: unactivated, expired, self-traded. What
/// a caller does with the run — count it, name its owners, size a cross
/// against it — is its own business, but the run itself has one definition.
pub fn resting_orders(
    book_data: &[u8],
    side: ClobSide,
    size: u64,
    taker: ClobUserRefV0,
    slot: u64,
    now: i64,
) -> Vec<program::state::prop_amm::ClobRestingOrderV0> {
    clob_resting_prefix(
        book_data,
        side,
        size,
        &[],
        &QuoterUserCapsV0::EMPTY,
        &taker,
        slot,
        now,
    )
}

/// Distinct makers a taker of `size` would sweep, best price first.
///
/// `taker` is skipped, since a fill never settles a user against itself.
/// `limit` bounds the answer: every maker costs the transaction two accounts
/// and a transaction locks 64, so a caller that cannot bring them all is
/// better served by the best-priced prefix than by a list it has to truncate
/// itself — the book stops at the first maker the caller did not bring, so a
/// prefix is the only shape that fills anything.
pub fn resting_makers(
    book_data: &[u8],
    side: ClobSide,
    size: u64,
    taker: ClobUserRefV0,
    slot: u64,
    now: i64,
    limit: usize,
) -> Vec<ClobUserRefV0> {
    // Unrestricted and uncapped: this is the question "who is here", and the
    // caller is deciding what to carry, not what it may settle.
    let mut makers: Vec<ClobUserRefV0> = Vec::new();
    for order in resting_orders(book_data, side, size, taker, slot, now) {
        if makers.contains(&order.user) {
            continue;
        }
        if makers.len() == limit {
            break;
        }
        makers.push(order.user);
    }
    makers
}

/// The book side a taker of this direction sweeps: a buyer takes asks.
pub fn swept_side(direction: PositionDirection) -> ClobSide {
    match direction {
        PositionDirection::Long => ClobSide::Ask,
        PositionDirection::Short => ClobSide::Bid,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use program::state::prop_amm::CLOB_NIL;

    /// Reading the same bytes the program reads, through the same walk. The
    /// point of the test is the wiring: an off-chain list that disagrees with
    /// what the fill will do names the wrong accounts, and the fill discovers
    /// that on-chain.
    #[test]
    fn a_books_makers_come_back_best_price_first_and_deduplicated() {
        let one = ClobUserRefV0 {
            authority: anchor_lang::prelude::Pubkey::new_unique(),
            sub_account_id: 0,
        };
        let two = ClobUserRefV0 {
            authority: anchor_lang::prelude::Pubkey::new_unique(),
            sub_account_id: 3,
        };
        let taker = ClobUserRefV0 {
            authority: anchor_lang::prelude::Pubkey::new_unique(),
            sub_account_id: 0,
        };
        // Two orders from the same maker, then a second maker, then the
        // taker's own — which a fill never settles against itself.
        let data = book_bytes(&[
            (one, 100, 5, 1),
            (one, 101, 5, 2),
            (two, 102, 5, 3),
            (taker, 103, 5, CLOB_NIL),
        ]);

        let makers = resting_makers(&data, ClobSide::Ask, 100, taker, 0, 0, 8);
        assert_eq!(makers, vec![one, two], "distinct owners, best price first");

        // The budget cuts the tail, never the head: the book stops at the
        // first maker the caller did not bring, so only a prefix fills.
        assert_eq!(
            resting_makers(&data, ClobSide::Ask, 100, taker, 0, 0, 1),
            vec![one]
        );

        // And a sweep that ends inside the first maker never reaches the second.
        assert_eq!(
            resting_makers(&data, ClobSide::Ask, 5, taker, 0, 0, 8),
            vec![one]
        );
    }

    /// Lays out an ask side the program's node reader can walk. Offsets come
    /// from the program's own constants so a header change breaks this too.
    fn book_bytes(orders: &[(ClobUserRefV0, u64, u64, u32)]) -> Vec<u8> {
        use program::state::prop_amm::{
            CLOB_BEST_ASK_OFFSET, CLOB_NODE_LEN, CLOB_ORDERS_OFFSET, CLOB_ORDER_BIT_FLAG_OPEN,
        };
        let mut data = vec![0u8; CLOB_ORDERS_OFFSET + orders.len().max(1) * CLOB_NODE_LEN + 64];
        data[CLOB_BEST_ASK_OFFSET..CLOB_BEST_ASK_OFFSET + 4].copy_from_slice(&0u32.to_le_bytes());
        for (index, (user, price, size, next)) in orders.iter().enumerate() {
            let at = CLOB_ORDERS_OFFSET + index * CLOB_NODE_LEN;
            data[at..at + 32].copy_from_slice(&user.authority.to_bytes());
            data[at + 32..at + 40].copy_from_slice(&price.to_le_bytes());
            data[at + 40..at + 48].copy_from_slice(&size.to_le_bytes());
            data[at + 84..at + 88].copy_from_slice(&next.to_le_bytes());
            data[at + 88] = CLOB_ORDER_BIT_FLAG_OPEN;
            data[at + 90..at + 92].copy_from_slice(&user.sub_account_id.to_le_bytes());
        }
        data
    }
}
