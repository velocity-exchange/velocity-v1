use crate::state::traits::Size;
use anchor_lang::prelude::*;
use solana_program::pubkey;

pub const PYTH_LAZER_ORACLE_SEED: &[u8] = b"pyth_lazer";
pub const PYTH_LAZER_STORAGE_ID: Pubkey = pubkey!("3rdJbqfnagQ4yx9HXJViD4zc4xpiSqmFsKpPuSCQVyQL");

/// Max age (seconds) a signed Lazer message's feed timestamp may lag `Clock::unix_timestamp`
/// and still be posted. `post_pyth_lazer_oracle_update` stamps `posted_slot` to the current
/// slot and downstream staleness is derived solely from that slot, so without this bound an
/// authentic-but-stale (or replayed) message would be treated as slot-fresh for AMM, margin,
/// liquidation, and settlement. Kept close to the AMM staleness window
/// (`slots_before_stale_for_amm` ≈ 10 slots ≈ 5s) with headroom for normal posting latency:
/// legitimate Lazer updates are sub-second and posted within a few slots, so 15s never rejects
/// a fresh post but tightly caps how long a replayed message can keep the price pegged as fresh.
pub const PYTH_LAZER_MAX_STALENESS_SECONDS: i64 = 15;

impl Size for PythLazerOracle {
    const SIZE: usize = 48;
}

#[account(zero_copy(unsafe))]
#[derive(Default, Eq, PartialEq, Debug)]
#[repr(C)]
pub struct PythLazerOracle {
    pub price: i64,
    pub publish_time: u64,
    pub posted_slot: u64,
    pub exponent: i32,
    pub _padding: [u8; 4],
    pub conf: u64,
}
