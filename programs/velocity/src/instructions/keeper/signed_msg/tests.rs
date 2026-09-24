use {
    super::{message_signer, validate_entry_order_type},
    crate::state::{
        order_params::{OrderParams, PostOnlyParam},
        user::{OrderType, User},
    },
    anchor_lang::prelude::Pubkey,
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
            ..OrderParams::default()
        };

        assert!(validate_entry_order_type(&params).is_ok());
    }
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
