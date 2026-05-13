use anchor_lang::prelude::{AccountInfo, Pubkey};
use anchor_lang::{Owner, ZeroCopy};
use base64;
use bytes::BytesMut;

use crate::state::pyth_lazer_oracle::PythLazerOracle;
use pyth::pc::Price;

use crate::state::user::{Order, PerpPosition, SpotPosition};

pub fn get_positions(position: PerpPosition) -> [PerpPosition; 8] {
    let mut positions = [PerpPosition::default(); 8];
    positions[0] = position;
    positions
}

pub fn get_orders(order: Order) -> [Order; 32] {
    let mut orders = [Order::default(); 32];
    orders[0] = order;
    orders
}

#[macro_export]
macro_rules! get_orders {
    ($($order: expr),+) => {
        {
            let mut orders = [Order::default(); 32];
            let mut index = 0;
            $(
                index += 1;
                orders[index - 1] = $order;
            )+
            orders
        }
    };
}

pub fn get_spot_positions(spot_position: SpotPosition) -> [SpotPosition; 8] {
    let mut spot_positions = [SpotPosition::default(); 8];
    if spot_position.market_index == 0 {
        spot_positions[0] = spot_position;
    } else {
        spot_positions[1] = spot_position;
    }
    spot_positions
}

pub fn get_account_bytes<T: bytemuck::Pod>(account: &mut T) -> BytesMut {
    let mut bytes = BytesMut::new();
    let data = bytemuck::bytes_of_mut(account);
    bytes.extend_from_slice(data);
    bytes
}

/// Serializes `account` into a buffer where `bytes[disc_len..]` is aligned to `align_of::<T>()`,
/// satisfying `AccountLoader::try_from`'s alignment requirement on Rust ≥ 1.77.
pub fn get_anchor_account_bytes<T: ZeroCopy + Owner>(account: &mut T) -> AlignedAccountBytes {
    let disc = T::DISCRIMINATOR;
    let struct_bytes = bytemuck::bytes_of_mut(account);
    let struct_align = std::mem::align_of::<T>();
    let disc_len = disc.len();
    let data_len = disc_len + struct_bytes.len();

    let alloc_align = struct_align.max(16);
    let alloc_size = data_len + alloc_align;
    let layout = std::alloc::Layout::from_size_align(alloc_size, alloc_align).unwrap();
    let base = unsafe { std::alloc::alloc_zeroed(layout) };
    assert!(!base.is_null());

    let base_addr = base as usize;
    let remainder = (base_addr + disc_len) % struct_align;
    let offset = if remainder == 0 {
        0
    } else {
        struct_align - remainder
    };
    let data_ptr = unsafe { base.add(offset) };

    unsafe {
        std::ptr::copy_nonoverlapping(disc.as_ptr(), data_ptr, disc_len);
        std::ptr::copy_nonoverlapping(
            struct_bytes.as_ptr(),
            data_ptr.add(disc_len),
            struct_bytes.len(),
        );
    }

    AlignedAccountBytes {
        base,
        data_ptr,
        data_len,
        layout,
    }
}

pub struct AlignedAccountBytes {
    base: *mut u8,
    data_ptr: *mut u8,
    data_len: usize,
    layout: std::alloc::Layout,
}

impl std::ops::Deref for AlignedAccountBytes {
    type Target = [u8];
    fn deref(&self) -> &[u8] {
        unsafe { std::slice::from_raw_parts(self.data_ptr, self.data_len) }
    }
}

impl std::ops::DerefMut for AlignedAccountBytes {
    fn deref_mut(&mut self) -> &mut [u8] {
        unsafe { std::slice::from_raw_parts_mut(self.data_ptr, self.data_len) }
    }
}

impl Drop for AlignedAccountBytes {
    fn drop(&mut self) {
        unsafe { std::alloc::dealloc(self.base, self.layout) }
    }
}

/// Decodes a base64 Anchor account blob into an aligned buffer ready for `AccountLoader::try_from`.
///
/// # Safety
/// Decoded bytes must be a valid `T` preceded by an 8-byte discriminator.
pub unsafe fn aligned_account_bytes_from_b64<T: ZeroCopy + Owner>(
    b64: &str,
) -> AlignedAccountBytes {
    let decoded = base64::decode(b64).unwrap();
    let disc_len = 8;
    assert!(
        decoded.len() >= disc_len + std::mem::size_of::<T>(),
        "decoded b64 blob too short for {}",
        std::any::type_name::<T>()
    );
    let mut account: T = std::ptr::read_unaligned(decoded[disc_len..].as_ptr() as *const T);
    get_anchor_account_bytes(&mut account)
}

pub fn create_account_info<'a>(
    key: &'a Pubkey,
    is_writable: bool,
    lamports: &'a mut u64,
    bytes: &'a mut [u8],
    owner: &'a Pubkey,
) -> AccountInfo<'a> {
    AccountInfo::new(key, false, is_writable, lamports, bytes, owner, false)
}

/// Variant of [`get_anchor_account_bytes`] for the dynamic-tail [`User`] account:
/// produces `[discriminator][User header][orders...]` in an aligned buffer suitable
/// for `AccountLoader::try_from` followed by `load_user!` / `load_user_mut!`.
pub fn get_anchor_user_account_bytes(
    user: &mut crate::state::user::User,
    orders: &[Order],
) -> AlignedAccountBytes {
    use anchor_lang::Discriminator;
    user.orders_len = orders.len() as u32;
    let disc = <crate::state::user::User as Discriminator>::DISCRIMINATOR;
    let user_bytes = bytemuck::bytes_of_mut(user);
    let user_align = std::mem::align_of::<crate::state::user::User>();
    let order_size = std::mem::size_of::<Order>();
    let disc_len = disc.len();
    let tail_len = orders.len() * order_size;
    let data_len = disc_len + user_bytes.len() + tail_len;

    let alloc_align = user_align.max(16);
    let alloc_size = data_len + alloc_align;
    let layout = std::alloc::Layout::from_size_align(alloc_size, alloc_align).unwrap();
    let base = unsafe { std::alloc::alloc_zeroed(layout) };
    assert!(!base.is_null());

    let base_addr = base as usize;
    let remainder = (base_addr + disc_len) % user_align;
    let offset = if remainder == 0 {
        0
    } else {
        user_align - remainder
    };
    let data_ptr = unsafe { base.add(offset) };

    unsafe {
        std::ptr::copy_nonoverlapping(disc.as_ptr(), data_ptr, disc_len);
        std::ptr::copy_nonoverlapping(
            user_bytes.as_ptr(),
            data_ptr.add(disc_len),
            user_bytes.len(),
        );
        let tail_ptr = data_ptr.add(disc_len + user_bytes.len());
        for (i, o) in orders.iter().enumerate() {
            let o_bytes = bytemuck::bytes_of(o);
            std::ptr::copy_nonoverlapping(
                o_bytes.as_ptr(),
                tail_ptr.add(i * order_size),
                order_size,
            );
        }
    }

    AlignedAccountBytes {
        base,
        data_ptr,
        data_len,
        layout,
    }
}

/// `create_anchor_account_info!` analog for the dynamic-tail [`User`] account.
/// Takes a User value and an orders slice expression, and binds an `AccountInfo`
/// over a `[discriminator][User header][orders...]` aligned buffer.
#[macro_export]
macro_rules! create_anchor_user_account_info {
    ($user:expr, $orders:expr, $name:ident) => {
        let __key = anchor_lang::prelude::Pubkey::default();
        let mut __lamports = 0;
        let mut __user_val = $user;
        let __orders_val = $orders;
        let mut __data =
            $crate::test_utils::get_anchor_user_account_bytes(&mut __user_val, &__orders_val[..]);
        let __owner = <$crate::state::user::User as anchor_lang::Owner>::owner();
        let $name = $crate::test_utils::create_account_info(
            &__key,
            true,
            &mut __lamports,
            &mut __data[..],
            &__owner,
        );
    };
    ($user:expr, $orders:expr, $pubkey:expr, $name:ident) => {
        let mut __lamports = 0;
        let mut __user_val = $user;
        let __orders_val = $orders;
        let mut __data =
            $crate::test_utils::get_anchor_user_account_bytes(&mut __user_val, &__orders_val[..]);
        let __owner = <$crate::state::user::User as anchor_lang::Owner>::owner();
        let $name = $crate::test_utils::create_account_info(
            $pubkey,
            true,
            &mut __lamports,
            &mut __data[..],
            &__owner,
        );
    };
}

pub fn get_pyth_price(price: i64, expo: i32) -> PythLazerOracle {
    let mut pyth_price = PythLazerOracle::default();
    let price = price * 10_i64.pow(expo as u32);
    pyth_price.price = price;
    pyth_price.publish_time = 0;
    pyth_price.posted_slot = 0;
    pyth_price.exponent = expo;
    pyth_price
}

/// Creates a PythLazerOracle with price as raw mantissa (no multiply). Use when the price is already
/// in the correct form for the given exponent (e.g. 990000 with expo 6 for $0.99).
pub fn get_pyth_price_mantissa(price: i64, expo: i32) -> PythLazerOracle {
    let mut pyth_price = PythLazerOracle::default();
    pyth_price.price = price;
    pyth_price.publish_time = 0;
    pyth_price.posted_slot = 0;
    pyth_price.exponent = expo;
    pyth_price
}

pub fn get_hardcoded_pyth_price(price: i64, expo: i32) -> Price {
    let mut pyth_price = Price::default();
    pyth_price.agg.price = price;
    pyth_price.twap = price;
    pyth_price.expo = expo;
    pyth_price
}

#[macro_export]
macro_rules! create_anchor_account_info {
    ($account:expr, $type:ident, $name: ident) => {
        let key = anchor_lang::prelude::Pubkey::default();
        let mut lamports = 0;
        let mut data = $crate::test_utils::get_anchor_account_bytes(&mut $account);
        let owner = <$type as anchor_lang::Owner>::owner();
        let $name = $crate::test_utils::create_account_info(
            &key,
            true,
            &mut lamports,
            &mut data[..],
            &owner,
        );
    };
    ($account:expr, $pubkey:expr, $type:ident, $name: ident) => {
        let mut lamports = 0;
        let mut data = $crate::test_utils::get_anchor_account_bytes(&mut $account);
        let owner = <$type as anchor_lang::Owner>::owner();
        let $name = $crate::test_utils::create_account_info(
            $pubkey,
            true,
            &mut lamports,
            &mut data[..],
            &owner,
        );
    };
}

#[macro_export]
macro_rules! create_account_info {
    ($account:expr, $owner:expr, $name: ident) => {
        let key = anchor_lang::prelude::Pubkey::default();
        let mut lamports = 0;
        let mut data = $crate::test_utils::get_account_bytes(&mut $account);
        let $name = $crate::test_utils::create_account_info(
            &key,
            true,
            &mut lamports,
            &mut data[..],
            $owner,
        );
    };
    ($account:expr, $pubkey:expr, $owner:expr, $name: ident) => {
        let mut lamports = 0;
        let mut data = $crate::test_utils::get_account_bytes(&mut $account);
        let $name = $crate::test_utils::create_account_info(
            $pubkey,
            true,
            &mut lamports,
            &mut data[..],
            $owner,
        );
    };
}
