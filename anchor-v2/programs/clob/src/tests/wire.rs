//! Every discriminator `clob-wire` publishes must be the one anchor derives.
//!
//! Anchor derives a discriminator from the handler's name, so a rename here is
//! silent on the caller's side. Velocity builds its CPI data from `clob-wire`,
//! and this test is what makes that constant a pin rather than a copy.

use {
    crate::instruction::{
        CancelAllV0, CancelOrderV0, EvictWorstV0, FillV0, NextCrossV0, NextRemovalV0, OrderRulesV0,
        OrdersV0, PlaceOrderV0, RemoveExpiredV0, SetCrankConditionsV0,
    },
    anchor_lang::Discriminator,
    clob_wire::discriminator,
};

#[test]
fn the_published_discriminators_are_the_ones_anchor_derives() {
    assert_eq!(discriminator::PLACE_ORDER_V0, PlaceOrderV0::DISCRIMINATOR);
    assert_eq!(discriminator::CANCEL_ORDER_V0, CancelOrderV0::DISCRIMINATOR);
    assert_eq!(discriminator::FILL_V0, FillV0::DISCRIMINATOR);
    assert_eq!(discriminator::CANCEL_ALL_V0, CancelAllV0::DISCRIMINATOR);
    assert_eq!(discriminator::EVICT_WORST_V0, EvictWorstV0::DISCRIMINATOR);
    assert_eq!(
        discriminator::REMOVE_EXPIRED_V0,
        RemoveExpiredV0::DISCRIMINATOR
    );
    assert_eq!(discriminator::NEXT_REMOVAL_V0, NextRemovalV0::DISCRIMINATOR);
    assert_eq!(
        discriminator::SET_CRANK_CONDITIONS_V0,
        SetCrankConditionsV0::DISCRIMINATOR
    );
    assert_eq!(discriminator::ORDERS_V0, OrdersV0::DISCRIMINATOR);
    assert_eq!(discriminator::NEXT_CROSS_V0, NextCrossV0::DISCRIMINATOR);
    assert_eq!(discriminator::ORDER_RULES_V0, OrderRulesV0::DISCRIMINATOR);
}
