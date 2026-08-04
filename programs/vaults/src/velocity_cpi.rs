use {
    anchor_lang::prelude::*,
    velocity::cpi::accounts::UpdateSpotMarketCumulativeInterest as VelocityUpdateSpotMarketCumulativeInterest,
};

/// Refresh the velocity spot market that denominates the vault's NAV (`Vault::spot_market_index`),
/// before any [`crate::Vault::calculate_equity`] snapshot.
///
/// `calculate_equity` prices the vault's velocity deposit off the market's STORED
/// `cumulative_deposit_interest`. Solana lets only the owning program mutate an account, so the vaults
/// program cannot advance that index. It must CPI velocity.
///
/// OtterSec #136: `deposit` refreshed the market only afterwards, as a side effect of the deposit CPI.
/// An entrant therefore minted shares against an index that had not yet absorbed lender interest the
/// incumbents had earned, and captured part of it.
///
/// OtterSec #137: the withdraw-request and cancel paths never refreshed. Request understated the frozen
/// request value, and cancel let request-window interest escape the share-forfeiture rule.
///
/// Call this FIRST in any handler that snapshots NAV. It must run before `load`/`load_mut` and before
/// `load_maps`: `invoke` rejects a CPI whose writable accounts still have live borrows, and the maps
/// must read post-refresh data.
///
/// The refresh is idempotent within a slot. Velocity's `update_spot_market_cumulative_interest` does
/// nothing once `last_interest_ts == now`, so a handler that later CPIs `deposit` or `withdraw` pays
/// nothing extra.
pub fn refresh_denomination_spot_market<'info>(
    velocity_program: &AccountInfo<'info>,
    state: &AccountInfo<'info>,
    spot_market: &AccountInfo<'info>,
    oracle: &AccountInfo<'info>,
    spot_market_vault: &AccountInfo<'info>,
) -> Result<()> {
    let cpi_accounts = VelocityUpdateSpotMarketCumulativeInterest {
        state: state.clone(),
        spot_market: spot_market.clone(),
        oracle: oracle.clone(),
        spot_market_vault: spot_market_vault.clone(),
    };
    let cpi_context = CpiContext::new(velocity_program.key(), cpi_accounts);
    velocity::cpi::update_spot_market_cumulative_interest(cpi_context)?;

    Ok(())
}

pub trait InitializeUserCPI {
    fn velocity_initialize_user(&self, name: [u8; 32], bump: u8) -> Result<()>;

    fn velocity_initialize_user_stats(&self, name: [u8; 32], bump: u8) -> Result<()>;
}

pub trait SetUserVaultOwnedCPI {
    /// Flag the vault-owned velocity User so the revenue-share sweep never
    /// credits it (OtterSec #91/#92/#93). Called once at vault init.
    fn velocity_set_user_vault_owned(&self, name: [u8; 32], bump: u8) -> Result<()>;
}

pub trait DepositCPI {
    fn velocity_deposit(&self, amount: u64) -> Result<()>;
}

pub trait ManagerRepayCPI {
    fn velocity_deposit(&self, market_index: u16, amount: u64) -> Result<()>;
}

pub trait WithdrawCPI {
    fn velocity_withdraw(&self, amount: u64) -> Result<()>;
}

pub trait ManagerBorrowCPI {
    fn velocity_withdraw(&self, market_index: u16, amount: u64) -> Result<()>;
}

pub trait UpdateUserDelegateCPI {
    fn velocity_update_user_delegate(&self, delegate: Pubkey) -> Result<()>;
}

pub trait UpdateUserReduceOnlyCPI {
    fn velocity_update_user_reduce_only(&self, reduce_only: bool) -> Result<()>;
}

pub trait UpdateUserMarginTradingEnabledCPI {
    fn velocity_update_user_margin_trading_enabled(&self, enabled: bool) -> Result<()>;
}

pub trait UpdatePoolIdCPI {
    fn velocity_update_pool_id(&self, pool_id: u8) -> Result<()>;
}

pub trait InitializeInsuranceFundStakeCPI {
    fn velocity_initialize_insurance_fund_stake(&self, market_index: u16) -> Result<()>;
}

pub trait AddInsuranceFundStakeCPI {
    fn velocity_add_insurance_fund_stake(&self, market_index: u16, amount: u64) -> Result<()>;
}

pub trait RequestRemoveInsuranceFundStakeCPI {
    fn velocity_request_remove_insurance_fund_stake(
        &self,
        market_index: u16,
        amount: u64,
    ) -> Result<()>;
}

pub trait CancelRequestRemoveInsuranceFundStakeCPI {
    fn velocity_cancel_request_remove_insurance_fund_stake(&self, market_index: u16) -> Result<()>;
}
pub trait RemoveInsuranceFundStakeCPI {
    fn velocity_remove_insurance_fund_stake(&self, market_index: u16) -> Result<()>;
}
