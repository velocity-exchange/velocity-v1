use {super::is_growth, crate::state::signed_msg_user::SignedMsgUserOrders};

/// A legacy record of 7 entries migrates in place to 4 entries, with 8 bytes
/// more than 4 entries need. A resize to 4 entries shrinks the account.
#[test]
fn a_resize_to_the_migrated_count_is_not_growth() {
    let data_len = SignedMsgUserOrders::legacy_space(7);
    let migrated_len = ((data_len - 48) / 40) as u32;
    assert_eq!(migrated_len, 4);
    assert!(SignedMsgUserOrders::space(4) < data_len);

    assert!(!is_growth(migrated_len, data_len, 4));
    assert!(is_growth(migrated_len, data_len, 5));
}

/// A legacy record of 10 entries fits in 6 entries of the current stride, but
/// 6 entries cannot hold its 10 uuids.
#[test]
fn a_resize_below_the_legacy_count_is_not_growth() {
    let data_len = SignedMsgUserOrders::legacy_space(10);
    assert!(SignedMsgUserOrders::space(6) >= data_len);

    assert!(!is_growth(10, data_len, 6));
    assert!(is_growth(10, data_len, 10));
}
