use anchor_lang::Discriminator;
use solana_rpc_client_api::filter::{Memcmp, RpcFilterType};

use crate::types::{
    accounts::{PerpMarket, SpotMarket, User, UserStats},
    MarketType,
};

pub fn get_user_filter() -> RpcFilterType {
    RpcFilterType::Memcmp(Memcmp::new_raw_bytes(0, User::DISCRIMINATOR.to_vec()))
}

// Account offsets of the `User` tail flags: idle(4534), has_open_order(4535). They must match
// `programs/velocity/src/state/user.rs`, which `programs/velocity/tests/user_layout.rs` pins, and
// the mirror in `packages/sdk/src/memcmp.ts`.
pub fn get_non_idle_user_filter() -> RpcFilterType {
    RpcFilterType::Memcmp(Memcmp::new_raw_bytes(4_534, vec![0]))
}

pub fn get_user_with_order_filter() -> RpcFilterType {
    RpcFilterType::Memcmp(Memcmp::new_raw_bytes(4_535, vec![1]))
}

pub fn get_user_stats_filter() -> RpcFilterType {
    RpcFilterType::Memcmp(Memcmp::new_raw_bytes(0, UserStats::DISCRIMINATOR.to_vec()))
}

pub fn get_user_stats_is_referred_filter() -> RpcFilterType {
    RpcFilterType::Memcmp(Memcmp::new_raw_bytes(164, vec![2]))
}

pub fn get_user_stats_is_referred_or_referrer_filter() -> RpcFilterType {
    RpcFilterType::Memcmp(Memcmp::new_raw_bytes(164, vec![3]))
}

pub fn get_market_filter(market_type: MarketType) -> RpcFilterType {
    match market_type {
        MarketType::Spot => {
            RpcFilterType::Memcmp(Memcmp::new_raw_bytes(0, SpotMarket::DISCRIMINATOR.to_vec()))
        }
        MarketType::Perp => {
            RpcFilterType::Memcmp(Memcmp::new_raw_bytes(0, PerpMarket::DISCRIMINATOR.to_vec()))
        }
    }
}
