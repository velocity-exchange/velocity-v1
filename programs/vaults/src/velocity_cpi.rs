use {
    crate::Vault,
    anchor_lang::prelude::*,
    std::collections::BTreeSet,
    velocity::{
        cpi::accounts::RefreshSpotMarketInterest as VelocityRefreshSpotMarketInterest,
        instructions::optional_accounts::{load_maps, AccountMaps},
        state::user::User,
    },
};

/// The spot markets whose lending-interest index prices [`crate::Vault::calculate_equity`].
///
/// Equity comes from velocity's `calculate_user_equity`, which reads a cumulative index in two
/// places. It converts every held spot position through that position's own market index, and it
/// converts an isolated perp position's collateral through the perp market's quote spot market.
/// One market is therefore not enough: a vault that also lends or borrows elsewhere prices those
/// positions off whatever index the last unrelated crank left behind. A stale deposit index reads
/// the asset low, and a stale borrow index reads the liability low, which reads NAV high and
/// overpays a withdrawer.
///
/// The denomination market is always included. It holds the vault's own deposit, and it divides
/// the final equity.
///
/// The quote market of an isolated perp position is named by the perp market, not by the position,
/// so finding it needs the perp market accounts. They arrive in `remaining_accounts` already,
/// because equity reads those markets too. That walk runs only when the user holds an isolated
/// position, so an ordinary vault pays nothing for it.
pub fn spot_markets_that_price_equity<'info>(
    vault: &Vault,
    user: &User,
    velocity_state: &AccountInfo<'info>,
    remaining_accounts: &'info [AccountInfo<'info>],
    slot: u64,
) -> Result<Vec<u16>> {
    let mut market_indexes: Vec<u16> = user
        .spot_positions
        .iter()
        .filter(|position| !position.is_available())
        .map(|position| position.market_index)
        .collect();

    if !market_indexes.contains(&vault.spot_market_index) {
        market_indexes.push(vault.spot_market_index);
    }

    let isolated_perp_market_indexes: Vec<u16> = user
        .perp_positions
        .iter()
        .filter(|position| !position.is_available() && position.is_isolated())
        .map(|position| position.market_index)
        .collect();

    if !isolated_perp_market_indexes.is_empty() {
        let slot_duration =
            velocity::state::state::State::slot_duration_from_account_info(velocity_state, slot)
                .map_err(|_| velocity::error::ErrorCode::DefaultError)?;
        let AccountMaps {
            perp_market_map, ..
        } = load_maps(
            &mut remaining_accounts.iter().peekable(),
            &BTreeSet::new(),
            &BTreeSet::new(),
            slot,
            slot_duration,
            None,
        )?;

        for perp_market_index in isolated_perp_market_indexes {
            let quote_spot_market_index = perp_market_map
                .get_ref(&perp_market_index)?
                .quote_spot_market_index;
            if !market_indexes.contains(&quote_spot_market_index) {
                market_indexes.push(quote_spot_market_index);
            }
        }
    }

    Ok(market_indexes)
}

/// Book the lending interest of every market that prices the vault's NAV, before the handler
/// snapshots it.
///
/// `calculate_equity` prices a position off its market's STORED cumulative index. Solana lets only
/// the owning program mutate an account, so the vaults program cannot advance those indexes. It
/// must CPI velocity.
///
/// OtterSec #136: `deposit` refreshed the market only afterwards, as a side effect of the deposit
/// CPI. An entrant therefore minted shares against an index that had not yet absorbed lender
/// interest the incumbents had earned, and captured part of it.
///
/// OtterSec #137: the withdraw-request and cancel paths never refreshed. Request understated the
/// frozen request value, and cancel let request-window interest escape the share-forfeiture rule.
///
/// Call this FIRST in any handler that snapshots NAV. It must run before `load_maps`, which has to
/// read post-refresh data, and before any `load_mut` of an account this call forwards.
///
/// The markets travel as writable accounts inside `remaining_accounts`, the same slice the handler
/// later hands to `load_maps`. Velocity keys each account by the market index stored in the account
/// itself, so it refreshes market `i` only when market `i`'s own account is passed.
///
/// The refresh is idempotent within a slot. Velocity's accrual does nothing once
/// `last_interest_ts` has reached the current time, so a handler that later CPIs `deposit` or
/// `withdraw` pays nothing extra.
///
/// Velocity books the interest but does not always advance the index. A carveout too small to
/// convert to one token defers the whole interval, leaving the index where it was and the call
/// still successful. The unbooked amount is under one token of the carveout divided by its factor,
/// and it cannot build up, because the deferred interval keeps growing until it clears.
pub fn refresh_spot_markets_that_price_equity<'info>(
    velocity_program: &AccountInfo<'info>,
    state: &AccountInfo<'info>,
    remaining_accounts: &[AccountInfo<'info>],
    market_indexes: Vec<u16>,
) -> Result<()> {
    let cpi_accounts = VelocityRefreshSpotMarketInterest {
        state: state.clone(),
    };
    let cpi_context = CpiContext::new(velocity_program.key(), cpi_accounts)
        .with_remaining_accounts(remaining_accounts.to_vec());
    velocity::cpi::refresh_spot_market_interest(cpi_context, market_indexes)?;

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
