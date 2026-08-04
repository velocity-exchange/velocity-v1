use crate::{
    create_anchor_account_info,
    math::{
        bankruptcy::{is_cross_margin_bankrupt, is_isolated_margin_bankrupt},
        constants::{
            QUOTE_PRECISION_I64, SPOT_BALANCE_PRECISION, SPOT_CUMULATIVE_INTEREST_PRECISION,
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

/// Scaffolding for the map-taking predicate (OtterSec #145 / #151).
///
/// `pnl_pool_dollars` funds perp market 0's PnL pool and `deposit_interest` sets spot
/// market 0's cumulative deposit index. Both matter now: a positive perp quote only
/// vetoes bankruptcy when the pool can pay it, and a deposit row only vetoes when it
/// is worth at least one token.
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
        assert!(!is_cross_margin_bankrupt(&user, &spot_map, &perp_map).unwrap());
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

    // Payable out of a funded pool, so it still vetoes — unchanged behavior.
    healthy_maps!(|perp_map, spot_map| {
        assert!(!is_cross_margin_bankrupt(&user, &spot_map, &perp_map).unwrap());
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
        assert!(!is_cross_margin_bankrupt(&user, &spot_map, &perp_map).unwrap());
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
        assert!(is_cross_margin_bankrupt(&user, &spot_map, &perp_map).unwrap());
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
        assert!(is_cross_margin_bankrupt(&user, &spot_map, &perp_map).unwrap());
    });
}

#[test]
fn user_with_empty_position_and_balances() {
    let user = User::default();
    healthy_maps!(|perp_map, spot_map| {
        assert!(!is_cross_margin_bankrupt(&user, &spot_map, &perp_map).unwrap());
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
            is_cross_margin_bankrupt(&user, &spot_map, &perp_map).unwrap(),
            "a zero-token deposit residue must not veto bankruptcy"
        );
    });

    // A deposit actually worth >= 1 token still vetoes.
    let mut solvent = user;
    solvent.spot_positions[0].scaled_balance = SPOT_BALANCE_PRECISION as u64;
    healthy_maps!(|perp_map, spot_map| {
        assert!(
            !is_cross_margin_bankrupt(&solvent, &spot_map, &perp_map).unwrap(),
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
        assert!(
            !is_cross_margin_bankrupt(&user_with_scaled_balance, &spot_map, &perp_map).unwrap()
        );

        let mut user_with_base_asset_amount = user;
        user_with_base_asset_amount.perp_positions[0].base_asset_amount = 1000000000000000000;
        assert!(
            !is_cross_margin_bankrupt(&user_with_base_asset_amount, &spot_map, &perp_map).unwrap()
        );

        let mut user_with_open_order = user;
        user_with_open_order.perp_positions[0].open_orders = 1;
        assert!(!is_cross_margin_bankrupt(&user_with_open_order, &spot_map, &perp_map).unwrap());

        let mut user_with_positive_pnl = user;
        user_with_positive_pnl.perp_positions[0].quote_asset_amount = 1000000000000000000;
        assert!(!is_cross_margin_bankrupt(&user_with_positive_pnl, &spot_map, &perp_map).unwrap());

        let mut user_with_negative_pnl = user;
        user_with_negative_pnl.perp_positions[0].quote_asset_amount = -1000000000000000000;
        assert!(!is_cross_margin_bankrupt(&user_with_negative_pnl, &spot_map, &perp_map).unwrap());

        assert!(is_isolated_margin_bankrupt(&user_with_negative_pnl, 0).unwrap());
    });
}

/// Scaffolding for the cross-market #145 cases: perp market 0 holds the claim (pool funded to
/// `$claim_pool_dollars`), perp market 1 holds the debt with an empty pool.
macro_rules! with_two_perp_markets {
    ($claim_pool_dollars:expr, |$perp_map:ident, $spot_map:ident| $body:block) => {{
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

/// OtterSec #145: an *unfundable* positive claim must stop vetoing a real, resolvable loss elsewhere.
///
/// Under the old rule any positive `quote_asset_amount` vetoed outright, so a claim on a market whose
/// pnl pool could not pay it stranded the loss in the other market forever — the pool only fills as
/// counterparty losses settle, which may never happen, and until then the claim can never become a
/// deposit to clear the veto.
#[test]
fn unfundable_positive_claim_does_not_veto_bankruptcy() {
    let user = cross_market_estate(500, -1_000);

    // Claim market's pool is empty: the 500 cannot be realized at all.
    with_two_perp_markets!(0, |perp_map, spot_map| {
        assert!(
            is_cross_margin_bankrupt(&user, &spot_map, &perp_map).unwrap(),
            "an unfundable claim must not strand a resolvable loss in another market"
        );
    });
}

/// OtterSec #145, the guard that makes unpayability alone insufficient: a *net-solvent* estate is
/// still refused, however unfundable its claim is.
///
/// Without this, an account with a large unfundable claim and a small debt would be admitted and have
/// the whole claim extinguished to cover a fraction of it — confiscating value it was genuinely owed.
/// This is also the shape that made a payability-only check unsound against
/// `successful_liquidation_over_multiple_slots`, where a user holds $1050 of real positive PnL against
/// a pool that is empty simply because counterparty losses have not settled yet.
#[test]
fn net_solvent_estate_is_never_bankrupt_however_unfundable() {
    let user = cross_market_estate(5_000, -1_000);

    with_two_perp_markets!(0, |perp_map, spot_map| {
        assert!(
            !is_cross_margin_bankrupt(&user, &spot_map, &perp_map).unwrap(),
            "a net-solvent estate must never be admitted, or the extinguish step over-confiscates"
        );
    });
}

/// OtterSec #145: a claim the pool CAN pay still vetoes — that portion belongs in the ordinary
/// pipeline (settle -> deposit -> `liquidate_perp_pnl_for_deposit`), which needs no insurance at all.
#[test]
fn fundable_positive_claim_still_vetoes() {
    let user = cross_market_estate(500, -1_000);

    // The pool can cover the whole claim.
    with_two_perp_markets!(500, |perp_map, spot_map| {
        assert!(
            !is_cross_margin_bankrupt(&user, &spot_map, &perp_map).unwrap(),
            "a fundable claim must settle through the ordinary pipeline, not via insurance"
        );
    });

    // Even a partially fundable claim vetoes: settle that part first, then the remainder is
    // genuinely unfundable and admission proceeds.
    with_two_perp_markets!(200, |perp_map, spot_map| {
        assert!(
            !is_cross_margin_bankrupt(&user, &spot_map, &perp_map).unwrap(),
            "a partially fundable claim must still veto"
        );
    });
}
