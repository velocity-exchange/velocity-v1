use {
    crate::state::{FeeUpdate, VaultProtocol},
    anchor_lang::prelude::{Context, *},
    std::collections::BTreeSet,
    velocity::{
        error::VelocityResult,
        instructions::optional_accounts::{load_maps, AccountMaps},
        state::spot_market_map::get_writable_spot_market_set,
    },
};

pub trait AccountMapProvider<'a> {
    fn load_maps(
        &self,
        slot: u64,
        writable_spot_market: Option<u16>,
        has_vault_protocol: bool,
        has_fee_update: bool,
        velocity_state: &AccountInfo,
    ) -> VelocityResult<AccountMaps<'a>>;
}

/// Velocity's account maps over the accounts a vault instruction carries.
///
/// The slot clock comes from velocity's `State` rather than a fixed 400ms
/// baseline. On a faster chain that baseline shrinks every oracle staleness
/// window, and a vault would then fail on an oracle the rest of the protocol
/// accepts.
pub fn velocity_maps<'a>(
    accounts: &'a [AccountInfo<'a>],
    velocity_state: &AccountInfo,
    writable_spot_market_index: Option<u16>,
    slot: u64,
) -> VelocityResult<AccountMaps<'a>> {
    let slot_clock = velocity::state::state::State::slot_clock_from_account_info(velocity_state)
        .map_err(|error| {
            msg!("invalid velocity State account: {}", error);
            velocity::error::ErrorCode::DefaultError
        })?;

    load_maps(
        &mut accounts.iter().peekable(),
        &BTreeSet::new(),
        &writable_spot_market_index
            .map(get_writable_spot_market_set)
            .unwrap_or_default(),
        slot,
        slot_clock,
        None,
    )
}

impl<'info, T: anchor_lang::Bumps> AccountMapProvider<'info> for Context<'info, T> {
    fn load_maps(
        &self,
        slot: u64,
        writable_spot_market_index: Option<u16>,
        has_vault_protocol: bool,
        has_fee_update: bool,
        velocity_state: &AccountInfo,
    ) -> VelocityResult<AccountMaps<'info>> {
        // `VaultProtocol` rides last in `remaining_accounts` and `FeeUpdate`
        // sits before it, so neither reaches the map loaders.
        let tail = self
            .remaining_accounts
            .len()
            .saturating_sub(has_vault_protocol as usize)
            .saturating_sub(has_fee_update as usize);

        velocity_maps(
            &self.remaining_accounts[..tail],
            velocity_state,
            writable_spot_market_index,
            slot,
        )
    }
}

pub trait VaultProtocolProvider<'a> {
    fn vault_protocol(&self) -> Option<AccountLoader<'a, VaultProtocol>>;
}

/// Provides the last remaining account as a [`VaultProtocol`].
impl<'info, T: anchor_lang::Bumps> VaultProtocolProvider<'info> for Context<'info, T> {
    fn vault_protocol(&self) -> Option<AccountLoader<'info, VaultProtocol>> {
        let acct = self.remaining_accounts.last()?;
        AccountLoader::<'info, VaultProtocol>::try_from(acct).ok()
    }
}

pub trait FeeUpdateProvider<'a> {
    fn fee_update(
        &self,
        has_vp: bool,
        has_fee_update: bool,
    ) -> Option<AccountLoader<'a, FeeUpdate>>;
}

/// Provides [`FeeUpdate`] from remaining_accounts, respects whether the vault has a VaultProtocol.
impl<'info, T: anchor_lang::Bumps> FeeUpdateProvider<'info> for Context<'info, T> {
    fn fee_update(
        &self,
        has_vp: bool,
        has_fee_update: bool,
    ) -> Option<AccountLoader<'info, FeeUpdate>> {
        if !has_fee_update {
            None
        } else {
            let acct_idx = if has_vp {
                // if there is a [`VaultProtocol`], the [`FeeUpdate`] is the second to last account
                self.remaining_accounts.len() - 2
            } else {
                // otherwise [`FeeUpdate`] is the last account
                self.remaining_accounts.len() - 1
            };
            let acct = self.remaining_accounts.get(acct_idx)?;

            AccountLoader::<'info, FeeUpdate>::try_from(acct).ok()
        }
    }
}
