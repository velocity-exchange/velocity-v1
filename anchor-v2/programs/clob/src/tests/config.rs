//! The config bounds `initialize_market_v0` and `update_market_v0` share.

use {
    super::market::{test_config, TestMarket},
    crate::{
        book::ClobBook,
        config::{
            validate_market_config, MAX_ACTIVATION_DELAY_SLOTS_CEILING,
            UNKNOWN_USER_GRACE_SLOTS_CEILING,
        },
        state::{MarketConfigV0, EXECUTE_USERS_CEILING, USER_SET_CAPACITY},
    },
    anchor_lang::prelude::*,
};

fn initializes(config: MarketConfigV0) -> bool {
    TestMarket::uninitialized(8)
        .book()
        .initialize(
            Address::new_from_array([1u8; 32]),
            Address::new_from_array([2u8; 32]),
            config,
        )
        .is_ok()
}

#[test]
fn a_zero_grid_or_minimum_is_refused() {
    assert!(!initializes(MarketConfigV0 {
        order_tick_size: 0,
        ..test_config()
    }));
    assert!(!initializes(MarketConfigV0 {
        order_step_size: 0,
        ..test_config()
    }));
    assert!(!initializes(MarketConfigV0 {
        min_order_size: 0,
        ..test_config()
    }));
}

#[test]
fn a_minimum_off_the_step_is_refused() {
    let off_step = MarketConfigV0 {
        order_step_size: 10,
        min_order_size: 15,
        ..test_config()
    };
    assert!(!initializes(off_step));
    assert!(initializes(MarketConfigV0 {
        min_order_size: 20,
        ..off_step
    }));
}

#[test]
fn each_ceiling_admits_its_bound_and_refuses_one_past_it() {
    let at_bounds = MarketConfigV0 {
        max_activation_delay_slots: MAX_ACTIVATION_DELAY_SLOTS_CEILING,
        unknown_user_grace_slots: UNKNOWN_USER_GRACE_SLOTS_CEILING,
        max_execute_users: EXECUTE_USERS_CEILING,
        ..test_config()
    };
    assert!(initializes(at_bounds));
    assert!(!initializes(MarketConfigV0 {
        max_activation_delay_slots: MAX_ACTIVATION_DELAY_SLOTS_CEILING + 1,
        ..at_bounds
    }));
    assert!(!initializes(MarketConfigV0 {
        unknown_user_grace_slots: UNKNOWN_USER_GRACE_SLOTS_CEILING + 1,
        ..at_bounds
    }));
    assert!(!initializes(MarketConfigV0 {
        max_execute_users: EXECUTE_USERS_CEILING + 1,
        ..at_bounds
    }));
}

#[test]
fn the_execute_user_ceiling_is_the_user_set_capacity() {
    assert_eq!(EXECUTE_USERS_CEILING as usize, USER_SET_CAPACITY);
}

#[test]
fn a_header_edited_to_a_zero_tick_fails_the_check() {
    let market = TestMarket::new(8);
    let mut book = market.book();
    assert!(validate_market_config(&book).is_ok());

    book.order_tick_size = 0;
    assert!(validate_market_config(&book).is_err());
}
