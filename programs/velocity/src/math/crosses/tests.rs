use {super::*, anchor_lang::prelude::Pubkey};

fn order(id: u64, price: u64, size: u64, owner: u8, taker_origin: bool) -> RestingOrder {
    RestingOrder {
        order_ref: ClobOrderRefV0 {
            node_index: id as u32,
            order_id: id,
        },

        user: UserRefV0 {
            authority: Pubkey::new_from_array([owner; 32]),
            sub_account_id: 0,
        },

        price,
        base_asset_amount: size,
        taker_origin,
        reduce_only: false,
        // Rest order is the id; the slot only prices the work of resolving it.
        placed_slot: id,
    }
}

fn remainder(id: u64, price: u64, size: u64, owner: u8) -> RestingOrder {
    order(id, price, size, owner, true)
}

fn maker(id: u64, price: u64, size: u64, owner: u8) -> RestingOrder {
    order(id, price, size, owner, false)
}

/// The later of two crossed remainders aggresses, and settles at the earlier
/// one's price — the improvement belongs to the order that arrived into a book
/// already showing the other.
#[test]
fn the_later_remainder_aggresses_at_the_earlier_ones_price() {
    let crosses = resolve_crosses(
        &[remainder(10, 101, 5, 0xA)],
        &[remainder(20, 99, 5, 0xB)],
        8,
    );

    assert_eq!(crosses.len(), 1);
    assert_eq!(crosses[0].kind, CrossKind::AskAggresses);
    assert_eq!(crosses[0].settlement_price(), Some(101));
    assert_eq!(crosses[0].base_asset_amount, 5);
}

/// Reverse the rest order and the improvement changes hands, which is the whole
/// content of the rule.
#[test]
fn rest_order_decides_who_captures() {
    let crosses = resolve_crosses(
        &[remainder(20, 101, 5, 0xA)],
        &[remainder(10, 99, 5, 0xB)],
        8,
    );

    assert_eq!(crosses[0].kind, CrossKind::BidAggresses);
    assert_eq!(crosses[0].settlement_price(), Some(99));
}

/// A maker quote is passive whenever it rested, so rest time does not arbitrate
/// against it — the remainder always aggresses and pays the maker's price.
#[test]
fn a_remainder_always_aggresses_against_a_maker() {
    let older = resolve_crosses(&[remainder(40, 101, 5, 0xA)], &[maker(10, 99, 5, 0xB)], 8);
    assert_eq!(older[0].kind, CrossKind::BidAggresses);
    assert_eq!(older[0].settlement_price(), Some(99));

    // Same, with the maker resting after the remainder.
    let newer = resolve_crosses(&[remainder(10, 101, 5, 0xA)], &[maker(40, 99, 5, 0xB)], 8);
    assert_eq!(newer[0].kind, CrossKind::BidAggresses);
    assert_eq!(newer[0].settlement_price(), Some(99));
}

/// Two makers crossing is unclaimed arbitrage: nobody demanded anything, so the
/// protocol middles it and each leg keeps its own price.
#[test]
fn two_makers_are_middled() {
    let crosses = resolve_crosses(&[maker(10, 101, 5, 0xA)], &[maker(20, 99, 5, 0xB)], 8);

    assert_eq!(crosses[0].kind, CrossKind::ProtocolMiddles);
    assert_eq!(crosses[0].settlement_price(), None);
    assert_eq!(crosses[0].bid.price, 101);
    assert_eq!(crosses[0].ask.price, 99);
}

/// The reason the arb path used to refuse to run while a remainder was pending:
/// middling first would hand a taker's own improvement to the protocol. One
/// pass settles them in the right order instead of refusing.
#[test]
fn a_taker_cross_outranks_a_maker_cross() {
    let bids = [remainder(30, 101, 5, 0xA), maker(11, 100, 5, 0xC)];
    let asks = [remainder(10, 99, 5, 0xB), maker(12, 98, 5, 0xD)];
    let crosses = resolve_crosses(&bids, &asks, 8);

    assert_eq!(
        crosses[0].kind,
        CrossKind::BidAggresses,
        "the remainder's improvement is settled before any arbitrage"
    );

    assert!(crosses
        .iter()
        .any(|cross| cross.kind == CrossKind::ProtocolMiddles));
}

/// The case a pairwise resolver gets wrong. Four remainders cross at once;
/// taking only the two heads leaves the other two still crossing, and a book
/// that gates every one of them can never free them by itself.
#[test]
fn several_crossed_remainders_resolve_in_one_pass() {
    let bids = [remainder(10, 101, 5, 0xA), remainder(30, 100, 5, 0xC)];
    let asks = [remainder(20, 99, 5, 0xB), remainder(40, 98, 5, 0xD)];
    let crosses = resolve_crosses(&bids, &asks, 8);

    assert_eq!(crosses.len(), 2);
    // The latest to rest goes first, and takes the best price it can reach.
    assert_eq!(crosses[0].ask.order_ref.order_id, 40);
    assert_eq!(crosses[0].settlement_price(), Some(101));
    // Then the next latest, against what is left.
    assert_eq!(crosses[1].bid.order_ref.order_id, 30);
    assert_eq!(crosses[1].settlement_price(), Some(99));
}

/// An aggressor bigger than its best counterparty takes the next one too, and
/// never more than it has.
#[test]
fn an_aggressor_sweeps_more_than_one_counterparty() {
    let bids = [remainder(10, 101, 3, 0xA), remainder(11, 100, 3, 0xC)];
    let asks = [remainder(40, 98, 10, 0xD)];
    let crosses = resolve_crosses(&bids, &asks, 8);

    assert_eq!(crosses.len(), 2);
    assert_eq!(crosses[0].settlement_price(), Some(101));
    assert_eq!(crosses[1].settlement_price(), Some(100));
    assert_eq!(
        crosses.iter().map(|c| c.base_asset_amount).sum::<u64>(),
        6,
        "the aggressor never fills past what the counterparties held"
    );
}

/// Uncrossed orders are just resting orders.
#[test]
fn nothing_crossing_matches_nothing() {
    assert!(resolve_crosses(
        &[remainder(10, 99, 5, 0xA)],
        &[remainder(20, 101, 5, 0xB)],
        8
    )
    .is_empty());
    assert!(resolve_crosses(&[maker(10, 99, 5, 0xA)], &[maker(20, 101, 5, 0xB)], 8).is_empty());
}

/// The cranker is paid out of what a cross produces, so one authority resting
/// both sides could otherwise manufacture one and collect for it. Sub-accounts
/// do not launder it: the check is on the authority.
#[test]
fn an_authority_cannot_cross_itself() {
    let mut ask = remainder(20, 99, 5, 0xA);
    ask.user.sub_account_id = 7;
    assert!(resolve_crosses(&[remainder(10, 101, 5, 0xA)], &[ask], 8).is_empty());

    let mut maker_ask = maker(20, 99, 5, 0xA);
    maker_ask.user.sub_account_id = 7;
    assert!(resolve_crosses(&[maker(10, 101, 5, 0xA)], &[maker_ask], 8).is_empty());
}

/// The pass is bounded: a caller settles what one transaction can carry and the
/// next crank picks up the rest.
#[test]
fn the_cross_count_is_bounded() {
    let bids = [remainder(10, 101, 5, 0xA), remainder(11, 100, 5, 0xC)];
    let asks = [remainder(40, 98, 5, 0xD), remainder(41, 98, 5, 0xE)];
    assert_eq!(resolve_crosses(&bids, &asks, 1).len(), 1);
}

/// The crossing-prefix walk that the protocol's two-legged cross consumes.
mod prefix {
    use super::*;

    const UNIT: u64 = crate::math::constants::BASE_PRECISION_U64;

    fn level(price: u64, size: u64, owner: u8) -> CrossLevel {
        CrossLevel {
            price,
            size,
            owner: UserRefV0 {
                authority: Pubkey::new_from_array([owner; 32]),
                sub_account_id: 0,
            },
        }
    }

    /// The walk stops before an owner past the cap, so every unit it sizes has
    /// its owner staged.
    #[test]
    fn the_walk_stops_at_the_owner_cap() {
        let bids = [level(105, UNIT, 1), level(104, UNIT, 2)];
        let asks = [level(100, UNIT, 3), level(101, UNIT, 4)];
        let prefix = crossing_prefix(&bids, &asks, 3);
        assert_eq!(prefix.size, UNIT);
        assert_eq!(prefix.makers.len(), 3);
        assert_eq!(crossing_prefix(&bids, &asks, 4).size, 2 * UNIT);
    }

    /// A quoter's ladder has one owner. Its own resting order on the other
    /// side is a self-cross, and the walk ends there.
    #[test]
    fn a_quoter_does_not_cross_its_own_resting_order() {
        let quoter_asks = [level(100, 2 * UNIT, 9)];
        let book_bids = [level(103, UNIT, 1), level(102, UNIT, 9)];
        let prefix = crossing_prefix(&book_bids, &quoter_asks, 8);
        assert_eq!(prefix.size, UNIT);
        assert_eq!(prefix.buy_quote, 100);
        assert_eq!(prefix.sell_quote, 103);
    }

    /// Both legs pay the fee, rounded up, before the protocol keeps anything.
    #[test]
    fn the_estimate_is_net_of_both_legs_fees() {
        let prefix = CrossPrefix {
            size: UNIT,
            buy_quote: 1_000,
            sell_quote: 1_010,
            makers: Vec::new(),
        };
        let fee = |fee_numerator| crate::state::state::FeeTier {
            fee_numerator,
            fee_denominator: 1_000,
            ..crate::state::state::FeeTier::default()
        };
        assert_eq!(prefix.estimated_surplus(&fee(0)), 10);
        assert_eq!(prefix.estimated_surplus(&fee(4)), 10 - 4 - 5);
        assert_eq!(prefix.estimated_surplus(&fee(5)), 0);
    }
}
