//! Error codes returned by the router program.

use anchor_lang::prelude::*;

#[error_code]
pub enum RouterError {
    #[msg("Signer is not authorized for this instruction")]
    Unauthorized,
    #[msg("Authority key cannot be the default pubkey")]
    InvalidAuthority,
    #[msg("At least one tier is required")]
    EmptyTiers,
    #[msg("Too many tiers")]
    TooManyTiers,
    #[msg("The first tier must start at threshold 0")]
    FirstTierThresholdNotZero,
    #[msg("Tier thresholds must strictly increase")]
    TiersNotIncreasing,
    #[msg("Tier pool_bps must be at most 10000")]
    InvalidBps,
    #[msg("Tiers cannot change after a distribution in the current period")]
    TiersLockedForPeriod,
    #[msg("Arithmetic overflow")]
    ArithmeticOverflow,
    #[msg("Treasury cannot be the default key, the router config or the redemption config")]
    InvalidTreasury,
    #[msg("USDT mint does not match the redemption config's mint")]
    UsdtMintMismatch,
}
