use {super::message_signer, crate::state::user::User, anchor_lang::prelude::Pubkey};

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
