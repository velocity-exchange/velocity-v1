use anchor_lang::Discriminator;
use solana_rpc_client_api::filter::{Memcmp, RpcFilterType};

use crate::types::{
    accounts::{PerpMarket, SpotMarket, User, UserStats},
    MarketType,
};

pub fn get_user_filter() -> RpcFilterType {
    RpcFilterType::Memcmp(Memcmp::new_raw_bytes(0, User::DISCRIMINATOR.to_vec()))
}

// Byte offsets of the trailing scalar flags in the `User` account. These MUST match the on-chain
// `User` layout (see `programs/velocity/src/state/user.rs` and the mirror in
// `packages/sdk/src/memcmp.ts`). The current Velocity layout has the tail block laid out as
// consecutive single bytes:
//   status(4468) is_margin_trading_enabled(4469) idle(4470) open_orders(4471)
//   has_open_order(4472) open_auctions(4473) has_open_auction(4474) pool_id(4475)
//   special_user_status(4476)
// These were previously the stale upstream-drift offsets (idle@4350, has_open_order@4352,
// has_open_auction@4354); after Velocity added fields to `PerpPosition` the account grew by 120
// bytes and every tail flag shifted +120, so the drift offsets matched zero accounts.
pub fn get_non_idle_user_filter() -> RpcFilterType {
    RpcFilterType::Memcmp(Memcmp::new_raw_bytes(4_470, vec![0]))
}

pub fn get_user_with_order_filter() -> RpcFilterType {
    RpcFilterType::Memcmp(Memcmp::new_raw_bytes(4_472, vec![1]))
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
