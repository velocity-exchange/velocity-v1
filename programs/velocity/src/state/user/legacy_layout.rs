//! The `User` layout before `open_orders` became a `u16`.
//!
//! That change moves fields, and a resize only appends zeros. `PerpPosition`
//! grows from 80 to 88 bytes, so every field after the first slot moves.
//! `has_open_order` also moves ahead of `open_orders`. `extend_account` rewrites
//! an account at [`LEGACY_USER_LEN`] through [`LegacyUser::write`] as it grows it.

use {
    super::{Order, PerpPosition, SpotPosition, User},
    crate::state::traits::Size,
    anchor_lang::prelude::*,
    static_assertions::const_assert_eq,
};

/// The account length of a `User` in the legacy layout, discriminator included.
pub const LEGACY_USER_LEN: usize = 8 + 4488;

#[derive(Clone, Copy)]
#[repr(C)]
pub struct LegacyPerpPosition {
    pub last_cumulative_funding_rate: i64,
    pub base_asset_amount: i64,
    pub quote_asset_amount: i64,
    pub quote_break_even_amount: i64,
    pub quote_entry_amount: i64,
    pub open_bids: i64,
    pub open_asks: i64,
    pub settled_pnl: i64,
    pub isolated_position_scaled_balance: u64,
    pub padding: [u8; 2],
    pub max_margin_ratio: u16,
    pub market_index: u16,
    pub open_orders: u8,
    pub position_flag: u8,
}

#[derive(Clone, Copy)]
#[repr(C)]
pub struct LegacyUser {
    pub authority: Pubkey,
    pub delegate: Pubkey,
    pub name: [u8; 32],
    pub spot_positions: [SpotPosition; 8],
    pub perp_positions: [LegacyPerpPosition; 8],
    pub orders: [Order; 32],
    pub total_deposits: u64,
    pub total_withdraws: u64,
    pub total_social_loss: u64,
    pub settled_perp_pnl: i64,
    pub cumulative_spot_fees: i64,
    pub cumulative_perp_funding: i64,
    pub liquidation_margin_freed: u64,
    pub last_active_slot: u64,
    pub next_order_id: u32,
    pub max_margin_ratio: u32,
    pub next_liquidation_id: u16,
    pub sub_account_id: u16,
    pub status: u8,
    pub is_margin_trading_enabled: bool,
    pub idle: bool,
    pub open_orders: u8,
    pub has_open_order: bool,
    pub open_auctions: u8,
    pub has_open_auction: bool,
    pub pool_id: u8,
    pub special_user_status: u8,
    pub padding: [u8; 3],
    pub equity_floor: u64,
    pub equity_floor_buffer: u64,
}

// `Pod` needs a layout without implicit padding. The `padding` fields fill every gap.
const_assert_eq!(std::mem::size_of::<LegacyUser>(), LEGACY_USER_LEN - 8);
unsafe impl bytemuck::Pod for LegacyUser {}
unsafe impl bytemuck::Zeroable for LegacyUser {}

impl From<&LegacyPerpPosition> for PerpPosition {
    /// `reduce_only_clob_orders` starts at zero, because a legacy account has
    /// no order on a CLOB.
    fn from(legacy: &LegacyPerpPosition) -> Self {
        PerpPosition {
            last_cumulative_funding_rate: legacy.last_cumulative_funding_rate,
            base_asset_amount: legacy.base_asset_amount,
            quote_asset_amount: legacy.quote_asset_amount,
            quote_break_even_amount: legacy.quote_break_even_amount,
            quote_entry_amount: legacy.quote_entry_amount,
            open_bids: legacy.open_bids,
            open_asks: legacy.open_asks,
            settled_pnl: legacy.settled_pnl,
            isolated_position_scaled_balance: legacy.isolated_position_scaled_balance,
            reduce_only_clob_orders: 0,
            max_margin_ratio: legacy.max_margin_ratio,
            market_index: legacy.market_index,
            open_orders: legacy.open_orders.into(),
            position_flag: legacy.position_flag,
            padding: [0; 7],
        }
    }
}

impl LegacyUser {
    /// Copies the body of a `User` account at [`LEGACY_USER_LEN`]. The copy
    /// never passes through the stack, because a `LegacyUser` is larger than
    /// the 4 KB SBF frame.
    pub fn read(account_data: &[u8]) -> Box<LegacyUser> {
        // SAFETY: `LegacyUser` is a zero-copy `Pod` struct, so all-zero bytes
        // are a valid value.
        let mut legacy_user = unsafe { Box::<LegacyUser>::new_zeroed().assume_init() };
        bytemuck::bytes_of_mut(&mut *legacy_user)
            .copy_from_slice(&account_data[8..LEGACY_USER_LEN]);
        legacy_user
    }

    /// Overwrites the body of a `User` account at `User::SIZE` with `self` in
    /// the current layout. The auction counters become zero, because no
    /// instruction decrements them now.
    pub fn write(&self, account_data: &mut [u8]) {
        let body = &mut account_data[8..User::SIZE];
        body.fill(0);
        let user = bytemuck::from_bytes_mut::<User>(body);

        user.authority = self.authority;
        user.delegate = self.delegate;
        user.name = self.name;
        user.spot_positions = self.spot_positions;
        for (position, legacy) in user.perp_positions.iter_mut().zip(&self.perp_positions) {
            *position = PerpPosition::from(legacy);
        }

        user.orders.copy_from_slice(&self.orders);

        user.total_deposits = self.total_deposits;
        user.total_withdraws = self.total_withdraws;
        user.total_social_loss = self.total_social_loss;
        user.settled_perp_pnl = self.settled_perp_pnl;
        user.cumulative_spot_fees = self.cumulative_spot_fees;
        user.cumulative_perp_funding = self.cumulative_perp_funding;
        user.liquidation_margin_freed = self.liquidation_margin_freed;
        user.last_active_slot = self.last_active_slot;
        user.next_order_id = self.next_order_id;
        user.max_margin_ratio = self.max_margin_ratio;
        user.next_liquidation_id = self.next_liquidation_id;
        user.sub_account_id = self.sub_account_id;
        user.status = self.status;
        user.is_margin_trading_enabled = self.is_margin_trading_enabled;
        user.idle = self.idle;
        user.has_open_order = self.has_open_order;
        user.open_orders = self.open_orders.into();
        user.open_auctions = 0;
        user.has_open_auction = false;
        user.pool_id = self.pool_id;
        user.special_user_status = self.special_user_status;
        user.equity_floor = self.equity_floor;
        user.equity_floor_buffer = self.equity_floor_buffer;
    }
}

#[cfg(test)]
mod tests;
