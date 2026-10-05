use {
    crate::{error::ErrorCode, validate},
    anchor_lang::prelude::*,
    anchor_spl::token::TokenAccount,
};

pub trait MintTokensCPI {
    fn mint(&self, vault_name: [u8; 32], vault_bump: u8, amount: u64) -> Result<()>;
}

pub trait BurnTokensCPI {
    fn burn(&self, vault_name: [u8; 32], vault_bump: u8, amount: u64) -> Result<()>;
}

pub trait TokenTransferCPI {
    fn token_transfer(&self, amount: u64) -> Result<()>;
}

/// Velocity `deposit` can succeed while taking less than asked (ReduceOnly caps it at the
/// borrow). Anything left in transit sits outside NAV, so revert.
pub fn validate_transit_settled(
    transit_token_account: &mut Account<TokenAccount>,
    balance_before: u64,
) -> Result<()> {
    transit_token_account.reload()?;
    validate!(
        transit_token_account.amount == balance_before,
        ErrorCode::DepositNotFullySettled,
        "velocity deposit left {} in transit, expected {}",
        transit_token_account.amount,
        balance_before
    )?;
    Ok(())
}
