//! Events emitted by the router, consumed by the off-chain indexer.

use anchor_lang::prelude::*;

#[event]
pub struct RouterInitialized {
    pub ts: i64,
    pub admin: Pubkey,
    pub cranker: Pubkey,
    pub treasury: Pubkey,
    pub usdt_mint: Pubkey,
    pub tier_count: u8,
}

#[event]
pub struct RouterConfigUpdated {
    pub ts: i64,
    pub admin: Pubkey,
    pub cranker: Pubkey,
    pub treasury: Pubkey,
    pub tier_count: u8,
}

#[event]
pub struct FeesDistributed {
    pub ts: i64,
    pub total: u64,
    pub to_pool: u64,
    pub to_treasury: u64,
    pub cap_room_after: u64,
    pub period_day: i64,
    pub period_fees_after: u128,
    pub lifetime_fees_after: u128,
}
