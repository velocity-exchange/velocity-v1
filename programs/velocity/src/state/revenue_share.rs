use std::cell::{Ref, RefMut};

use anchor_lang::prelude::borsh::{BorshDeserialize, BorshSerialize};
use anchor_lang::prelude::Pubkey;
use anchor_lang::*;
use anchor_lang::{account, zero_copy};
use prelude::AccountInfo;

use crate::error::{ErrorCode, VelocityResult};
use crate::math::casting::Cast;
use crate::math::safe_unwrap::SafeUnwrap;
use crate::state::user::{MarketType, OrderStatus, User};
use crate::validate;
use crate::{msg, ID};

pub const REVENUE_SHARE_PDA_SEED: &str = "REV_SHARE";
pub const REVENUE_SHARE_ESCROW_PDA_SEED: &str = "REV_ESCROW";

#[derive(Clone, Copy, BorshSerialize, BorshDeserialize, PartialEq, Debug, Eq, Default)]
#[borsh(use_discriminant = true)]
pub enum RevenueShareOrderBitFlag {
    #[default]
    Init = 0b00000000,
    Open = 0b00000001,
    Completed = 0b00000010,
    Referral = 0b00000100,
}

#[account(zero_copy(unsafe))]
#[derive(Eq, PartialEq, Debug, Default)]
#[repr(C)]
pub struct RevenueShare {
    /// the owner of this account, a builder or referrer
    pub authority: Pubkey,
    pub total_referrer_rewards: u64,
    pub total_builder_rewards: u64,
    pub padding: [u8; 24],
}

impl RevenueShare {
    pub fn space() -> usize {
        8 + std::mem::size_of::<RevenueShare>()
    }
}

#[zero_copy(unsafe)]
#[derive(Default, Eq, PartialEq, Debug, BorshDeserialize, BorshSerialize)]
#[repr(C)]
pub struct RevenueShareOrder {
    /// fees accrued so far for this order slot. This is not exclusively fees from this order_id
    /// and may include fees from other orders in the same market. This may be swept to the
    /// builder's SpotPosition during settle_pnl.
    pub fees_accrued: u64,
    /// the order_id of the current active order in this slot. It's only relevant while bit_flag = Open
    pub order_id: u32,
    /// the builder fee on this order, in tenths of a bps, e.g. 100 = 0.01%
    pub fee_tenth_bps: u16,
    pub market_index: u16,
    /// the subaccount_id of the user who created this order. It's only relevant while bit_flag = Open
    pub sub_account_id: u16,
    /// the index of the RevenueShareEscrow.approved_builders list, that this order's fee will settle to. Ignored
    /// if bit_flag = Referral.
    pub builder_idx: u8,
    /// bitflags that describe the state of the order.
    /// [`RevenueShareOrderBitFlag::Init`]: this order slot is available for use.
    /// [`RevenueShareOrderBitFlag::Open`]: this order slot is occupied, `order_id` is the `sub_account_id`'s active order.
    /// [`RevenueShareOrderBitFlag::Completed`]: this order has been filled or canceled, and is waiting to be settled into.
    /// the builder's account order_id and sub_account_id are no longer relevant, it may be merged with other orders.
    /// [`RevenueShareOrderBitFlag::Referral`]: this order stores referral rewards waiting to be settled for this market.
    /// If it is set, no other bitflag should be set.
    pub bit_flags: u8,
    /// the index into the User's orders list when this RevenueShareOrder was created, make sure to verify that order_id matches.
    pub user_order_index: u8,
    pub market_type: MarketType,
    pub padding: [u8; 10],
}

unsafe impl bytemuck::Pod for RevenueShareOrder {}
unsafe impl bytemuck::Zeroable for RevenueShareOrder {}

impl RevenueShareOrder {
    pub fn new(
        builder_idx: u8,
        sub_account_id: u16,
        order_id: u32,
        fee_tenth_bps: u16,
        market_type: MarketType,
        market_index: u16,
        bit_flags: u8,
        user_order_index: u8,
    ) -> Self {
        Self {
            builder_idx,
            order_id,
            fee_tenth_bps,
            market_type,
            market_index,
            fees_accrued: 0,
            bit_flags,
            sub_account_id,
            user_order_index,
            padding: [0; 10],
        }
    }

    pub fn space() -> usize {
        std::mem::size_of::<RevenueShareOrder>()
    }

    pub fn add_bit_flag(&mut self, flag: RevenueShareOrderBitFlag) {
        self.bit_flags |= flag as u8;
    }

    pub fn is_bit_flag_set(&self, flag: RevenueShareOrderBitFlag) -> bool {
        (self.bit_flags & flag as u8) != 0
    }

    // An order is Open after it is created, the slot is considered occupied
    // and it is waiting to become `Completed` (filled or canceled).
    pub fn is_open(&self) -> bool {
        self.is_bit_flag_set(RevenueShareOrderBitFlag::Open)
    }

    // An order is Completed after it is filled or canceled. It is waiting to be settled
    // into the builder's account
    pub fn is_completed(&self) -> bool {
        self.is_bit_flag_set(RevenueShareOrderBitFlag::Completed)
    }

    /// An order slot is available (can be written to) if it is neither Completed nor Open.
    pub fn is_available(&self) -> bool {
        !self.is_completed() && !self.is_open() && !self.is_referral_order()
    }

    pub fn is_referral_order(&self) -> bool {
        self.is_bit_flag_set(RevenueShareOrderBitFlag::Referral)
    }

    /// Checks if `self` can be merged with `other`. Merged orders track cumulative fees accrued
    /// and are settled together, making more efficient use of the orders list.
    pub fn is_mergeable(&self, other: &RevenueShareOrder) -> bool {
        (self.is_referral_order() == other.is_referral_order())
            && other.is_completed()
            && other.market_index == self.market_index
            && other.market_type == self.market_type
            && other.builder_idx == self.builder_idx
    }

    /// Merges `other` into `self`. The orders must be mergeable.
    pub fn merge(mut self, other: &RevenueShareOrder) -> VelocityResult<RevenueShareOrder> {
        validate!(
            self.is_mergeable(other),
            ErrorCode::DefaultError,
            "Orders are not mergeable"
        )?;
        self.fees_accrued = self
            .fees_accrued
            .checked_add(other.fees_accrued)
            .ok_or(ErrorCode::MathError)?;
        Ok(self)
    }
}

#[zero_copy(unsafe)]
#[derive(Default, Eq, PartialEq, Debug, BorshDeserialize, BorshSerialize)]
#[repr(C)]
pub struct BuilderInfo {
    pub authority: Pubkey, // builder authority
    pub max_fee_tenth_bps: u16,
    pub padding: [u8; 6],
}

unsafe impl bytemuck::Pod for BuilderInfo {}
unsafe impl bytemuck::Zeroable for BuilderInfo {}

impl BuilderInfo {
    pub fn space() -> usize {
        std::mem::size_of::<BuilderInfo>()
    }

    pub fn is_revoked(&self) -> bool {
        self.max_fee_tenth_bps == 0
    }
}

#[account]
#[derive(Eq, PartialEq, Debug, Default)]
pub struct RevenueShareEscrow {
    /// the owner of this account, a user
    pub authority: Pubkey,
    pub referrer: Pubkey,
    pub reserved_fixed: [u8; 24],
    pub padding0: u32, // align with [`RevenueShareEscrow::orders`] 4 bytes len prefix
    pub orders: Vec<RevenueShareOrder>,
    pub padding1: u32, // align with [`RevenueShareEscrow::approved_builders`] 4 bytes len prefix
    pub approved_builders: Vec<BuilderInfo>,
}

impl RevenueShareEscrow {
    pub fn space(num_orders: usize, num_builders: usize) -> usize {
        8 + // discriminator
        std::mem::size_of::<RevenueShareEscrowFixed>() + // fixed header
        4 + // orders Vec length prefix
        4 + // padding0
        num_orders * std::mem::size_of::<RevenueShareOrder>() + // orders data
        4 + // approved_builders Vec length prefix
        4 + // padding1
        num_builders * std::mem::size_of::<BuilderInfo>() // builders data
    }

    pub fn validate(&self) -> VelocityResult<()> {
        validate!(
            self.orders.len() <= 128 && self.approved_builders.len() <= 128,
            ErrorCode::DefaultError,
            "RevenueShareEscrow orders and approved_builders len must be between 1 and 128"
        )?;
        Ok(())
    }
}

#[zero_copy(unsafe)]
#[derive(Eq, PartialEq, Debug, BorshDeserialize, BorshSerialize)]
#[repr(C)]
#[derive(Default)]
pub struct RevenueShareEscrowFixed {
    pub authority: Pubkey,
    pub referrer: Pubkey,
    pub reserved_fixed: [u8; 24],
}

unsafe impl bytemuck::Pod for RevenueShareEscrowFixed {}
unsafe impl bytemuck::Zeroable for RevenueShareEscrowFixed {}

pub struct RevenueShareEscrowZeroCopy<'a> {
    pub fixed: Ref<'a, RevenueShareEscrowFixed>,
    pub data: Ref<'a, [u8]>,
}

impl<'a> RevenueShareEscrowZeroCopy<'a> {
    pub fn orders_len(&self) -> u32 {
        let length_bytes = &self.data[4..8];
        u32::from_le_bytes([
            length_bytes[0],
            length_bytes[1],
            length_bytes[2],
            length_bytes[3],
        ])
    }
    pub fn approved_builders_len(&self) -> u32 {
        let orders_data_size =
            self.orders_len() as usize * std::mem::size_of::<RevenueShareOrder>();
        let offset = 4 + // RevenueShareEscrow.padding0
        4 + // vec len
        orders_data_size + 4; // RevenueShareEscrow.padding1
        let length_bytes = &self.data[offset..offset + 4];
        u32::from_le_bytes([
            length_bytes[0],
            length_bytes[1],
            length_bytes[2],
            length_bytes[3],
        ])
    }

    pub fn get_order(&self, index: u32) -> VelocityResult<&RevenueShareOrder> {
        validate!(
            index < self.orders_len(),
            ErrorCode::DefaultError,
            "Order index out of bounds"
        )?;
        let size = std::mem::size_of::<RevenueShareOrder>();
        let start = 4 + // RevenueShareEscrow.padding0
        4 + // vec len
        index as usize * size; // orders data
        Ok(bytemuck::from_bytes(&self.data[start..start + size]))
    }

    pub fn get_approved_builder(&self, index: u32) -> VelocityResult<&BuilderInfo> {
        validate!(
            index < self.approved_builders_len(),
            ErrorCode::DefaultError,
            "Builder index out of bounds"
        )?;
        let size = std::mem::size_of::<BuilderInfo>();
        let offset = 4 + 4 + // Skip orders Vec length prefix + padding0
            self.orders_len() as usize * std::mem::size_of::<RevenueShareOrder>() + // orders data
            4; // Skip approved_builders Vec length prefix + padding1
        let start = offset + index as usize * size;
        Ok(bytemuck::from_bytes(&self.data[start..start + size]))
    }

    pub fn iter_orders(&self) -> impl Iterator<Item = VelocityResult<&RevenueShareOrder>> + '_ {
        (0..self.orders_len()).map(move |i| self.get_order(i))
    }

    pub fn iter_approved_builders(
        &self,
    ) -> impl Iterator<Item = VelocityResult<&BuilderInfo>> + '_ {
        (0..self.approved_builders_len()).map(move |i| self.get_approved_builder(i))
    }
}

pub struct RevenueShareEscrowZeroCopyMut<'a> {
    pub fixed: RefMut<'a, RevenueShareEscrowFixed>,
    pub data: RefMut<'a, [u8]>,
}

impl<'a> RevenueShareEscrowZeroCopyMut<'a> {
    pub fn has_referrer(&self) -> bool {
        self.fixed.referrer != Pubkey::default()
    }

    pub fn get_referrer(&self) -> Option<Pubkey> {
        if self.has_referrer() {
            Some(self.fixed.referrer)
        } else {
            None
        }
    }

    pub fn orders_len(&self) -> u32 {
        // skip RevenueShareEscrow.padding0
        let length_bytes = &self.data[4..8];
        u32::from_le_bytes([
            length_bytes[0],
            length_bytes[1],
            length_bytes[2],
            length_bytes[3],
        ])
    }
    pub fn approved_builders_len(&self) -> u32 {
        // Calculate offset to the approved_builders Vec length
        let orders_data_size =
            self.orders_len() as usize * std::mem::size_of::<RevenueShareOrder>();
        let offset = 4 + // RevenueShareEscrow.padding0
        4 + // vec len
        orders_data_size +
        4; // RevenueShareEscrow.padding1
        let length_bytes = &self.data[offset..offset + 4];
        u32::from_le_bytes([
            length_bytes[0],
            length_bytes[1],
            length_bytes[2],
            length_bytes[3],
        ])
    }

    pub fn get_order_mut(&mut self, index: u32) -> VelocityResult<&mut RevenueShareOrder> {
        validate!(
            index < self.orders_len(),
            ErrorCode::DefaultError,
            "Order index out of bounds"
        )?;
        let size = std::mem::size_of::<RevenueShareOrder>();
        let start = 4 + // RevenueShareEscrow.padding0
        4 + // vec len
        index as usize * size;
        Ok(bytemuck::from_bytes_mut(
            &mut self.data[start..(start + size)],
        ))
    }

    /// Returns the index of an order for a given sub_account_id and order_id, if present.
    pub fn find_order_index(&self, sub_account_id: u16, order_id: u32) -> Option<u32> {
        for i in 0..self.orders_len() {
            if let Ok(existing_order) = self.get_order(i) {
                if existing_order.order_id == order_id
                    && existing_order.sub_account_id == sub_account_id
                {
                    return Some(i);
                }
            }
        }
        None
    }

    /// Returns the index of the live builder-order row for a fill, binding the match to the
    /// market being filled — not just `(sub_account_id, order_id)`.
    ///
    /// `(sub_account_id, order_id)` alone is ambiguous: order ids are per-subaccount and are
    /// reused across markets, and a builder row created by `add_builder_order` can linger past
    /// its order (a placement that soft-skips after writing the row, a `Completed` row awaiting
    /// sweep). Matching on order id alone lets a stale row from market A attach to a same-id
    /// order filled in market B, so the market-B taker is charged a builder fee that accrues to —
    /// and is later swept from — market A's pnl pool (OtterSec #88). Requiring the row's
    /// `market_index`/`market_type` to equal the fill's, that it still be `Open` (not a
    /// `Completed` row whose id is stale), and that it not be a referral row binds the fee terms
    /// to the order actually being filled.
    pub fn find_builder_order_index(
        &self,
        sub_account_id: u16,
        order_id: u32,
        market_index: u16,
        market_type: MarketType,
    ) -> Option<u32> {
        for i in 0..self.orders_len() {
            if let Ok(existing_order) = self.get_order(i) {
                if existing_order.order_id == order_id
                    && existing_order.sub_account_id == sub_account_id
                    && existing_order.market_index == market_index
                    && existing_order.market_type == market_type
                    // Completed rows keep their Open bit (`add_bit_flag` never
                    // clears), so Open alone does not exclude them.
                    && existing_order.is_open()
                    && !existing_order.is_completed()
                    && !existing_order.is_referral_order()
                {
                    return Some(i);
                }
            }
        }
        None
    }

    /// Returns the index for the referral order, creating one if necessary. Returns None if the
    /// escrow has no referrer (a referral slot without a referrer could never be swept, so one is
    /// never claimed) or if a new order cannot be created.
    pub fn find_or_create_referral_index(&mut self, market_index: u16) -> Option<u32> {
        if !self.has_referrer() {
            return None;
        }

        // look for an existing referral order
        for i in 0..self.orders_len() {
            if let Ok(existing_order) = self.get_order(i) {
                if existing_order.is_referral_order() && existing_order.market_index == market_index
                {
                    return Some(i);
                }
            }
        }

        // try to create a referral order in an available order slot
        match self.add_order(RevenueShareOrder::new(
            0,
            0,
            0,
            0,
            MarketType::Perp,
            market_index,
            RevenueShareOrderBitFlag::Referral as u8,
            0,
        )) {
            Ok(idx) => Some(idx),
            Err(_) => {
                msg!("Failed to add referral order, RevenueShareEscrow is full");
                None
            }
        }
    }

    pub fn get_order(&self, index: u32) -> VelocityResult<&RevenueShareOrder> {
        validate!(
            index < self.orders_len(),
            ErrorCode::DefaultError,
            "Order index out of bounds"
        )?;
        let size = std::mem::size_of::<RevenueShareOrder>();
        let start = 4 + // RevenueShareEscrow.padding0
        4 + // vec len
        index as usize * size; // orders data
        Ok(bytemuck::from_bytes(&self.data[start..start + size]))
    }

    pub fn get_approved_builder_mut(&mut self, index: u8) -> VelocityResult<&mut BuilderInfo> {
        validate!(
            index < self.approved_builders_len().cast::<u8>()?,
            ErrorCode::DefaultError,
            "Builder index out of bounds, index: {}, orderslen: {}, builderslen: {}",
            index,
            self.orders_len(),
            self.approved_builders_len()
        )?;
        let size = std::mem::size_of::<BuilderInfo>();
        let offset = 4 + // RevenueShareEscrow.padding0
            4 + // vec len
            self.orders_len() as usize * std::mem::size_of::<RevenueShareOrder>() + // orders data
            4 + // RevenueShareEscrow.padding1
            4; // vec len
        let start = offset + index as usize * size;
        Ok(bytemuck::from_bytes_mut(
            &mut self.data[start..start + size],
        ))
    }

    pub fn add_order(&mut self, order: RevenueShareOrder) -> VelocityResult<u32> {
        for i in 0..self.orders_len() {
            let existing_order = self.get_order_mut(i)?;
            if existing_order.is_mergeable(&order) {
                *existing_order = existing_order.merge(&order)?;
                return Ok(i);
            } else if existing_order.is_available() {
                *existing_order = order;
                return Ok(i);
            }
        }

        Err(ErrorCode::RevenueShareEscrowOrdersAccountFull)
    }

    /// Marks any [`RevenueShareOrder`]s as Complete if there is no longer a corresponding
    /// open order in the user's account. This is used to lazily reconcile state when
    /// in place_order and settle_pnl instead of requiring explicit updates on cancels.
    pub fn revoke_completed_orders(&mut self, user: &User) -> VelocityResult<()> {
        for i in 0..self.orders_len() {
            if let Ok(rev_share_order) = self.get_order_mut(i) {
                if rev_share_order.is_referral_order() {
                    continue;
                }
                if user.sub_account_id != rev_share_order.sub_account_id {
                    continue;
                }
                if rev_share_order.is_open() && !rev_share_order.is_completed() {
                    // Match by (sub_account_id, order_id) across the whole order
                    // list, not the row's stored `user_order_index`. That index
                    // is captured at placement and can go stale (the order moves
                    // to a different slot, or a later order occupies that slot),
                    // which made this incorrectly mark a still-open fee-bearing
                    // row Completed and clear it early (OtterSec #82). Order ids
                    // are unique among a user's open orders, so the scan is exact.
                    let still_open = user.orders.iter().any(|user_order| {
                        user_order.status == OrderStatus::Open
                            && user_order.order_id == rev_share_order.order_id
                    });
                    if !still_open {
                        if rev_share_order.fees_accrued > 0 {
                            rev_share_order.add_bit_flag(RevenueShareOrderBitFlag::Completed);
                        } else {
                            // order had no fees accrued, we can just clear out the slot
                            *rev_share_order = RevenueShareOrder::default();
                        }
                    }
                }
            }
        }

        Ok(())
    }
}

pub trait RevenueShareEscrowLoader<'a> {
    fn load_zc(&self) -> VelocityResult<RevenueShareEscrowZeroCopy<'_>>;
    fn load_zc_mut(&self) -> VelocityResult<RevenueShareEscrowZeroCopyMut<'_>>;
}

impl<'a> RevenueShareEscrowLoader<'a> for AccountInfo<'a> {
    fn load_zc(&self) -> VelocityResult<RevenueShareEscrowZeroCopy<'_>> {
        let owner = self.owner;

        validate!(
            owner == &ID,
            ErrorCode::DefaultError,
            "invalid RevenueShareEscrow owner",
        )?;

        let data = self.try_borrow_data().safe_unwrap()?;

        let (discriminator, data) = Ref::map_split(data, |d| d.split_at(8));
        validate!(
            discriminator.as_ref() == RevenueShareEscrow::DISCRIMINATOR,
            ErrorCode::DefaultError,
            "invalid signed_msg user orders discriminator",
        )?;

        let hdr_size = std::mem::size_of::<RevenueShareEscrowFixed>();
        let (fixed, data) = Ref::map_split(data, |d| d.split_at(hdr_size));
        Ok(RevenueShareEscrowZeroCopy {
            fixed: Ref::map(fixed, |b| bytemuck::from_bytes(b)),
            data,
        })
    }

    fn load_zc_mut(&self) -> VelocityResult<RevenueShareEscrowZeroCopyMut<'_>> {
        let owner = self.owner;

        validate!(
            owner == &ID,
            ErrorCode::DefaultError,
            "invalid RevenueShareEscrow owner",
        )?;

        let data = self.try_borrow_mut_data().safe_unwrap()?;

        let (discriminator, data) = RefMut::map_split(data, |d| d.split_at_mut(8));
        validate!(
            discriminator.as_ref() == RevenueShareEscrow::DISCRIMINATOR,
            ErrorCode::DefaultError,
            "invalid signed_msg user orders discriminator",
        )?;

        let hdr_size = std::mem::size_of::<RevenueShareEscrowFixed>();
        let (fixed, data) = RefMut::map_split(data, |d| d.split_at_mut(hdr_size));
        Ok(RevenueShareEscrowZeroCopyMut {
            fixed: RefMut::map(fixed, |b| bytemuck::from_bytes_mut(b)),
            data,
        })
    }
}

#[cfg(test)]
mod revoke_completed_orders_tests {
    use super::*;
    use crate::state::user::{Order, OrderStatus, User};
    use std::cell::RefCell;

    fn open_builder_row(order_id: u32, user_order_index: u8, fees: u64) -> RevenueShareOrder {
        let mut o = RevenueShareOrder::new(
            0,
            0,
            order_id,
            100,
            MarketType::Perp,
            0,
            RevenueShareOrderBitFlag::Open as u8,
            user_order_index,
        );
        o.fees_accrued = fees;
        o
    }

    /// Build a 16-byte-aligned escrow buffer (so `bytemuck` reads of the
    /// 8-aligned orders never trip alignment) with `orders` written via the same
    /// byte path production uses.
    fn escrow_backing(orders: &[RevenueShareOrder]) -> Vec<u128> {
        let n = RevenueShareEscrow::space(orders.len(), 0);
        let mut backing = vec![0u128; (n + 15) / 16];
        {
            let full: &mut [u8] = bytemuck::cast_slice_mut(&mut backing);
            let buf = &mut full[..n];
            buf[0..8].copy_from_slice(RevenueShareEscrow::DISCRIMINATOR);
            let hdr = 8 + std::mem::size_of::<RevenueShareEscrowFixed>();
            buf[hdr + 4..hdr + 8].copy_from_slice(&(orders.len() as u32).to_le_bytes());
            let osz = std::mem::size_of::<RevenueShareOrder>();
            for (i, o) in orders.iter().enumerate() {
                let s = hdr + 8 + i * osz;
                buf[s..s + osz].copy_from_slice(bytemuck::bytes_of(o));
            }
        }
        backing
    }

    /// OtterSec #82: `revoke_completed_orders` must decide whether an order is
    /// still open by its `order_id`, not the row's stored `user_order_index`.
    /// Here the builder row for order 7 carries a STALE index (5), but order 7
    /// is actually still open at a different slot (3). The old index-based check
    /// read `user.orders[5]` (an unrelated/empty slot), concluded the order was
    /// gone, and marked the still-live fee-bearing row Completed early.
    #[test]
    fn revoke_keeps_open_order_row_despite_stale_index() {
        let n = RevenueShareEscrow::space(1, 0);
        let mut backing = escrow_backing(&[open_builder_row(7, 5, 50)]);
        let full: &mut [u8] = bytemuck::cast_slice_mut(&mut backing);
        let cell = RefCell::new(&mut full[..n]);
        let data = RefMut::map(cell.borrow_mut(), |d| &mut **d);
        let (_disc, data) = RefMut::map_split(data, |d| d.split_at_mut(8));
        let (fixed, data) = RefMut::map_split(data, |d| {
            d.split_at_mut(std::mem::size_of::<RevenueShareEscrowFixed>())
        });
        let mut escrow = RevenueShareEscrowZeroCopyMut {
            fixed: RefMut::map(fixed, |b| bytemuck::from_bytes_mut(b)),
            data,
        };

        let mut orders = [Order::default(); 32];
        orders[3] = Order {
            order_id: 7,
            status: OrderStatus::Open,
            ..Order::default()
        };
        let user = User {
            sub_account_id: 0,
            orders,
            ..User::default()
        };

        escrow.revoke_completed_orders(&user).unwrap();

        let row = escrow.get_order(0).unwrap();
        assert!(
            row.is_open() && !row.is_completed(),
            "a still-open order's builder row must not be revoked via a stale index"
        );

        // Negative control: once order 7 is no longer open anywhere, the
        // fee-bearing row is correctly marked Completed.
        let user_closed = User {
            sub_account_id: 0,
            orders: [Order::default(); 32],
            ..User::default()
        };
        escrow.revoke_completed_orders(&user_closed).unwrap();
        assert!(
            escrow.get_order(0).unwrap().is_completed(),
            "a closed order's fee-bearing row should be marked Completed"
        );
    }
}

#[cfg(test)]
mod builder_order_index_tests {
    use super::*;
    use std::cell::{RefCell, RefMut};

    fn open_builder(order_id: u32, sub_account_id: u16, market_index: u16) -> RevenueShareOrder {
        RevenueShareOrder::new(
            0,
            sub_account_id,
            order_id,
            100,
            MarketType::Perp,
            market_index,
            RevenueShareOrderBitFlag::Open as u8,
            0,
        )
    }

    /// Builds a 16-byte-aligned escrow buffer (so `bytemuck::from_bytes` over the
    /// 8-aligned `RevenueShareOrder`s never trips alignment) with `orders` written
    /// via the same byte path production uses, then runs `body` against the loaded
    /// zero-copy view. Mirrors `load_zc_mut`'s split without the AccountInfo/owner
    /// plumbing.
    fn with_escrow(
        orders: &[RevenueShareOrder],
        body: impl FnOnce(&RevenueShareEscrowZeroCopyMut),
    ) {
        let n = RevenueShareEscrow::space(orders.len(), 0);
        let mut backing = vec![0u128; (n + 15) / 16];
        let full: &mut [u8] = bytemuck::cast_slice_mut(&mut backing);
        let buf = &mut full[..n];

        buf[0..8].copy_from_slice(RevenueShareEscrow::DISCRIMINATOR);
        let hdr = 8 + std::mem::size_of::<RevenueShareEscrowFixed>();
        // data layout after the fixed header: [padding0 4][orders_len 4][orders..]
        buf[hdr + 4..hdr + 8].copy_from_slice(&(orders.len() as u32).to_le_bytes());
        let osz = std::mem::size_of::<RevenueShareOrder>();
        for (i, order) in orders.iter().enumerate() {
            let start = hdr + 8 + i * osz;
            buf[start..start + osz].copy_from_slice(bytemuck::bytes_of(order));
        }

        let cell = RefCell::new(buf);
        let data = RefMut::map(cell.borrow_mut(), |d| &mut **d);
        let (_disc, data) = RefMut::map_split(data, |d| d.split_at_mut(8));
        let (fixed, data) = RefMut::map_split(data, |d| {
            d.split_at_mut(std::mem::size_of::<RevenueShareEscrowFixed>())
        });
        let escrow = RevenueShareEscrowZeroCopyMut {
            fixed: RefMut::map(fixed, |b| bytemuck::from_bytes_mut(b)),
            data,
        };
        body(&escrow);
    }

    /// OtterSec #88: the fill-time builder lookup must bind to the market being
    /// filled, not just `(sub_account_id, order_id)`. A market-A row must never be
    /// returned for a market-B fill, a `Completed` row's stale id must not match,
    /// and the market type must agree.
    #[test]
    fn find_builder_order_index_binds_market_type_and_open_state() {
        let mut completed = open_builder(9, 1, 5);
        // completion ORs the bit in, so real completed rows are Open|Completed
        completed.add_bit_flag(RevenueShareOrderBitFlag::Completed);
        let mut referral = open_builder(7, 1, 0);
        referral.bit_flags = RevenueShareOrderBitFlag::Referral as u8;
        let orders = [
            open_builder(7, 1, 0), // idx 0: market 0
            open_builder(7, 1, 3), // idx 1: SAME (sub, order_id), market 3
            completed,             // idx 2: completed, stale id 9
            referral,              // idx 3: referral row, id 7 market 0
        ];

        with_escrow(&orders, |escrow| {
            // exact (sub, order_id, market, Perp), open, non-referral → matched
            assert_eq!(
                escrow.find_builder_order_index(1, 7, 0, MarketType::Perp),
                Some(0)
            );
            // same (sub, order_id) but the fill is in a different market → the
            // market-0 row is NOT charged on a market-3 fill; only the genuine
            // market-3 row matches
            assert_eq!(
                escrow.find_builder_order_index(1, 7, 3, MarketType::Perp),
                Some(1)
            );
            // no row for this (order_id, market) pair
            assert_eq!(
                escrow.find_builder_order_index(1, 7, 9, MarketType::Perp),
                None
            );
            // wrong market type
            assert_eq!(
                escrow.find_builder_order_index(1, 7, 0, MarketType::Spot),
                None
            );
            // a Completed row whose id is stale must not match
            assert_eq!(
                escrow.find_builder_order_index(1, 9, 5, MarketType::Perp),
                None
            );
            // a referral row is never a builder row
            assert_eq!(
                escrow.find_builder_order_index(1, 7, 0, MarketType::Perp),
                Some(0)
            );

            // the legacy id-only finder still matches the first (sub, order_id)
            // row regardless of market — the exact behavior the tightened finder
            // guards the fill path against.
            assert_eq!(escrow.find_order_index(1, 7), Some(0));
        });
    }
}
