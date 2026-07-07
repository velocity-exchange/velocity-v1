use velocity::state::user::User;

// Guards the equity_floor layout: same total size as before the field was
// added (carved out of tail padding) and 8-aligned so the on-chain and host
// representations agree.
#[test]
fn user_equity_floor_layout() {
    assert_eq!(std::mem::size_of::<User>(), 4488);
    assert_eq!(std::mem::offset_of!(User, equity_floor), 4472);
    assert_eq!(std::mem::offset_of!(User, equity_floor) % 8, 0);
}
