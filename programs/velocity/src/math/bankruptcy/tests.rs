use crate::{
    create_anchor_account_info,
    math::{
        bankruptcy::{
            has_realizable_isolated_assets, has_realizable_spot_assets_for_setoff,
            is_cross_margin_bankrupt, is_isolated_margin_bankrupt,
        },
        constants::{
            QUOTE_PRECISION_I128, QUOTE_PRECISION_I64, SPOT_BALANCE_PRECISION,
            SPOT_CUMULATIVE_INTEREST_PRECISION,
        },
    },
    state::{
        perp_market::PerpMarket,
        perp_market_map::PerpMarketMap,
        spot_market::{SpotBalanceType, SpotMarket},
        spot_market_map::SpotMarketMap,
        user::{PerpPosition, PositionFlag, SpotPosition, User},
    },
    test_utils::{get_positions, get_spot_positions},
};

/// Scaffolding for the map-taking predicate (OtterSec #151).
///
/// `deposit_interest` sets spot market 0's cumulative deposit index, which is what decides
/// whether a deposit row is worth a token and so whether it vetoes. `pnl_pool_dollars` funds
/// perp market 0's PnL pool, which admission is deliberately blind to.
macro_rules! with_maps {
    ($pnl_pool_dollars:expr, $deposit_interest:expr, |$perp_map:ident, $spot_map:ident| $body:block) => {{
        let mut spot_market = SpotMarket {
            market_index: 0,
            decimals: 6,
            cumulative_deposit_interest: $deposit_interest,
            cumulative_borrow_interest: SPOT_CUMULATIVE_INTEREST_PRECISION,
            deposit_balance: 1_000_000 * SPOT_BALANCE_PRECISION,
            ..SpotMarket::default()
        };
        create_anchor_account_info!(spot_market, SpotMarket, spot_market_ai);
        let $spot_map = SpotMarketMap::load_one(&spot_market_ai, true).unwrap();

        let mut perp_market = PerpMarket {
            market_index: 0,
            quote_spot_market_index: 0,
            ..PerpMarket::default()
        };
        // $1 of pool = SPOT_BALANCE_PRECISION of scaled balance at a 1.0 index.
        perp_market.pnl_pool.scaled_balance =
            ($pnl_pool_dollars as u128) * (SPOT_BALANCE_PRECISION as u128);
        create_anchor_account_info!(perp_market, PerpMarket, perp_market_ai);
        let $perp_map = PerpMarketMap::load_one(&perp_market_ai, true).unwrap();

        $body
    }};
}

/// A funded pool and a 1.0 deposit index — the "nothing unusual" baseline, so the
/// pre-existing expectations below keep their original meaning.
macro_rules! healthy_maps {
    (|$perp_map:ident, $spot_map:ident| $body:block) => {
        with_maps!(
            1_000_000,
            SPOT_CUMULATIVE_INTEREST_PRECISION,
            |$perp_map, $spot_map| $body
        )
    };
}

#[test]
fn user_has_position_with_base() {
    let user = User {
        perp_positions: get_positions(PerpPosition {
            base_asset_amount: 1,
            ..PerpPosition::default()
        }),
        ..User::default()
    };

    healthy_maps!(|perp_map, spot_map| {
        assert!(!is_cross_margin_bankrupt(&user, &spot_map).unwrap());
    });
}

#[test]
fn user_has_position_with_positive_quote() {
    let user = User {
        perp_positions: get_positions(PerpPosition {
            quote_asset_amount: 1,
            ..PerpPosition::default()
        }),
        ..User::default()
    };

    // A lone positive claim leaves the estate net solvent, so it still vetoes.
    healthy_maps!(|perp_map, spot_map| {
        assert!(!is_cross_margin_bankrupt(&user, &spot_map).unwrap());
    });
}

#[test]
fn user_with_deposit() {
    let user = User {
        spot_positions: get_spot_positions(SpotPosition {
            balance_type: SpotBalanceType::Deposit,
            scaled_balance: SPOT_BALANCE_PRECISION as u64,
            ..SpotPosition::default()
        }),
        ..User::default()
    };

    healthy_maps!(|perp_map, spot_map| {
        assert!(!is_cross_margin_bankrupt(&user, &spot_map).unwrap());
    });
}

#[test]
fn user_has_position_with_negative_quote() {
    let user = User {
        perp_positions: get_positions(PerpPosition {
            quote_asset_amount: -1,
            ..PerpPosition::default()
        }),
        ..User::default()
    };

    healthy_maps!(|perp_map, spot_map| {
        assert!(is_cross_margin_bankrupt(&user, &spot_map).unwrap());
    });
}

#[test]
fn user_with_borrow() {
    let user = User {
        spot_positions: get_spot_positions(SpotPosition {
            balance_type: SpotBalanceType::Borrow,
            scaled_balance: 1,
            ..SpotPosition::default()
        }),
        ..User::default()
    };

    healthy_maps!(|perp_map, spot_map| {
        assert!(is_cross_margin_bankrupt(&user, &spot_map).unwrap());
    });
}

#[test]
fn user_with_empty_position_and_balances() {
    let user = User::default();
    healthy_maps!(|perp_map, spot_map| {
        assert!(!is_cross_margin_bankrupt(&user, &spot_map).unwrap());
    });
}

/// OtterSec #151 — a zero-token deposit residue must not veto bankruptcy.
///
/// A full spot-market socialization floors `cumulative_deposit_interest` at 1 and
/// leaves each wiped depositor's scaled row positive. The row survives while its token
/// value is zero, and that worthless row blocked bankruptcy admission for a user with
/// unrelated cross-margin debt — stalling the next bad-debt repair.
#[test]
fn zero_token_deposit_residue_does_not_veto_bankruptcy() {
    let mut user = User {
        perp_positions: get_positions(PerpPosition {
            market_index: 0,
            quote_asset_amount: -100,
            ..PerpPosition::default()
        }),
        ..User::default()
    };
    // Socialized-away deposit: scaled balance still positive, worth nothing.
    user.spot_positions[0] = SpotPosition {
        market_index: 0,
        balance_type: SpotBalanceType::Deposit,
        scaled_balance: 1_000,
        ..SpotPosition::default()
    };

    // cumulative_deposit_interest floored at 1 => the row converts to 0 tokens.
    with_maps!(1_000_000, 1, |perp_map, spot_map| {
        assert!(
            is_cross_margin_bankrupt(&user, &spot_map).unwrap(),
            "a zero-token deposit residue must not veto bankruptcy"
        );
    });

    // A deposit actually worth >= 1 token still vetoes.
    let mut solvent = user;
    solvent.spot_positions[0].scaled_balance = SPOT_BALANCE_PRECISION as u64;
    healthy_maps!(|perp_map, spot_map| {
        assert!(
            !is_cross_margin_bankrupt(&solvent, &spot_map).unwrap(),
            "a deposit worth >= 1 token must still veto"
        );
    });
}

#[test]
fn user_with_isolated_position() {
    let user = User {
        perp_positions: get_positions(PerpPosition {
            position_flag: PositionFlag::IsolatedPosition as u8,
            ..PerpPosition::default()
        }),
        ..User::default()
    };

    healthy_maps!(|perp_map, spot_map| {
        let mut user_with_scaled_balance = user;
        user_with_scaled_balance.perp_positions[0].isolated_position_scaled_balance =
            1000000000000000000;
        assert!(!is_cross_margin_bankrupt(&user_with_scaled_balance, &spot_map).unwrap());

        let mut user_with_base_asset_amount = user;
        user_with_base_asset_amount.perp_positions[0].base_asset_amount = 1000000000000000000;
        assert!(!is_cross_margin_bankrupt(&user_with_base_asset_amount, &spot_map).unwrap());

        let mut user_with_open_order = user;
        user_with_open_order.perp_positions[0].open_orders = 1;
        assert!(!is_cross_margin_bankrupt(&user_with_open_order, &spot_map).unwrap());

        let mut user_with_positive_pnl = user;
        user_with_positive_pnl.perp_positions[0].quote_asset_amount = 1000000000000000000;
        assert!(!is_cross_margin_bankrupt(&user_with_positive_pnl, &spot_map).unwrap());

        let mut user_with_negative_pnl = user;
        user_with_negative_pnl.perp_positions[0].quote_asset_amount = -1000000000000000000;
        assert!(!is_cross_margin_bankrupt(&user_with_negative_pnl, &spot_map).unwrap());

        assert!(is_isolated_margin_bankrupt(&user_with_negative_pnl, 0).unwrap());
    });
}

/// Scaffolding for the cross-market #145 cases: perp market 0 holds the claim, perp market 1 holds
/// the debt with an empty pool.
///
/// The claim market owes `$claim_aggregate_dollars` to its users in total and holds
/// `$claim_pool_dollars` of pool against that.
macro_rules! with_two_perp_markets {
    ($claim_aggregate_dollars:expr, $claim_pool_dollars:expr, |$perp_map:ident, $spot_map:ident| $body:block) => {{
        let mut spot_market = SpotMarket {
            market_index: 0,
            decimals: 6,
            cumulative_deposit_interest: SPOT_CUMULATIVE_INTEREST_PRECISION,
            cumulative_borrow_interest: SPOT_CUMULATIVE_INTEREST_PRECISION,
            deposit_balance: 1_000_000 * SPOT_BALANCE_PRECISION,
            ..SpotMarket::default()
        };
        create_anchor_account_info!(spot_market, SpotMarket, spot_market_ai);
        let $spot_map = SpotMarketMap::load_one(&spot_market_ai, true).unwrap();

        let mut claim_market = PerpMarket {
            market_index: 0,
            quote_spot_market_index: 0,
            quote_asset_amount: ($claim_aggregate_dollars as i128) * QUOTE_PRECISION_I128,
            ..PerpMarket::default()
        };
        claim_market.pnl_pool.scaled_balance =
            ($claim_pool_dollars as u128) * (SPOT_BALANCE_PRECISION as u128);
        create_anchor_account_info!(claim_market, PerpMarket, claim_market_ai);

        let mut debt_market = PerpMarket {
            market_index: 1,
            quote_spot_market_index: 0,
            ..PerpMarket::default()
        };
        create_anchor_account_info!(debt_market, PerpMarket, debt_market_ai);

        let $perp_map =
            PerpMarketMap::load_multiple(vec![&claim_market_ai, &debt_market_ai], true).unwrap();

        $body
    }};
}

/// Builds an estate with `claim` unsettled in perp market 0 and `debt` in perp market 1.
fn cross_market_estate(claim_dollars: i64, debt_dollars: i64) -> User {
    let claim = claim_dollars * QUOTE_PRECISION_I64;
    let debt = debt_dollars * QUOTE_PRECISION_I64;
    let mut user = User::default();
    user.perp_positions[0] = PerpPosition {
        market_index: 0,
        quote_asset_amount: claim,
        ..PerpPosition::default()
    };
    user.perp_positions[1] = PerpPosition {
        market_index: 1,
        quote_asset_amount: debt,
        ..PerpPosition::default()
    };
    user
}

/// OtterSec #145: an unfundable positive claim must not veto a resolvable loss elsewhere.
///
/// The old rule vetoed on any positive `quote_asset_amount`. A claim on a market whose PnL pool could
/// not pay it therefore stranded the loss in the other market forever. The pool fills only as
/// counterparty losses settle, which can never happen, and until then the claim cannot become a
/// deposit to clear the veto.
#[test]
fn unfundable_positive_claim_does_not_veto_bankruptcy() {
    let user = cross_market_estate(500, -1_000);

    // Claim market's pool is empty: the 500 cannot be realized at all.
    with_two_perp_markets!(500, 0, |perp_map, spot_map| {
        assert!(
            is_cross_margin_bankrupt(&user, &spot_map).unwrap(),
            "an unfundable claim must not strand a resolvable loss in another market"
        );
    });
}

/// OtterSec #145, second half: the state of the pool must not veto admission at all.
///
/// The original rule vetoed while the pool held anything, which left the same stall in place. Any
/// market participant could re-arm it for the price of a trade, because trading fees flow into the
/// pnl pool, and a keeper had to drain the pool through the ordinary pipeline before the repair
/// could proceed.
///
/// Admission is now silent about the pool. The resolvers recover what it can pay and forfeit the
/// rest, so no pool state can hold a bad-debt repair open.
#[test]
fn pool_state_never_vetoes_bankruptcy() {
    let user = cross_market_estate(500, -1_000);

    // Empty, part-funded, and funded past the claim: the answer is the same.
    for pool in [0, 200, 1_000] {
        with_two_perp_markets!(500, pool, |perp_map, spot_map| {
            assert!(
                is_cross_margin_bankrupt(&user, &spot_map).unwrap(),
                "pool state must not gate admission (pool = {})",
                pool
            );
        });
    }
}

/// OtterSec #145: a net-solvent estate is still refused, however unfundable its claim is.
///
/// Without this gate, a large unfundable claim against a small debt is admitted, and the whole claim
/// is forfeited to cover a fraction of it. That confiscates value the user is owed. It is also the
/// shape that made a payability-only check unsound against
/// `successful_liquidation_over_multiple_slots`, where a user holds $1050 of real positive PnL against
/// a pool that is empty only because counterparty losses have not settled.
#[test]
fn net_solvent_estate_is_never_bankrupt_however_unfundable() {
    let user = cross_market_estate(5_000, -1_000);

    with_two_perp_markets!(5_000, 0, |perp_map, spot_map| {
        assert!(
            !is_cross_margin_bankrupt(&user, &spot_map).unwrap(),
            "a net-solvent estate must never be admitted, or the extinguish step over-confiscates"
        );
    });
}

/// The stale-latch re-check stays scoped to spot deposits (OtterSec #130).
///
/// A perp claim is the other place value can sit on a cross-margin estate, but it never needs to be
/// handed back to ordinary liquidation: the resolvers recover the payable part into the quote deposit
/// themselves and forfeit the rest under a `ForfeitBudget`. Widening this check to perp claims would
/// un-latch an estate whose claim no pool can pay, and nothing in ordinary liquidation could move
/// that claim onto the debt, so the bad debt would never resolve.
#[test]
fn the_stale_latch_check_is_scoped_to_spot_deposits() {
    let user = cross_market_estate(500, -1_000);

    with_two_perp_markets!(500, 1_000, |perp_map, spot_map| {
        assert!(
            !has_realizable_spot_assets_for_setoff(&user, &spot_map).unwrap(),
            "a perp claim belongs to the resolver's recovery pass, not to the un-latch"
        );
    });

    let mut with_deposit = user;
    with_deposit.spot_positions[0] = SpotPosition {
        market_index: 0,
        balance_type: SpotBalanceType::Deposit,
        scaled_balance: SPOT_BALANCE_PRECISION as u64,
        ..SpotPosition::default()
    };

    with_two_perp_markets!(500, 0, |perp_map, spot_map| {
        assert!(
            has_realizable_spot_assets_for_setoff(&with_deposit, &spot_map).unwrap(),
            "a deposit ordinary liquidation can seize must un-latch"
        );
    });
}

/// An isolated position is walled off from the cross-margin book, so only its own collateral counts.
///
/// The cross check must never be asked about an isolated bankruptcy. A cross deposit can never pay an
/// isolated debt, so un-latching for one would clear the latch that `is_isolated_margin_bankrupt`
/// immediately sets again, and the resolver would make no progress on any call.
#[test]
fn isolated_assets_are_the_isolated_position_own_collateral() {
    let mut user = User {
        spot_positions: get_spot_positions(SpotPosition {
            market_index: 0,
            balance_type: SpotBalanceType::Deposit,
            scaled_balance: 1_000 * SPOT_BALANCE_PRECISION as u64,
            ..SpotPosition::default()
        }),
        perp_positions: get_positions(PerpPosition {
            market_index: 0,
            quote_asset_amount: -1_000 * QUOTE_PRECISION_I64,
            position_flag: PositionFlag::IsolatedPosition as u8,
            ..PerpPosition::default()
        }),
        ..User::default()
    };

    assert!(
        !has_realizable_isolated_assets(&user, 0).unwrap(),
        "a cross deposit is out of reach of an isolated debt"
    );

    user.perp_positions[0].isolated_position_scaled_balance = SPOT_BALANCE_PRECISION as u64;
    assert!(
        has_realizable_isolated_assets(&user, 0).unwrap(),
        "the position's own collateral is what can pay it"
    );
}
