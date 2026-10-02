use velocity::state::user::{PerpPosition, User};

// Struct offsets. An account offset is 8 more, for the discriminator.
#[test]
fn user_equity_floor_layout() {
    assert_eq!(std::mem::size_of::<User>(), 4552);
    assert_eq!(std::mem::offset_of!(User, equity_floor), 4536);
    assert_eq!(std::mem::offset_of!(User, equity_floor) % 8, 0);
    assert_eq!(std::mem::offset_of!(User, equity_floor_buffer), 4544);
    assert_eq!(std::mem::offset_of!(User, equity_floor_buffer) % 8, 0);
}

// `packages/sdk/src/memcmp.ts` and `rust/velocity-rs/crates/src/memcmp.rs`
// filter on these at account offsets 4534, 4535 and 4540.
#[test]
fn user_tail_flag_layout() {
    assert_eq!(std::mem::offset_of!(User, idle), 4526);
    assert_eq!(std::mem::offset_of!(User, has_open_order), 4527);
    assert_eq!(std::mem::offset_of!(User, open_orders), 4528);
    assert_eq!(std::mem::offset_of!(User, pool_id), 4532);
}

// `packages/sdk/src/decode/user.ts` reads these at fixed offsets.
#[test]
fn perp_position_layout() {
    assert_eq!(std::mem::size_of::<PerpPosition>(), 88);
    assert_eq!(std::mem::offset_of!(PerpPosition, open_orders), 78);
    assert_eq!(std::mem::offset_of!(PerpPosition, position_flag), 80);
}
