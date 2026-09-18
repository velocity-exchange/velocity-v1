use {crate::state::traits::Size, anchor_lang::prelude::*, solana_program::pubkey};

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

/// allow-verbose: derives a security bound, so the derivation stays next
/// to the number it justifies rather than being re-derived by a reader.
///
/// The most seconds a signed Lazer message's feed timestamp may lead
/// `Clock::unix_timestamp` and still be posted. The monotonic gate skips any
/// message whose timestamp is at or below the stored `publish_time`. A message
/// stamped ahead of the wall clock therefore stops every later message until
/// real time reaches that stamp. Without this bound, one bad upstream
/// timestamp freezes the feed for as long as the stamp is ahead. This bound
/// holds that freeze to 60 seconds.
///
/// The bound is wider than `PYTH_LAZER_MAX_STALENESS_SECONDS` because the two
/// guard different risks. A stale message replays a real price as fresh, so
/// that bound stays tight. A future message only delays the feed, and by at
/// most this bound. `Clock::unix_timestamp` is a stake-weighted median of
/// validator clocks and can run behind slot progression. A lag past 15 seconds
/// is within protocol bounds. At a 15-second bound every legitimate message
/// would read as future, and the feed would freeze for as long as the lag
/// lasts.
pub const PYTH_LAZER_MAX_FUTURE_SECONDS: i64 = 60;

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
