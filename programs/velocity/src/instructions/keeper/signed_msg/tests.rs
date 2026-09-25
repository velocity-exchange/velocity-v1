use {
    super::{
        admit_message, message_signer, validate_entry_order_type, PlacementEnv, SignedMsgTaker,
    },
    crate::{
        instructions::optional_accounts::AccountMaps,
        state::{
            oracle_map::OracleMap,
            order_params::{OrderParams, PostOnlyParam},
            perp_market_map::PerpMarketMap,
            signed_msg_user::{SignedMsgUserOrdersFixed, SignedMsgUserOrdersZeroCopyMut},
            spot_market_map::SpotMarketMap,
            state::State,
            user::{MarketType, OrderType, User, UserStats},
        },
        validation::sig_verification::VerifiedMessage,
    },
    anchor_lang::prelude::{Clock, Pubkey},
    std::cell::RefCell,
};

/// A user with no delegate holds the all-zero key, and that key is a
/// small-order point. A delegate-signed message for such a user is refused
/// before any signature check.
#[test]
fn a_default_delegate_cannot_sign() {
    let user = User {
        authority: Pubkey::new_unique(),
        ..User::default()
    };

    assert!(message_signer(&user, true).is_err());
    assert_eq!(message_signer(&user, false).unwrap(), user.authority);
}

#[test]
fn a_set_delegate_signs_for_its_user() {
    let user = User {
        authority: Pubkey::new_unique(),
        delegate: Pubkey::new_unique(),
        ..User::default()
    };

    assert_eq!(message_signer(&user, true).unwrap(), user.delegate);
}

#[test]
fn an_entry_that_can_take_is_admitted() {
    for order_type in [OrderType::Market, OrderType::Limit, OrderType::Oracle] {
        let params = OrderParams {
            order_type,
            price: 100,
            ..OrderParams::default()
        };

        assert!(validate_entry_order_type(&params).is_ok());
    }
}

/// An unnamed price would derive from the oracle at the slot the keeper
/// lands the message, so the signer would have signed no bound.
#[test]
fn a_market_entry_without_a_worst_price_is_refused() {
    let params = OrderParams {
        order_type: OrderType::Market,
        price: 0,
        ..OrderParams::default()
    };

    assert!(validate_entry_order_type(&params).is_err());
}

#[test]
fn a_post_only_entry_is_refused() {
    for post_only in [
        PostOnlyParam::MustPostOnly,
        PostOnlyParam::TryPostOnly,
        PostOnlyParam::Slide,
    ] {
        let params = OrderParams {
            order_type: OrderType::Limit,
            post_only,
            ..OrderParams::default()
        };

        assert!(validate_entry_order_type(&params).is_err());
    }
}

#[test]
fn a_trigger_entry_is_refused() {
    for order_type in [OrderType::TriggerMarket, OrderType::TriggerLimit] {
        let params = OrderParams {
            order_type,
            ..OrderParams::default()
        };

        assert!(validate_entry_order_type(&params).is_err());
    }
}

/// A message past its `max_ts` does not spend its uuid, so any keeper can
/// submit it again. None of its position settings may apply, or each
/// submission would apply them once more.
#[test]
fn an_expired_message_applies_no_position_settings() {
    let now = 1_000_000;
    let mut user = User::default();
    let mut stats = UserStats::default();
    let fixed = RefCell::new(SignedMsgUserOrdersFixed {
        user_pubkey: Pubkey::default(),
        version: 1,
        len: 4,
    });
    let data = RefCell::new([0u8; 160]);
    let mut orders = SignedMsgUserOrdersZeroCopyMut {
        fixed: fixed.borrow_mut(),
        data: data.borrow_mut(),
    };

    let message = VerifiedMessage {
        signed_msg_order_params: OrderParams {
            order_type: OrderType::Market,
            market_type: MarketType::Perp,
            price: 1,
            max_ts: Some(now - 1),
            ..OrderParams::default()
        },
        sub_account_id: Some(0),
        delegate_signed_taker_pubkey: None,
        slot: 100,
        uuid: [3; 8],
        take_profit_order_params: None,
        stop_loss_order_params: None,
        max_margin_ratio: Some(500),
        builder_idx: None,
        builder_fee_tenth_bps: None,
        isolated_position_deposit: Some(1),
        route: None,
        signature: [0; 64],
    };

    let mut maps = AccountMaps::new(
        PerpMarketMap::empty(),
        SpotMarketMap::empty(),
        OracleMap::empty(),
    );
    let state = State::default();
    let clock = Clock {
        slot: 100,
        unix_timestamp: now,
        ..Clock::default()
    };

    let admitted = admit_message(
        &mut SignedMsgTaker {
            key: Pubkey::new_unique(),
            user: &mut user,
            stats: &mut stats,
            orders: &mut orders,
        },
        &message,
        &mut PlacementEnv {
            maps: &mut maps,
            state: &state,
            clock: &clock,
        },
    );

    assert!(matches!(admitted, Ok(None)));
    assert_eq!(user.perp_positions, User::default().perp_positions);
}
