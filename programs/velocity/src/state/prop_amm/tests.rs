use {
    super::*,
    crate::{signer::find_quoter_signer, state::pdas},
};

fn meta(pubkey: Pubkey, is_writable: bool) -> AmmAccountMeta {
    AmmAccountMeta {
        pubkey,
        is_writable,
        padding: [0; 7],
    }
}

/// The key velocity signs quoter CPIs as must not be the key that authorizes
/// spending: signer privilege is inherited by a callee, so a quoter handed the
/// vault authority could forward it to the token program.
#[test]
fn quoter_signer_is_not_the_vault_authority() {
    let (quoter_signer, _) = find_quoter_signer();
    assert_eq!(quoter_signer, pdas::quoter_signer());
    assert_ne!(quoter_signer, pdas::velocity_signer());
    // Nor is it the protocol account's authority, which is derived from the
    // vault authority.
    let (protocol_user, protocol_user_stats) = pdas::protocol_user_pair();
    assert_ne!(pdas::user(&quoter_signer, 0), protocol_user);
    assert_ne!(pdas::user_stats(&quoter_signer), protocol_user_stats);
}

/// Only the quoter signer's slot is handed signer privilege. The vault
/// authority is passed unprivileged even if it somehow reached a stored list
/// (entries registered before the reserved-key check shipped).
#[test]
fn only_the_quoter_signer_slot_is_a_signer() {
    let vault_authority = pdas::velocity_signer();
    let (quoter_signer, _) = find_quoter_signer();
    let book = Pubkey::new_unique();
    let taker_wallet = Pubkey::new_unique();

    let registered = [
        meta(book, true),
        meta(quoter_signer, false),
        meta(vault_authority, false),
        meta(taker_wallet, false),
    ];
    let metas = quoter_account_metas(&registered, &quoter_signer);

    assert_eq!(
        metas
            .iter()
            .filter(|m| m.is_signer)
            .map(|m| m.pubkey)
            .collect::<Vec<_>>(),
        vec![quoter_signer]
    );
    assert_eq!(
        metas
            .iter()
            .filter(|m| m.is_writable)
            .map(|m| m.pubkey)
            .collect::<Vec<_>>(),
        vec![book]
    );
}

#[test]
fn registration_rejects_the_vault_authority() {
    let vault_authority = pdas::velocity_signer();
    let (quoter_signer, _) = find_quoter_signer();
    let book = Pubkey::new_unique();

    assert!(validate_quoter_accounts([book, quoter_signer].iter()).is_ok());
    assert!(validate_quoter_accounts([book, vault_authority].iter()).is_err());
    assert!(validate_quoter_accounts([vault_authority].iter()).is_err());
}
