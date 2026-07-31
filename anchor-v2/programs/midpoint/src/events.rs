//! Execute is the only evented path — mid/level writes are the hot loop and
//! deliberately emit nothing (log serialization is compute the maker pays
//! for thousands of times a day; fills are rare and worth a record).

use anchor_lang_v2::prelude::*;

#[event(bytemuck)]
#[repr(C)]
pub struct MidpointExecuteRecord {
    /// The quoted wallet.
    pub authority: Address,
    pub ts: i64,
    pub slot: u64,
    pub mid_price: u64,
    pub base_size: u64,
    pub quote_size: u64,
    pub market_index: u16,
    pub sub_account_id: u16,
    /// 0 = Long (taker bought the ask side), 1 = Short.
    pub direction: u8,
    pub _pad: [u8; 3],
}
