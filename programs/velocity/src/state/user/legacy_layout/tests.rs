use {
    super::*,
    crate::state::user::{OrderStatus, PositionFlag},
    anchor_lang::Discriminator,
    std::mem::{offset_of, size_of},
};

// Offsets of the layout on master, which live `User` accounts hold.
#[test]
fn legacy_layout_matches_the_deployed_user() {
    assert_eq!(size_of::<LegacyUser>() + 8, LEGACY_USER_LEN);
    assert_eq!(LEGACY_USER_LEN, 4496);
    assert_eq!(size_of::<LegacyPerpPosition>(), 80);
    assert_eq!(offset_of!(LegacyPerpPosition, open_orders), 78);
    assert_eq!(offset_of!(LegacyPerpPosition, position_flag), 79);
    assert_eq!(offset_of!(LegacyUser, perp_positions), 416);
    assert_eq!(offset_of!(LegacyUser, orders), 1056);
    assert_eq!(offset_of!(LegacyUser, total_deposits), 4384);
    assert_eq!(offset_of!(LegacyUser, idle), 4462);
    assert_eq!(offset_of!(LegacyUser, open_orders), 4463);
    assert_eq!(offset_of!(LegacyUser, has_open_order), 4464);
    assert_eq!(offset_of!(LegacyUser, equity_floor), 4472);
}

/// A `u64` backing keeps the bytes as aligned as an account's data.
fn account_buffer(len: usize) -> Vec<u64> {
    assert_eq!(len % 8, 0);
    vec![0; len / 8]
}

fn legacy_account() -> Vec<u64> {
    let mut words = account_buffer(LEGACY_USER_LEN);
    let bytes: &mut [u8] = bytemuck::cast_slice_mut(&mut words);
    bytes[..8].copy_from_slice(User::DISCRIMINATOR);
    let legacy = bytemuck::from_bytes_mut::<LegacyUser>(&mut bytes[8..]);

    legacy.authority = Pubkey::new_from_array([1; 32]);
    legacy.delegate = Pubkey::new_from_array([2; 32]);
    legacy.name = [3; 32];
    legacy.spot_positions[7].scaled_balance = 4;
    for (i, position) in legacy.perp_positions.iter_mut().enumerate() {
        let i = i as i64;
        position.last_cumulative_funding_rate = 100 + i;
        position.base_asset_amount = 200 + i;
        position.quote_asset_amount = 300 + i;
        position.quote_break_even_amount = 400 + i;
        position.quote_entry_amount = 500 + i;
        position.open_bids = 600 + i;
        position.open_asks = -700 - i;
        position.settled_pnl = 800 + i;
        position.isolated_position_scaled_balance = 900 + i as u64;
        position.padding = [0xAA; 2];
        position.max_margin_ratio = 1000 + i as u16;
        position.market_index = i as u16;
        position.open_orders = 250 - i as u8;
        position.position_flag =
            PositionFlag::IsolatedPosition as u8 | PositionFlag::Bankrupt as u8;
    }

    legacy.orders[31].order_id = 31;
    legacy.orders[31].status = OrderStatus::Open;
    legacy.total_deposits = 5;
    legacy.total_withdraws = 6;
    legacy.total_social_loss = 7;
    legacy.settled_perp_pnl = -8;
    legacy.cumulative_spot_fees = -9;
    legacy.cumulative_perp_funding = -10;
    legacy.liquidation_margin_freed = 11;
    legacy.last_active_slot = 12;
    legacy.next_order_id = 13;
    legacy.max_margin_ratio = 14;
    legacy.next_liquidation_id = 15;
    legacy.sub_account_id = 16;
    legacy.status = 17;
    legacy.is_margin_trading_enabled = true;
    legacy.idle = true;
    legacy.open_orders = 240;
    legacy.has_open_order = true;
    legacy.open_auctions = 2;
    legacy.has_open_auction = true;
    legacy.pool_id = 18;
    legacy.special_user_status = 19;
    legacy.padding = [0xBB; 3];
    legacy.equity_floor = 20;
    legacy.equity_floor_buffer = 21;
    words
}

/// Runs the bytes the way `extend_account` does: copy, grow with zeros, rewrite.
fn migrate(legacy_words: &[u64]) -> Vec<u64> {
    let legacy_bytes: &[u8] = bytemuck::cast_slice(legacy_words);
    let legacy = LegacyUser::read(legacy_bytes);

    let mut words = account_buffer(User::SIZE);
    let bytes: &mut [u8] = bytemuck::cast_slice_mut(&mut words);
    bytes[..LEGACY_USER_LEN].copy_from_slice(legacy_bytes);
    legacy.write(bytes);
    words
}

#[test]
fn write_moves_every_perp_position_to_its_new_slot() {
    let words = migrate(&legacy_account());
    let bytes: &[u8] = bytemuck::cast_slice(&words);
    let user = bytemuck::from_bytes::<User>(&bytes[8..]);

    for (i, position) in user.perp_positions.iter().enumerate() {
        let n = i as i64;
        let expected = PerpPosition {
            last_cumulative_funding_rate: 100 + n,
            base_asset_amount: 200 + n,
            quote_asset_amount: 300 + n,
            quote_break_even_amount: 400 + n,
            quote_entry_amount: 500 + n,
            open_bids: 600 + n,
            open_asks: -700 - n,
            settled_pnl: 800 + n,
            isolated_position_scaled_balance: 900 + n as u64,
            reduce_only_clob_orders: 0,
            max_margin_ratio: 1000 + n as u16,
            market_index: i as u16,
            open_orders: 250 - i as u16,
            position_flag: PositionFlag::IsolatedPosition as u8 | PositionFlag::Bankrupt as u8,
            padding: [0; 7],
        };
        assert_eq!(*position, expected, "perp slot {i}");
    }
}

#[test]
fn write_keeps_every_other_user_field() {
    let legacy_words = legacy_account();
    let words = migrate(&legacy_words);
    let bytes: &[u8] = bytemuck::cast_slice(&words);
    let legacy_bytes: &[u8] = bytemuck::cast_slice(&legacy_words);
    let legacy = bytemuck::from_bytes::<LegacyUser>(&legacy_bytes[8..]);
    let user = bytemuck::from_bytes::<User>(&bytes[8..]);

    assert_eq!(&bytes[..8], User::DISCRIMINATOR);
    assert_eq!(user.authority, legacy.authority);
    assert_eq!(user.delegate, legacy.delegate);
    assert_eq!(user.name, legacy.name);
    assert_eq!(user.spot_positions, legacy.spot_positions);
    assert_eq!(user.orders, legacy.orders);
    assert_eq!(user.total_deposits, 5);
    assert_eq!(user.total_withdraws, 6);
    assert_eq!(user.total_social_loss, 7);
    assert_eq!(user.settled_perp_pnl, -8);
    assert_eq!(user.cumulative_spot_fees, -9);
    assert_eq!(user.cumulative_perp_funding, -10);
    assert_eq!(user.liquidation_margin_freed, 11);
    assert_eq!(user.last_active_slot, 12);
    assert_eq!(user.next_order_id, 13);
    assert_eq!(user.max_margin_ratio, 14);
    assert_eq!(user.next_liquidation_id, 15);
    assert_eq!(user.sub_account_id, 16);
    assert_eq!(user.status, 17);
    assert!(user.is_margin_trading_enabled);
    assert!(user.idle);
    assert_eq!(user.open_orders, 240);
    assert!(user.has_open_order);
    assert_eq!(user.open_auctions, 0);
    assert!(!user.has_open_auction);
    assert_eq!(user.pool_id, 18);
    assert_eq!(user.special_user_status, 19);
    assert_eq!(user.padding, [0; 2]);
    assert_eq!(user.equity_floor, 20);
    assert_eq!(user.equity_floor_buffer, 21);
}
