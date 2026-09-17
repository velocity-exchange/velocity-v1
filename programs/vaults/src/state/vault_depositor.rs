use {
    crate::{
        error::ErrorCode,
        events::VaultDepositorAction,
        state::{
            events::{VaultDepositorRecord, VaultDepositorV1Record},
            withdraw_request::WithdrawRequest,
            withdraw_unit::WithdrawUnit,
            FeeUpdate, Vault, VaultDepositorBase, VaultFee, VaultProtocol,
        },
        validate, Size,
    },
    anchor_lang::prelude::*,
    static_assertions::const_assert_eq,
    std::cell::RefMut,
    velocity::{
        controller::spot_balance::update_spot_balances,
        error::ErrorCode as VelocityErrorCode,
        math::{
            casting::Cast,
            insurance::{
                if_shares_to_vault_amount as depositor_shares_to_vault_amount,
                vault_amount_to_if_shares as vault_amount_to_depositor_shares,
            },
            margin::{meets_initial_margin_requirement, validate_spot_margin_trading},
            safe_math::SafeMath,
        },
        state::{spot_market::SpotBalanceType, user::User},
    },
    velocity_macros::assert_no_slop,
};

#[assert_no_slop]
#[account(zero_copy(unsafe))]
#[derive(Default, Eq, PartialEq, Debug)]
#[repr(C)]
pub struct VaultDepositor {
    /// The vault deposited into
    pub vault: Pubkey,
    /// The vault depositor account's pubkey. It is a pda of vault and authority
    pub pubkey: Pubkey,
    /// The authority is the address w permission to deposit/withdraw
    pub authority: Pubkey,
    /// share of vault owned by this depositor. vault_shares / vault.total_shares is depositor's ownership of vault_equity
    vault_shares: u128,
    /// last withdraw request
    pub last_withdraw_request: WithdrawRequest,
    /// creation ts of vault depositor
    pub last_valid_ts: i64,
    /// lifetime net deposits of vault depositor for the vault
    pub net_deposits: i64,
    /// lifetime total deposits
    pub total_deposits: u64,
    /// lifetime total withdraws
    pub total_withdraws: u64,
    /// the token amount of gain, net of the profit share taken on it, that the high-water mark
    /// already covers. `net_deposits + cumulative_profit_share_amount` is the high-water mark.
    pub cumulative_profit_share_amount: i64,
    pub profit_share_fee_paid: u64,
    /// the exponent for vault_shares decimal places
    pub vault_shares_base: u32,
    /// the vault's profit share when the high-water mark was last set. Gain above the high-water
    /// mark is priced at this rate, so a later raise never prices gain earned before it.
    pub profit_share_at_basis: u32,
    /// the vault's hurdle rate when the high-water mark was last set. Gain above the high-water
    /// mark keeps this shelter, so a later cut never exposes gain earned before it.
    pub hurdle_rate_at_basis: u32,
    pub padding_align: u32,
    pub padding: [u64; 4],
}

impl Size for VaultDepositor {
    const SIZE: usize = 240 + 8;
}

const_assert_eq!(
    VaultDepositor::SIZE,
    std::mem::size_of::<VaultDepositor>() + 8
);

impl VaultDepositorBase for VaultDepositor {
    fn get_authority(&self) -> Pubkey {
        self.authority
    }
    fn get_pubkey(&self) -> Pubkey {
        self.pubkey
    }

    fn get_vault_shares(&self) -> u128 {
        self.vault_shares
    }
    fn set_vault_shares(&mut self, shares: u128) {
        self.vault_shares = shares;
    }

    fn get_vault_shares_base(&self) -> u32 {
        self.vault_shares_base
    }
    fn set_vault_shares_base(&mut self, base: u32) {
        self.vault_shares_base = base;
    }

    fn get_net_deposits(&self) -> i64 {
        self.net_deposits
    }
    fn set_net_deposits(&mut self, amount: i64) {
        self.net_deposits = amount;
    }

    fn get_cumulative_profit_share_amount(&self) -> i64 {
        self.cumulative_profit_share_amount
    }
    fn set_cumulative_profit_share_amount(&mut self, amount: i64) {
        self.cumulative_profit_share_amount = amount;
    }

    fn get_profit_share_fee_paid(&self) -> u64 {
        self.profit_share_fee_paid
    }
    fn set_profit_share_fee_paid(&mut self, amount: u64) {
        self.profit_share_fee_paid = amount;
    }

    fn get_profit_share_at_basis(&self) -> u32 {
        self.profit_share_at_basis
    }
    fn set_profit_share_at_basis(&mut self, profit_share: u32) {
        self.profit_share_at_basis = profit_share;
    }

    fn get_hurdle_rate_at_basis(&self) -> u32 {
        self.hurdle_rate_at_basis
    }
    fn set_hurdle_rate_at_basis(&mut self, hurdle_rate: u32) {
        self.hurdle_rate_at_basis = hurdle_rate;
    }
}

impl VaultDepositor {
    pub fn new(vault: &Vault, pubkey: Pubkey, authority: Pubkey, now: i64) -> Self {
        VaultDepositor {
            vault: vault.pubkey,
            pubkey,
            authority,
            vault_shares: 0,
            vault_shares_base: 0,
            last_withdraw_request: WithdrawRequest::default(),
            last_valid_ts: now,
            net_deposits: 0,
            total_deposits: 0,
            total_withdraws: 0,
            cumulative_profit_share_amount: 0,
            profit_share_fee_paid: 0,
            profit_share_at_basis: vault.profit_share,
            hurdle_rate_at_basis: vault.hurdle_rate,
            padding_align: 0,
            padding: [0u64; 4],
        }
    }

    pub fn validate_base(&self, vault: &Vault) -> Result<()> {
        validate!(
            self.vault_shares_base == vault.shares_base,
            ErrorCode::InvalidVaultRebase,
            "vault depositor bases mismatch. user base: {} vault base {}",
            self.vault_shares_base,
            vault.shares_base
        )?;

        Ok(())
    }

    pub fn checked_vault_shares(&self, vault: &Vault) -> Result<u128> {
        self.validate_base(vault)?;
        Ok(self.vault_shares)
    }

    pub fn unchecked_vault_shares(&self) -> u128 {
        self.vault_shares
    }

    pub fn increase_vault_shares(&mut self, delta: u128, vault: &Vault) -> Result<()> {
        self.validate_base(vault)?;
        self.vault_shares = self.vault_shares.safe_add(delta)?;
        Ok(())
    }

    pub fn decrease_vault_shares(&mut self, delta: u128, vault: &Vault) -> Result<()> {
        self.validate_base(vault)?;
        self.vault_shares = self.vault_shares.safe_sub(delta)?;
        Ok(())
    }

    pub fn update_vault_shares(&mut self, new_shares: u128, vault: &Vault) -> Result<()> {
        self.validate_base(vault)?;
        self.vault_shares = new_shares;

        Ok(())
    }

    pub fn apply_rebase(
        &mut self,
        vault: &mut Vault,
        vault_protocol: &mut Option<RefMut<VaultProtocol>>,
        vault_equity: u64,
    ) -> Result<Option<u128>> {
        if let Some(rebase_divisor) =
            VaultDepositorBase::apply_rebase(self, vault, vault_protocol, vault_equity)?
        {
            self.last_withdraw_request.rebase(rebase_divisor)?;
            Ok(Some(rebase_divisor))
        } else {
            Ok(None)
        }
    }

    /// Permissionless lazy rebase used by the signerless `apply_rebase` instruction.
    ///
    /// The base rebase floors `vault_shares`, and a pending request's shares, by integer
    /// division. A third party could commit the lazy rebase on a small depositor whose shares or
    /// pending-request shares floor to zero while the request's `value` stays above zero. That
    /// freezes the position, because withdraw needs `n_shares > 0`, and cancel clears the value
    /// without restoring the shares. So the public rebase is rejected when it would floor an
    /// active depositor's shares or a pending request's shares to zero (OtterSec #106). The
    /// depositor can still
    /// rebase through its own signed lifecycle actions.
    pub fn apply_rebase_public(
        &mut self,
        vault: &mut Vault,
        vault_protocol: &mut Option<RefMut<VaultProtocol>>,
        vault_equity: u64,
    ) -> Result<Option<u128>> {
        let vault_shares_before = self.unchecked_vault_shares();
        let request_shares_before = self.last_withdraw_request.shares;
        let request_value_before = self.last_withdraw_request.value;

        let rebase_divisor = self.apply_rebase(vault, vault_protocol, vault_equity)?;

        validate!(
            !(vault_shares_before > 0 && self.unchecked_vault_shares() == 0),
            ErrorCode::InvalidVaultRebase,
            "public rebase would floor depositor shares to zero; depositor must rebase via a signed action"
        )?;
        validate!(
            !(request_value_before > 0
                && request_shares_before > 0
                && self.last_withdraw_request.shares == 0),
            ErrorCode::InvalidVaultRebase,
            "public rebase would floor a pending withdraw request's shares to zero while value remains"
        )?;

        Ok(rebase_divisor)
    }

    #[allow(clippy::too_many_arguments)]
    pub fn deposit(
        &mut self,
        amount: u64,
        vault_equity: u64,
        vault: &mut Vault,
        vault_protocol: &mut Option<RefMut<VaultProtocol>>,
        fee_update: &mut Option<AccountLoader<FeeUpdate>>,
        now: i64,
        deposit_oracle_price: i64,
    ) -> Result<()> {
        validate!(
            vault.max_tokens == 0 || vault.max_tokens >= vault_equity.safe_add(amount)?,
            ErrorCode::VaultIsAtCapacity,
            "after deposit vault equity is {} > {}",
            vault_equity.safe_add(amount)?,
            vault.max_tokens
        )?;

        validate!(
            vault.min_deposit_amount == 0 || amount >= vault.min_deposit_amount,
            ErrorCode::InvalidVaultDeposit,
            "deposit amount {} is below vault min_deposit_amount {}",
            amount,
            vault.min_deposit_amount
        )?;

        validate!(
            !(vault_equity == 0 && vault.total_shares != 0),
            ErrorCode::InvalidVaultForNewDepositors,
            "Vault balance should be non-zero for new depositors to enter"
        )?;

        validate!(
            !self.last_withdraw_request.pending(),
            ErrorCode::WithdrawInProgress,
            "withdraw request is in progress"
        )?;

        self.apply_rebase(vault, vault_protocol, vault_equity)?;

        let vault_shares_before = self.checked_vault_shares(vault)?;
        let total_vault_shares_before = vault.total_shares;
        let user_vault_shares_before = vault.user_shares;
        let protocol_shares_before = vault.get_protocol_shares(vault_protocol);

        let VaultFee {
            management_fee_payment,
            management_fee_shares,
            protocol_fee_payment,
            protocol_fee_shares,
        } = vault.apply_fee(vault_protocol, fee_update, vault_equity, now)?;
        // apply_fee can mint fee shares that push total_shares above equity and cause a second
        // vault rebase. That rebase raises vault.shares_base and does not re-sync this depositor.
        // Run the depositor rebase again, so the base-checked calls below, apply_profit_share
        // and increase_vault_shares, do not abort with InvalidVaultRebase (OtterSec #107).
        self.apply_rebase(vault, vault_protocol, vault_equity)?;
        let (manager_profit_share, protocol_profit_share) =
            self.apply_profit_share(vault_equity, vault, vault_protocol, now)?;

        let n_shares = vault_amount_to_depositor_shares(amount, vault.total_shares, vault_equity)?;

        // Reject a positive deposit that mints zero shares. A deposit that is small next to the
        // per-share NAV floors to zero shares, and the depositor's tokens then stay in the vault
        // as price appreciation on the existing shares. `request_withdraw` already requires
        // `n_shares > 0`. The deposit side applies the same rule, which matches the
        // `IFDepositMintsZeroShares` guard on the insurance-fund add path (OtterSec #93).
        validate!(
            amount == 0 || n_shares > 0,
            ErrorCode::InvalidVaultDeposit,
            "deposit of {} mints zero shares (vault_equity {}, total_shares {})",
            amount,
            vault_equity,
            vault.total_shares
        )?;

        self.total_deposits = self.total_deposits.saturating_add(amount);
        self.net_deposits = self.net_deposits.safe_add(amount.cast()?)?;

        vault.total_deposits = vault.total_deposits.saturating_add(amount);
        vault.net_deposits = vault.net_deposits.safe_add(amount.cast()?)?;

        self.increase_vault_shares(n_shares, vault)?;

        vault.total_shares = vault.total_shares.safe_add(n_shares)?;
        vault.user_shares = vault.user_shares.safe_add(n_shares)?;

        let vault_shares_after = self.checked_vault_shares(vault)?;
        let protocol_shares_after = vault.get_protocol_shares(vault_protocol);

        match vault_protocol {
            None => {
                emit!(VaultDepositorRecord {
                    ts: now,
                    vault: vault.pubkey,
                    depositor_authority: self.authority,
                    action: VaultDepositorAction::Deposit,
                    amount,
                    spot_market_index: vault.spot_market_index,
                    vault_equity_before: vault_equity,
                    vault_shares_before,
                    user_vault_shares_before,
                    total_vault_shares_before,
                    vault_shares_after,
                    total_vault_shares_after: vault.total_shares,
                    user_vault_shares_after: vault.user_shares,
                    profit_share: manager_profit_share,
                    management_fee: management_fee_payment,
                    management_fee_shares,
                    deposit_oracle_price,
                });
            }
            Some(_) => {
                emit!(VaultDepositorV1Record {
                    ts: now,
                    vault: vault.pubkey,
                    depositor_authority: self.authority,
                    action: VaultDepositorAction::Deposit,
                    amount,
                    spot_market_index: vault.spot_market_index,
                    vault_equity_before: vault_equity,
                    vault_shares_before,
                    user_vault_shares_before,
                    total_vault_shares_before,
                    vault_shares_after,
                    total_vault_shares_after: vault.total_shares,
                    user_vault_shares_after: vault.user_shares,
                    protocol_profit_share,
                    protocol_fee: protocol_fee_payment,
                    protocol_fee_shares,
                    manager_profit_share,
                    management_fee: management_fee_payment,
                    management_fee_shares,
                    protocol_shares_before,
                    protocol_shares_after,
                    deposit_oracle_price,
                });
            }
        }

        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    pub fn request_withdraw(
        &mut self,
        withdraw_amount: u64,
        withdraw_unit: WithdrawUnit,
        vault_equity: u64,
        vault: &mut Vault,
        vault_protocol: &mut Option<RefMut<VaultProtocol>>,
        fee_update: &mut Option<AccountLoader<FeeUpdate>>,
        now: i64,
        deposit_oracle_price: i64,
    ) -> Result<()> {
        let mut rebase_divisor = self.apply_rebase(vault, vault_protocol, vault_equity)?;
        let VaultFee {
            management_fee_payment,
            management_fee_shares,
            protocol_fee_payment,
            protocol_fee_shares,
        } = vault.apply_fee(vault_protocol, fee_update, vault_equity, now)?;
        // apply_fee can cause a further vault rebase. Re-sync this depositor and its pending
        // request (OtterSec #107). Fold any extra divisor into rebase_divisor, so a Shares-unit
        // request still
        // converts the caller's original-base share count correctly.
        if let Some(extra) = self.apply_rebase(vault, vault_protocol, vault_equity)? {
            rebase_divisor = Some(rebase_divisor.unwrap_or(1).safe_mul(extra)?);
        }
        let (manager_profit_share, protocol_profit_share) =
            self.apply_profit_share(vault_equity, vault, vault_protocol, now)?;

        let (withdraw_value, n_shares) = withdraw_unit.get_withdraw_value_and_shares(
            withdraw_amount,
            vault_equity,
            self.get_vault_shares(),
            vault.total_shares,
            rebase_divisor,
        )?;

        validate!(
            n_shares > 0,
            ErrorCode::InvalidVaultWithdrawSize,
            "Requested n_shares = 0"
        )?;

        let vault_shares_before: u128 = self.checked_vault_shares(vault)?;
        let total_vault_shares_before = vault.total_shares;
        let user_vault_shares_before = vault.user_shares;
        let protocol_shares_before = vault.get_protocol_shares(vault_protocol);

        self.last_withdraw_request.set(
            vault_shares_before,
            n_shares,
            withdraw_value,
            vault_equity,
            now,
        )?;
        vault.total_withdraw_requested = vault.total_withdraw_requested.safe_add(withdraw_value)?;

        let vault_shares_after = self.checked_vault_shares(vault)?;
        let protocol_shares_after = vault.get_protocol_shares(vault_protocol);

        match vault_protocol {
            None => {
                emit!(VaultDepositorRecord {
                    ts: now,
                    vault: vault.pubkey,
                    depositor_authority: self.authority,
                    action: VaultDepositorAction::WithdrawRequest,
                    amount: self.last_withdraw_request.value,
                    spot_market_index: vault.spot_market_index,
                    vault_equity_before: vault_equity,
                    vault_shares_before,
                    user_vault_shares_before,
                    total_vault_shares_before,
                    vault_shares_after,
                    total_vault_shares_after: vault.total_shares,
                    user_vault_shares_after: vault.user_shares,
                    profit_share: manager_profit_share,
                    management_fee: management_fee_payment,
                    management_fee_shares,
                    deposit_oracle_price,
                });
            }
            Some(_) => {
                emit!(VaultDepositorV1Record {
                    ts: now,
                    vault: vault.pubkey,
                    depositor_authority: self.authority,
                    action: VaultDepositorAction::WithdrawRequest,
                    amount: self.last_withdraw_request.value,
                    spot_market_index: vault.spot_market_index,
                    vault_equity_before: vault_equity,
                    vault_shares_before,
                    user_vault_shares_before,
                    total_vault_shares_before,
                    vault_shares_after,
                    total_vault_shares_after: vault.total_shares,
                    user_vault_shares_after: vault.user_shares,
                    protocol_profit_share,
                    protocol_fee: protocol_fee_payment,
                    protocol_fee_shares,
                    manager_profit_share,
                    management_fee: management_fee_payment,
                    management_fee_shares,
                    protocol_shares_before,
                    protocol_shares_after,
                    deposit_oracle_price,
                });
            }
        }

        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    pub fn cancel_withdraw_request(
        &mut self,
        vault_equity: u64,
        vault: &mut Vault,
        vault_protocol: &mut Option<RefMut<VaultProtocol>>,
        fee_update: &mut Option<AccountLoader<FeeUpdate>>,
        now: i64,
        deposit_oracle_price: i64,
    ) -> Result<()> {
        self.apply_rebase(vault, vault_protocol, vault_equity)?;

        let vd_vault_shares_before: u128 = self.checked_vault_shares(vault)?;
        let total_vault_shares_before = vault.total_shares;
        let user_vault_shares_before = vault.user_shares;
        let protocol_shares_before = vault.get_protocol_shares(vault_protocol);

        let VaultFee {
            management_fee_payment,
            management_fee_shares,
            protocol_fee_payment,
            protocol_fee_shares,
        } = vault.apply_fee(vault_protocol, fee_update, vault_equity, now)?;
        // Re-sync the depositor and its pending request, in case apply_fee raised the vault
        // base (OtterSec #107). calculate_shares_lost and decrease_vault_shares then work in the
        // current base.
        self.apply_rebase(vault, vault_protocol, vault_equity)?;

        let vault_shares_lost = self
            .last_withdraw_request
            .calculate_shares_lost(vault, vault_equity)?;

        // only deduct lost shares if user doesn't own 100% of the vault
        let user_owns_entire_vault = total_vault_shares_before == vd_vault_shares_before;

        if vault_shares_lost > 0 && !user_owns_entire_vault {
            self.decrease_vault_shares(vault_shares_lost, vault)?;

            vault.total_shares = vault.total_shares.safe_sub(vault_shares_lost)?;
            vault.user_shares = vault.user_shares.safe_sub(vault_shares_lost)?;
        }

        let vault_shares_after = self.checked_vault_shares(vault)?;
        let protocol_shares_after = vault.get_protocol_shares(vault_protocol);

        match vault_protocol {
            None => {
                emit!(VaultDepositorRecord {
                    ts: now,
                    vault: vault.pubkey,
                    depositor_authority: self.authority,
                    action: VaultDepositorAction::CancelWithdrawRequest,
                    amount: 0,
                    spot_market_index: vault.spot_market_index,
                    vault_equity_before: vault_equity,
                    vault_shares_before: vd_vault_shares_before,
                    user_vault_shares_before,
                    total_vault_shares_before,
                    vault_shares_after,
                    total_vault_shares_after: vault.total_shares,
                    user_vault_shares_after: vault.user_shares,
                    profit_share: 0,
                    management_fee: management_fee_payment,
                    management_fee_shares,
                    deposit_oracle_price,
                });
            }
            Some(_) => {
                emit!(VaultDepositorV1Record {
                    ts: now,
                    vault: vault.pubkey,
                    depositor_authority: self.authority,
                    action: VaultDepositorAction::CancelWithdrawRequest,
                    amount: 0,
                    spot_market_index: vault.spot_market_index,
                    vault_equity_before: vault_equity,
                    vault_shares_before: vd_vault_shares_before,
                    user_vault_shares_before,
                    total_vault_shares_before,
                    vault_shares_after,
                    total_vault_shares_after: vault.total_shares,
                    user_vault_shares_after: vault.user_shares,
                    protocol_profit_share: 0,
                    protocol_fee: protocol_fee_payment,
                    protocol_fee_shares,
                    manager_profit_share: 0,
                    management_fee: management_fee_payment,
                    management_fee_shares,
                    protocol_shares_before,
                    protocol_shares_after,
                    deposit_oracle_price,
                });
            }
        }

        vault.total_withdraw_requested = vault
            .total_withdraw_requested
            .safe_sub(self.last_withdraw_request.value)?;

        self.last_withdraw_request.reset(now)?;

        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    pub fn withdraw(
        &mut self,
        vault_equity: u64,
        vault: &mut Vault,
        vault_protocol: &mut Option<RefMut<VaultProtocol>>,
        fee_update: &mut Option<AccountLoader<FeeUpdate>>,
        now: i64,
        deposit_oracle_price: i64,
    ) -> Result<(u64, bool)> {
        self.last_withdraw_request
            .check_redeem_period_finished(vault, now)?;

        self.apply_rebase(vault, vault_protocol, vault_equity)?;

        // Apply the fee before reading the request shares (OtterSec #107). apply_fee can mint
        // fee shares and cause a second vault rebase. Re-sync the depositor and its pending request, so n_shares
        // below is read in the same base as vault.total_shares and the base-checked
        // decrease_vault_shares does not abort with InvalidVaultRebase.
        let VaultFee {
            management_fee_payment,
            management_fee_shares,
            protocol_fee_payment,
            protocol_fee_shares,
        } = vault.apply_fee(vault_protocol, fee_update, vault_equity, now)?;
        self.apply_rebase(vault, vault_protocol, vault_equity)?;

        let vault_shares_before: u128 = self.checked_vault_shares(vault)?;
        let total_vault_shares_before = vault.total_shares;
        let user_vault_shares_before = vault.user_shares;
        let protocol_shares_before = vault.get_protocol_shares(vault_protocol);

        let n_shares = self.last_withdraw_request.shares;

        validate!(
            n_shares > 0,
            ErrorCode::InvalidVaultWithdraw,
            "No last_withdraw_request.shares found, must call request_withdraw first",
        )?;

        validate!(
            vault_shares_before >= n_shares,
            ErrorCode::InsufficientVaultShares
        )?;

        let amount: u64 =
            depositor_shares_to_vault_amount(n_shares, vault.total_shares, vault_equity)?;

        let withdraw_amount = amount.min(self.last_withdraw_request.value);
        msg!(
            "amount={}, last_withdraw_request_value={}",
            amount,
            self.last_withdraw_request.value
        );
        msg!(
            "vault_shares={}, last_withdraw_request_shares={}",
            self.get_vault_shares(),
            self.last_withdraw_request.shares
        );

        self.decrease_vault_shares(n_shares, vault)?;

        self.total_withdraws = self.total_withdraws.saturating_add(withdraw_amount);
        self.net_deposits = self.net_deposits.safe_sub(withdraw_amount.cast()?)?;

        vault.total_withdraws = vault.total_withdraws.saturating_add(withdraw_amount);
        vault.net_deposits = vault.net_deposits.safe_sub(withdraw_amount.cast()?)?;
        vault.total_shares = vault.total_shares.safe_sub(n_shares)?;
        vault.user_shares = vault.user_shares.safe_sub(n_shares)?;
        vault.total_withdraw_requested = vault
            .total_withdraw_requested
            .safe_sub(self.last_withdraw_request.value)?;

        self.last_withdraw_request.reset(now)?;

        let vault_shares_after = self.checked_vault_shares(vault)?;
        let protocol_shares_after = vault.get_protocol_shares(vault_protocol);

        match vault_protocol {
            None => {
                emit!(VaultDepositorRecord {
                    ts: now,
                    vault: vault.pubkey,
                    depositor_authority: self.authority,
                    action: VaultDepositorAction::Withdraw,
                    amount: withdraw_amount,
                    spot_market_index: vault.spot_market_index,
                    vault_equity_before: vault_equity,
                    vault_shares_before,
                    user_vault_shares_before,
                    total_vault_shares_before,
                    vault_shares_after,
                    total_vault_shares_after: vault.total_shares,
                    user_vault_shares_after: vault.user_shares,
                    profit_share: 0,
                    management_fee: management_fee_payment,
                    management_fee_shares,
                    deposit_oracle_price,
                });
            }
            Some(_) => {
                emit!(VaultDepositorV1Record {
                    ts: now,
                    vault: vault.pubkey,
                    depositor_authority: self.authority,
                    action: VaultDepositorAction::Withdraw,
                    amount: withdraw_amount,
                    spot_market_index: vault.spot_market_index,
                    vault_equity_before: vault_equity,
                    vault_shares_before,
                    user_vault_shares_before,
                    total_vault_shares_before,
                    vault_shares_after,
                    total_vault_shares_after: vault.total_shares,
                    user_vault_shares_after: vault.user_shares,
                    protocol_profit_share: 0,
                    protocol_fee: protocol_fee_payment,
                    protocol_fee_shares,
                    manager_profit_share: 0,
                    management_fee: management_fee_payment,
                    management_fee_shares,
                    protocol_shares_before,
                    protocol_shares_after,
                    deposit_oracle_price,
                });
            }
        }

        let finishing_liquidation = vault.liquidation_delegate == self.authority;

        Ok((withdraw_amount, finishing_liquidation))
    }

    pub fn apply_profit_share(
        &mut self,
        vault_equity: u64,
        vault: &mut Vault,
        vault_protocol: &mut Option<RefMut<VaultProtocol>>,
        _now: i64,
    ) -> Result<(u64, u64)> {
        validate!(
            !self.last_withdraw_request.pending(),
            ErrorCode::InvalidVaultDeposit,
            "Cannot apply profit share to depositor with pending withdraw request"
        )?;
        VaultDepositorBase::apply_profit_share(self, vault_equity, vault, vault_protocol)
    }

    #[allow(clippy::too_many_arguments)]
    pub fn realize_profits(
        &mut self,
        vault_equity: u64,
        vault: &mut Vault,
        vault_protocol: &mut Option<RefMut<VaultProtocol>>,
        fee_update: &mut Option<AccountLoader<FeeUpdate>>,
        now: i64,
        deposit_oracle_price: i64,
    ) -> Result<u64> {
        let VaultFee {
            management_fee_payment,
            management_fee_shares,
            protocol_fee_payment,
            protocol_fee_shares,
        } = vault.apply_fee(vault_protocol, fee_update, vault_equity, now)?;

        let vault_shares_before = self.checked_vault_shares(vault)?;
        let total_vault_shares_before = vault.total_shares;
        let user_vault_shares_before = vault.user_shares;
        let protocol_shares_before = vault.get_protocol_shares(vault_protocol);

        let (manager_profit_share, protocol_profit_share) =
            self.apply_profit_share(vault_equity, vault, vault_protocol, now)?;
        let profit_share = manager_profit_share.saturating_add(protocol_profit_share);
        let protocol_shares_after = vault.get_protocol_shares(vault_protocol);

        match vault_protocol {
            None => {
                emit!(VaultDepositorRecord {
                    ts: now,
                    vault: vault.pubkey,
                    depositor_authority: self.authority,
                    action: VaultDepositorAction::FeePayment,
                    amount: 0,
                    spot_market_index: vault.spot_market_index,
                    vault_equity_before: vault_equity,
                    vault_shares_before,
                    user_vault_shares_before,
                    total_vault_shares_before,
                    vault_shares_after: self.vault_shares,
                    total_vault_shares_after: vault.total_shares,
                    user_vault_shares_after: vault.user_shares,
                    profit_share: manager_profit_share,
                    management_fee: management_fee_payment,
                    management_fee_shares,
                    deposit_oracle_price,
                });
            }
            Some(_) => {
                emit!(VaultDepositorV1Record {
                    ts: now,
                    vault: vault.pubkey,
                    depositor_authority: self.authority,
                    action: VaultDepositorAction::FeePayment,
                    amount: 0,
                    spot_market_index: vault.spot_market_index,
                    vault_equity_before: vault_equity,
                    vault_shares_before,
                    user_vault_shares_before,
                    total_vault_shares_before,
                    vault_shares_after: self.vault_shares,
                    total_vault_shares_after: vault.total_shares,
                    user_vault_shares_after: vault.user_shares,
                    protocol_profit_share,
                    protocol_fee: protocol_fee_payment,
                    protocol_fee_shares,
                    manager_profit_share,
                    management_fee: management_fee_payment,
                    management_fee_shares,
                    protocol_shares_before,
                    protocol_shares_after,
                    deposit_oracle_price,
                });
            }
        }

        Ok(profit_share)
    }

    pub fn check_cant_withdraw(
        &self,
        vault: &Vault,
        vault_equity: u64,
        velocity_user: &mut User,
        maps: &mut velocity::instructions::optional_accounts::AccountMaps,
    ) -> Result<()> {
        let shares_value = depositor_shares_to_vault_amount(
            self.last_withdraw_request.shares,
            vault.total_shares,
            vault_equity,
        )?;
        let withdraw_amount = self.last_withdraw_request.value.min(shares_value);

        let mut spot_market = maps.spot_market_map.get_ref_mut(&vault.spot_market_index)?;

        // Save relevant data before updating balances
        let spot_market_deposit_balance_before = spot_market.deposit_balance;
        let spot_market_borrow_balance_before = spot_market.borrow_balance;
        let user_spot_position_before = velocity_user.spot_positions;

        update_spot_balances(
            withdraw_amount.cast()?,
            &SpotBalanceType::Borrow,
            &mut spot_market,
            velocity_user.force_get_spot_position_mut(vault.spot_market_index)?,
            true,
        )?;

        drop(spot_market);

        let sufficient_collateral = meets_initial_margin_requirement(velocity_user, maps)?;

        let margin_trading_ok = match validate_spot_margin_trading(velocity_user, maps) {
            Ok(_) => true,
            Err(VelocityErrorCode::MarginTradingDisabled) => false,
            Err(e) => {
                msg!("Error validating spot margin trading: {:?}", e);
                return Err(ErrorCode::VelocityError.into());
            }
        };

        if sufficient_collateral && margin_trading_ok {
            msg!(
                "depositor is able to withdraw. sufficient collateral = {} margin trading ok = {}",
                sufficient_collateral,
                margin_trading_ok
            );
            return Err(ErrorCode::VelocityError.into());
        }

        // Must reset velocity accounts afterward else ix will fail
        let mut spot_market = maps.spot_market_map.get_ref_mut(&vault.spot_market_index)?;
        spot_market.deposit_balance = spot_market_deposit_balance_before;
        spot_market.borrow_balance = spot_market_borrow_balance_before;

        velocity_user.spot_positions = user_spot_position_before;

        Ok(())
    }
}

#[cfg(test)]
mod vault_v1_tests {
    use {
        crate::{Vault, VaultDepositor, VaultProtocol, WithdrawUnit},
        anchor_lang::prelude::Pubkey,
        std::cell::RefCell,
        velocity::math::{
            casting::Cast,
            constants::{PERCENTAGE_PRECISION_U64, QUOTE_PRECISION_U64},
            insurance::if_shares_to_vault_amount,
        },
    };

    #[test]
    fn base_init() {
        let now = 1337;
        let vault = Vault::default();
        let vd = VaultDepositor::new(&vault, Pubkey::default(), Pubkey::default(), now);
        assert_eq!(vd.vault_shares_base, 0);
        assert_eq!(vd.last_valid_ts, now);
    }

    /// A positive deposit that floors to zero shares must revert (OtterSec #93).
    /// Otherwise the tokens become price appreciation on the existing shares. The rule matches
    /// `request_withdraw`'s `n_shares > 0` guard and the insurance-fund add path's
    /// `IFDepositMintsZeroShares`.
    #[test]
    fn deposit_rejects_zero_share_mint() {
        let now = 1000;
        let mut vault = Vault::default();
        let vp = RefCell::new(VaultProtocol::default());
        let vd = &mut VaultDepositor::new(&vault, Pubkey::default(), Pubkey::default(), now);

        // The first deposit sets shares one for one at 100 tokens of equity.
        vd.deposit(
            100 * QUOTE_PRECISION_U64,
            100 * QUOTE_PRECISION_U64,
            &mut vault,
            &mut Some(vp.borrow_mut()),
            &mut None,
            now,
            0,
        )
        .unwrap();

        // Equity triples, so one share is worth 3. A deposit of 1 unit floors to zero
        // shares. It must revert rather than become appreciation on the existing shares.
        assert!(
            vd.deposit(
                1,
                300 * QUOTE_PRECISION_U64,
                &mut vault,
                &mut Some(vp.borrow_mut()),
                &mut None,
                now,
                0,
            )
            .is_err(),
            "1-unit deposit into a 3x-inflated vault must revert, not mint zero shares"
        );
    }

    #[test]
    fn test_deposit_withdraw() {
        let now = 1000;
        let mut vault = Vault::default();
        let vp = RefCell::new(VaultProtocol::default());

        let vd = &mut VaultDepositor::new(&vault, Pubkey::default(), Pubkey::default(), now);

        let vault_equity: u64 = 100 * QUOTE_PRECISION_U64; // $100 in total equity
        let amount: u64 = 100 * QUOTE_PRECISION_U64; // $100 of new deposits to add to total equity, for new total of $200
        vd.deposit(
            amount,
            vault_equity,
            &mut vault,
            &mut Some(vp.borrow_mut()),
            &mut None,
            now + 20,
            0,
        )
        .unwrap();

        let vault_equity: u64 = 200 * QUOTE_PRECISION_U64;

        vd.request_withdraw(
            amount.cast().unwrap(),
            WithdrawUnit::Token,
            vault_equity,
            &mut vault,
            &mut Some(vp.borrow_mut()),
            &mut None,
            now + 20,
            0,
        )
        .unwrap();

        let (withdraw_amount, _) = vd
            .withdraw(
                vault_equity,
                &mut vault,
                &mut Some(vp.borrow_mut()),
                &mut None,
                now + 20,
                0,
            )
            .unwrap();
        assert_eq!(vd.vault_shares_base, 0);
        assert_eq!(withdraw_amount, amount);
    }

    #[test]
    fn test_deposit_partial_withdraw_profit_share() {
        let now = 1000;
        let mut vault = Vault::default();
        let vp = RefCell::new(VaultProtocol::default());
        vault.profit_share = 100_000; // 10% profit share
        vp.borrow_mut().protocol_profit_share = 50_000; // 5% profit share

        let vd = &mut VaultDepositor::new(&vault, Pubkey::default(), Pubkey::default(), now);

        let mut vault_equity: u64 = 100 * QUOTE_PRECISION_U64; // $100 in total equity for depositor
        let amount: u64 = 100 * QUOTE_PRECISION_U64; // $100 in total equity for vault
        vd.deposit(
            amount,
            vault_equity,
            &mut vault,
            &mut Some(vp.borrow_mut()),
            &mut None,
            now + 20,
            0,
        )
        .unwrap();
        assert_eq!(vd.vault_shares_base, 0);
        assert_eq!(vd.checked_vault_shares(&vault).unwrap(), 100_000_000); // 100_000_000 shares or $200 in equity
        assert_eq!(vault.user_shares, 100_000_000);
        assert_eq!(vault.total_shares, 200_000_000);

        vault_equity = 400 * QUOTE_PRECISION_U64; // vault gains 100% in value ($200 -> $400)

        // withdraw principal
        vd.request_withdraw(
            amount.cast().unwrap(), // only withdraw profit ($100)
            WithdrawUnit::Token,
            vault_equity,
            &mut vault,
            &mut Some(vp.borrow_mut()),
            &mut None,
            now + 20,
            0,
        )
        .unwrap();
        // 100M shares, 50M of which are profit. 15% profit share on 50M shares is 7.5M shares. 100M - 7.5M = 92.5M shares
        assert_eq!(vd.checked_vault_shares(&vault).unwrap(), 92_500_000);

        assert_eq!(vd.last_withdraw_request.shares, 50_000_000);
        assert_eq!(vd.last_withdraw_request.value, 100_000_000);
        assert_eq!(vd.last_withdraw_request.ts, now + 20);

        let (withdraw_amount, _ll) = vd
            .withdraw(
                vault_equity,
                &mut vault,
                &mut Some(vp.borrow_mut()),
                &mut None,
                now + 20,
                0,
            )
            .unwrap();
        // 100M shares minus 50M shares of profit and 15% or 7.5M profit share = 42.5M shares
        assert_eq!(vd.checked_vault_shares(&vault).unwrap(), 42_500_000);
        assert_eq!(vault.user_shares, 42_500_000);
        // manager is 200M total shares - 100M user shares + 5M or 10% profit share from user withdrawal.
        assert_eq!(
            vault
                .get_manager_shares(&mut Some(vp.borrow_mut()))
                .unwrap(),
            105_000_000
        );
        // protocol received 5% profit share on 50M shares, or 2.5M shares.
        assert_eq!(
            vault.get_protocol_shares(&mut Some(vp.borrow_mut())),
            2_500_000
        );
        assert_eq!(vd.vault_shares_base, 0);
        assert_eq!(vault.total_shares, 150_000_000);
        assert_eq!(withdraw_amount, amount);

        vault_equity -= withdraw_amount;

        let manager_owned_shares = vault
            .get_manager_shares(&mut Some(vp.borrow_mut()))
            .unwrap();
        let manager_owned_amount =
            if_shares_to_vault_amount(manager_owned_shares, vault.total_shares, vault_equity)
                .unwrap();
        // 100M shares or $200 in equity plus 10% of 50M shares or $100 profit which is $10, for a total of $210.
        assert_eq!(manager_owned_amount, 210_000_000);

        let user_owned_shares = vault.user_shares;
        let user_owned_amount =
            if_shares_to_vault_amount(user_owned_shares, vault.total_shares, vault_equity).unwrap();
        // $200 in equity - $100 in realized profit - 15% profit share on $100 = $85
        assert_eq!(user_owned_amount, 85_000_000);

        let protocol_owned_shares = vault.get_protocol_shares(&mut Some(vp.borrow_mut()));
        let protocol_owned_amount =
            if_shares_to_vault_amount(protocol_owned_shares, vault.total_shares, vault_equity)
                .unwrap();
        // 5% profit share on $100 = $5
        assert_eq!(protocol_owned_amount, 5_000_000);
    }

    #[test]
    fn test_deposit_partial_withdraw_profit_share_no_protocol() {
        let now = 1000;
        let mut vault = Vault::default();
        let vp = RefCell::new(VaultProtocol::default());
        vault.profit_share = 100_000; // 10% profit share

        let vd = &mut VaultDepositor::new(&vault, Pubkey::default(), Pubkey::default(), now);

        let mut vault_equity: u64 = 100 * QUOTE_PRECISION_U64; // $100 in total equity for depositor
        let amount: u64 = 100 * QUOTE_PRECISION_U64; // $100 in total equity for vault
        vd.deposit(
            amount,
            vault_equity,
            &mut vault,
            &mut Some(vp.borrow_mut()),
            &mut None,
            now + 20,
            0,
        )
        .unwrap();
        assert_eq!(vd.vault_shares_base, 0);
        assert_eq!(vd.checked_vault_shares(&vault).unwrap(), 100_000_000); // 100_000_000 shares or $200 in equity
        assert_eq!(vault.user_shares, 100_000_000);
        assert_eq!(vault.total_shares, 200_000_000);

        vault_equity = 400 * QUOTE_PRECISION_U64; // vault gains 100% in value ($200 -> $400)

        // withdraw principal
        vd.request_withdraw(
            amount.cast().unwrap(), // only withdraw profit ($100)
            WithdrawUnit::Token,
            vault_equity,
            &mut vault,
            &mut Some(vp.borrow_mut()),
            &mut None,
            now + 20,
            0,
        )
        .unwrap();
        assert_eq!(vd.checked_vault_shares(&vault).unwrap(), 95_000_000);

        assert_eq!(vd.last_withdraw_request.shares, 50_000_000);
        assert_eq!(vd.last_withdraw_request.value, 100_000_000);
        assert_eq!(vd.last_withdraw_request.ts, now + 20);

        let (withdraw_amount, _ll) = vd
            .withdraw(
                vault_equity,
                &mut vault,
                &mut Some(vp.borrow_mut()),
                &mut None,
                now + 20,
                0,
            )
            .unwrap();
        assert_eq!(vd.checked_vault_shares(&vault).unwrap(), 45_000_000);
        assert_eq!(vd.vault_shares_base, 0);
        assert_eq!(vault.user_shares, 45_000_000);
        assert_eq!(vault.total_shares, 150_000_000);
        assert_eq!(withdraw_amount, amount);

        vault_equity -= withdraw_amount;

        let manager_owned_shares = vault
            .get_manager_shares(&mut Some(vp.borrow_mut()))
            .unwrap();
        let manager_owned_amount =
            if_shares_to_vault_amount(manager_owned_shares, vault.total_shares, vault_equity)
                .unwrap();
        assert_eq!(manager_owned_amount, 210_000_000); // $210

        let user_owned_shares = vault.user_shares;
        let user_owned_amount =
            if_shares_to_vault_amount(user_owned_shares, vault.total_shares, vault_equity).unwrap();
        assert_eq!(user_owned_amount, 90_000_000); // $90

        let protocol_owned_shares = vault.get_protocol_shares(&mut Some(vp.borrow_mut()));
        let protocol_owned_amount =
            if_shares_to_vault_amount(protocol_owned_shares, vault.total_shares, vault_equity)
                .unwrap();
        println!("protocol amount: {}", protocol_owned_amount);
        assert_eq!(protocol_owned_amount, 0); // $100
    }

    #[test]
    fn test_deposit_full_withdraw_profit_share() {
        let now = 1000;
        let mut vault = Vault::default();
        let vp = RefCell::new(VaultProtocol::default());
        vault.profit_share = 100_000; // 10% profit share
        vp.borrow_mut().protocol_profit_share = 50_000; // 5% profit share

        let vd = &mut VaultDepositor::new(&vault, Pubkey::default(), Pubkey::default(), now);

        let mut vault_equity: u64 = 100 * QUOTE_PRECISION_U64;
        let amount: u64 = 100 * QUOTE_PRECISION_U64;
        vd.deposit(
            amount,
            vault_equity,
            &mut vault,
            &mut Some(vp.borrow_mut()),
            &mut None,
            now + 20,
            0,
        )
        .unwrap();
        assert_eq!(vd.vault_shares_base, 0);
        assert_eq!(vd.checked_vault_shares(&vault).unwrap(), 100_000_000);
        assert_eq!(vault.user_shares, 100_000_000);
        assert_eq!(vault.total_shares, 200_000_000);

        vault_equity = 400 * QUOTE_PRECISION_U64; // up 100%

        // withdraw all
        vd.request_withdraw(
            185 * QUOTE_PRECISION_U64, // vault_equity * (100% - 15% profit share)
            WithdrawUnit::Token,
            vault_equity,
            &mut vault,
            &mut Some(vp.borrow_mut()),
            &mut None,
            now + 20,
            0,
        )
        .unwrap();
        // user has 100M shares, with 100% profit, so 50M shares are profit.
        // profit share of 15% of 50M shares is 7.5M shares, and 100M - 7.5M = 92.5M shares
        assert_eq!(vd.checked_vault_shares(&vault).unwrap(), 92_500_000);
        assert_eq!(vd.last_withdraw_request.shares, 92_500_000);
        // user has 200M worth of value, with 15% profit share on 100M in profit, or 200M - 15M = 185M
        assert_eq!(vd.last_withdraw_request.value, 185_000_000);
        assert_eq!(vd.last_withdraw_request.ts, now + 20);

        let (withdraw_amount, _) = vd
            .withdraw(
                vault_equity,
                &mut vault,
                &mut Some(vp.borrow_mut()),
                &mut None,
                now + 20,
                0,
            )
            .unwrap();
        let profit = amount;
        let equity_minus_fee = amount + profit - (profit as f64 * 0.15).round() as u64;
        assert_eq!(vd.checked_vault_shares(&vault).unwrap(), 0);
        assert_eq!(vd.vault_shares_base, 0);
        assert_eq!(vault.user_shares, 0);
        // user had 100M shares, vault had 200M total
        // user paid 15% profit share on 50M shares, or 7.5M shares
        // total shares outside of user is now 100M + 7.5M = 107.5M
        assert_eq!(vault.total_shares, 107_500_000);
        assert_eq!(withdraw_amount, equity_minus_fee);
        // $85 = 100 - 10% - 5%, worth of profit that has been realized (this is not total fees paid)
        assert_eq!(vd.cumulative_profit_share_amount, 85_000_000);
        println!("vault shares: {}", vd.checked_vault_shares(&vault).unwrap());
        println!("shares base: {}", vd.vault_shares_base);
        println!("user shares: {}", vault.user_shares);
        println!("total shares: {}", vault.total_shares);
        println!(
            "withdraw amount: {}, actual: {}",
            withdraw_amount, equity_minus_fee
        );
        println!(
            "cum profit share amount: {}",
            vd.cumulative_profit_share_amount
        );

        vault_equity -= withdraw_amount;

        let manager_owned_shares = vault
            .get_manager_shares(&mut Some(vp.borrow_mut()))
            .unwrap();
        let manager_owned_amount =
            if_shares_to_vault_amount(manager_owned_shares, vault.total_shares, vault_equity)
                .unwrap();
        println!(
            "manager total profit share: {}",
            vault.manager_total_profit_share
        );
        println!("manager shares: {}", manager_owned_shares);
        println!("manager owned amount: {}", manager_owned_amount);
        // 10% of 50M shares of profit on top of 100M owned shares
        assert_eq!(manager_owned_shares, 105_000_000);
        // 10% of $100 in profit on top of $200 in owned equity
        // totals $210 in equity
        assert_eq!(manager_owned_amount, 210_000_000);

        let protocol_owned_shares = vault.get_protocol_shares(&mut Some(vp.borrow_mut()));
        let protocol_owned_amount =
            if_shares_to_vault_amount(protocol_owned_shares, vault.total_shares, vault_equity)
                .unwrap();
        println!(
            "protocol total profit share: {}",
            vp.borrow().protocol_total_profit_share
        );
        println!("protocol shares: {}", protocol_owned_shares);
        println!("protocol amount: {}", protocol_owned_amount);
        // 5% of 50M shares of profit
        assert_eq!(protocol_owned_shares, 2_500_000);
        // 5% of $100 in profit which totals $5 in equity
        assert_eq!(protocol_owned_amount, 5_000_000);
    }

    #[test]
    fn test_deposit_full_withdraw_profit_share_no_protocol() {
        let now = 1000;
        let mut vault = Vault::default();
        let vp = RefCell::new(VaultProtocol::default());
        vault.profit_share = 100_000; // 10% profit share

        let vd = &mut VaultDepositor::new(&vault, Pubkey::default(), Pubkey::default(), now);

        let mut vault_equity: u64 = 100 * QUOTE_PRECISION_U64;
        let amount: u64 = 100 * QUOTE_PRECISION_U64;
        vd.deposit(
            amount,
            vault_equity,
            &mut vault,
            &mut Some(vp.borrow_mut()),
            &mut None,
            now + 20,
            0,
        )
        .unwrap();
        assert_eq!(vd.vault_shares_base, 0);
        assert_eq!(vd.checked_vault_shares(&vault).unwrap(), 100_000_000);
        assert_eq!(vault.user_shares, 100_000_000);
        assert_eq!(vault.total_shares, 200_000_000);

        vault_equity = 400 * QUOTE_PRECISION_U64; // up 100%

        // withdraw all
        vd.request_withdraw(
            190 * QUOTE_PRECISION_U64, // vault_equity * (100% - 10% profit share)
            WithdrawUnit::Token,
            vault_equity,
            &mut vault,
            &mut Some(vp.borrow_mut()),
            &mut None,
            now + 20,
            0,
        )
        .unwrap();
        // user has 100M shares, with 100% profit, so 50M shares are profit.
        // profit share of 15% of 50M shares is 7.5M shares, and 100M - 5M = 95M shares
        assert_eq!(vd.checked_vault_shares(&vault).unwrap(), 95_000_000);
        assert_eq!(vd.last_withdraw_request.shares, 95_000_000);
        // user has 200M worth of value, with 10% profit share on 100M in profit, or 200M - 10M = 190M
        assert_eq!(vd.last_withdraw_request.value, 190_000_000);
        assert_eq!(vd.last_withdraw_request.ts, now + 20);

        let (withdraw_amount, _) = vd
            .withdraw(
                vault_equity,
                &mut vault,
                &mut Some(vp.borrow_mut()),
                &mut None,
                now + 20,
                0,
            )
            .unwrap();
        let profit = amount;
        let equity_minus_fee = amount + profit - (profit as f64 * 0.10).round() as u64;
        assert_eq!(vd.checked_vault_shares(&vault).unwrap(), 0);
        assert_eq!(vd.vault_shares_base, 0);
        assert_eq!(vault.user_shares, 0);
        // user had 100M shares, vault had 200M total
        // user paid 15% profit share on 50M shares, or 5M shares
        // total shares outside of user is now 100M + 5M = 105M
        assert_eq!(vault.total_shares, 105_000_000);
        assert_eq!(withdraw_amount, equity_minus_fee);
        // $90 = $100 - 10% worth of profit that has been realized (this is not total fees paid)
        assert_eq!(vd.cumulative_profit_share_amount, 90_000_000);
        println!("vault shares: {}", vd.checked_vault_shares(&vault).unwrap());
        println!("shares base: {}", vd.vault_shares_base);
        println!("user shares: {}", vault.user_shares);
        println!("total shares: {}", vault.total_shares);
        println!(
            "withdraw amount: {}, actual: {}",
            withdraw_amount, equity_minus_fee
        );
        println!(
            "cum profit share amount: {}",
            vd.cumulative_profit_share_amount
        );

        vault_equity -= withdraw_amount;

        let manager_owned_shares = vault
            .get_manager_shares(&mut Some(vp.borrow_mut()))
            .unwrap();
        let manager_owned_amount =
            if_shares_to_vault_amount(manager_owned_shares, vault.total_shares, vault_equity)
                .unwrap();
        println!(
            "manager total profit share: {}",
            vault.manager_total_profit_share
        );
        println!("manager shares: {}", manager_owned_shares);
        println!("manager owned amount: {}", manager_owned_amount);
        // 10% of 50M shares of profit on top of 100M owned shares
        assert_eq!(manager_owned_shares, 105_000_000);
        // 10% of $100 in profit on top of $200 in owned equity
        // totals $210 in equity
        assert_eq!(manager_owned_amount, 210_000_000);

        let protocol_owned_shares = vault.get_protocol_shares(&mut Some(vp.borrow_mut()));
        let protocol_owned_amount =
            if_shares_to_vault_amount(protocol_owned_shares, vault.total_shares, vault_equity)
                .unwrap();
        println!(
            "protocol total profit share: {}",
            vp.borrow().protocol_total_profit_share
        );
        println!("protocol shares: {}", protocol_owned_shares);
        println!("protocol amount: {}", protocol_owned_amount);
        // 0% of 50M shares of profit is 0 shares
        assert_eq!(protocol_owned_shares, 0);
        // 0% of $100 in profit which totals $0 in equity
        assert_eq!(protocol_owned_amount, 0);
    }

    #[test]
    fn test_force_realize_profit_share() {
        let now = 1000;
        let mut vault = Vault::default();
        let vp = RefCell::new(VaultProtocol::default());
        vault.profit_share = 100_000; // 10% profit share

        let vd = &mut VaultDepositor::new(&vault, Pubkey::default(), Pubkey::default(), now);

        let mut vault_equity: u64 = 100 * QUOTE_PRECISION_U64; // $100 in equity
        let amount: u64 = 100 * QUOTE_PRECISION_U64;
        vd.deposit(
            amount,
            vault_equity,
            &mut vault,
            &mut Some(vp.borrow_mut()),
            &mut None,
            now,
            0,
        )
        .unwrap();
        assert_eq!(vd.vault_shares_base, 0);
        assert_eq!(vd.checked_vault_shares(&vault).unwrap(), 100000000);
        assert_eq!(vault.user_shares, 100000000);
        assert_eq!(vault.total_shares, 200000000);

        // vault_protocol.protocol_profit_share = 50_000; // 5% profit share
        vault_equity = 400 * QUOTE_PRECISION_U64; // up 100%

        vd.realize_profits(
            vault_equity,
            &mut vault,
            &mut Some(vp.borrow_mut()),
            &mut None,
            now,
            0,
        )
        .unwrap();

        println!("vault shares: {}", vd.checked_vault_shares(&vault).unwrap());
        println!(
            "cum profit share amount: {}",
            vd.cumulative_profit_share_amount
        );
        println!("user shares: {}", vault.user_shares);
        println!("total shares: {}", vault.total_shares);
        // assert_eq!(vd.checked_vault_shares(vault).unwrap(), 95000000);
        // assert_eq!(vd.cumulative_profit_share_amount, 100000000); // $100
        // assert_eq!(vault.user_shares, 95000000); // $95
        // assert_eq!(vault.total_shares, 200000000); // $200

        // withdraw all
        vd.request_withdraw(
            190 * QUOTE_PRECISION_U64,
            WithdrawUnit::Token,
            vault_equity,
            &mut vault,
            &mut Some(vp.borrow_mut()),
            &mut None,
            now + 20,
            0,
        )
        .unwrap();
        assert_eq!(vd.checked_vault_shares(&vault).unwrap(), 95000000);

        assert_eq!(vd.last_withdraw_request.value, 190000000);
        assert_eq!(vd.last_withdraw_request.ts, now + 20);
        // assert_eq!(vd.last_withdraw_request.shares, 100000000);

        let (withdraw_amount, _ll) = vd
            .withdraw(
                vault_equity,
                &mut vault,
                &mut Some(vp.borrow_mut()),
                &mut None,
                now + 20,
                0,
            )
            .unwrap();
        // assert_eq!(vd.checked_vault_shares(vault).unwrap(), 0);
        // assert_eq!(vd.vault_shares_base, 0);
        // assert_eq!(vault.user_shares, 0);
        // assert_eq!(vault.total_shares, 105000000);
        assert_eq!(withdraw_amount, amount * 2 - amount * 2 / 20);
        // assert_eq!(vd.cumulative_profit_share_amount, 100000000); // $100
        println!("vault shares: {}", vd.checked_vault_shares(&vault).unwrap());
        println!("shares base: {}", vd.vault_shares_base);
        println!("user shares: {}", vault.user_shares);
        println!("total shares: {}", vault.total_shares);
        println!(
            "cum profit share amount: {}",
            vd.cumulative_profit_share_amount
        );
    }

    #[test]
    fn test_vault_depositor_request_in_loss_withdraw_in_profit() {
        // test for vault depositor who requests withdraw when in loss
        // then waits redeem period for withdraw
        // upon withdraw, vault depositor would have been in profit had they not requested in loss
        // should get request withdraw valuation and not break invariants

        let now = 1000;
        let mut vault = Vault::default();
        let vp = RefCell::new(VaultProtocol::default());

        let vd = &mut VaultDepositor::new(&vault, Pubkey::default(), Pubkey::default(), now);

        let mut vault_equity: u64 = 100 * QUOTE_PRECISION_U64;
        let amount: u64 = 100 * QUOTE_PRECISION_U64;
        vd.deposit(
            amount,
            vault_equity,
            &mut vault,
            &mut Some(vp.borrow_mut()),
            &mut None,
            now,
            0,
        )
        .unwrap();
        assert_eq!(vd.vault_shares_base, 0);
        assert_eq!(vd.checked_vault_shares(&vault).unwrap(), 100000000);
        assert_eq!(vault.user_shares, 100000000);
        assert_eq!(vault.total_shares, 200000000);

        vault.profit_share = 100_000; // 10% profit share
        vp.borrow_mut().protocol_profit_share = 50_000; // 5% profit share
        vault.redeem_period = 3600; // 1 hour
        vault_equity = 100 * QUOTE_PRECISION_U64; // down 50%

        // assert_eq!(vd.checked_vault_shares(vault).unwrap(), 100000000);
        // assert_eq!(vd.cumulative_profit_share_amount, 0); // $0
        // assert_eq!(vault.user_shares, 100000000);
        // assert_eq!(vault.total_shares, 200000000);
        println!("vault shares: {}", vd.checked_vault_shares(&vault).unwrap());
        println!(
            "cum profit share amount: {}",
            vd.cumulative_profit_share_amount
        );
        println!("user shares: {}", vault.user_shares);
        println!("total shares: {}", vault.total_shares);

        // let vault_before = vault;
        vd.realize_profits(
            vault_equity,
            &mut vault,
            &mut Some(vp.borrow_mut()),
            &mut None,
            now,
            0,
        )
        .unwrap(); // should be noop

        // request withdraw all
        vd.request_withdraw(
            PERCENTAGE_PRECISION_U64,
            WithdrawUnit::SharesPercent,
            vault_equity,
            &mut vault,
            &mut Some(vp.borrow_mut()),
            &mut None,
            now + 20,
            0,
        )
        .unwrap();
        // assert_eq!(vd.checked_vault_shares(vault).unwrap(), 100000000);
        println!("request shares: {}", vd.last_withdraw_request.shares);

        // assert_eq!(vd.last_withdraw_request.value, 50000000);
        assert_eq!(vd.last_withdraw_request.ts, now + 20);
        println!("request value: {}", vd.last_withdraw_request.value);

        vault_equity *= 5; // up 400%

        let (withdraw_amount, _ll) = vd
            .withdraw(
                vault_equity,
                &mut vault,
                &mut Some(vp.borrow_mut()),
                &mut None,
                now + 20 + 3600,
                0,
            )
            .unwrap();
        // assert_eq!(vd.checked_vault_shares(vault).unwrap(), 0);
        // assert_eq!(vd.vault_shares_base, 0);
        // assert_eq!(vault.user_shares, 0);
        // assert_eq!(vault.total_shares, 100000000);
        assert_eq!(withdraw_amount, 50000000);
        // assert_eq!(vd.cumulative_profit_share_amount, 0); // $0
        println!("vault shares: {}", vd.checked_vault_shares(&vault).unwrap());
        println!("shares base: {}", vd.vault_shares_base);
        println!("user shares: {}", vault.user_shares);
        println!("total shares: {}", vault.total_shares);
        println!(
            "cum profit share amount: {}",
            vd.cumulative_profit_share_amount
        );
    }

    #[test]
    fn test_vault_depositor_request_in_profit_withdraw_in_loss() {
        // test for vault depositor who requests withdraw when in profit
        // then waits redeem period for withdraw
        // upon withdraw, vault depositor is in loss even though they withdrew in profit
        // should get withdraw valuation and not break invariants

        let now = 1000;
        let mut vault = Vault::default();
        let vp = RefCell::new(VaultProtocol::default());

        let vd = &mut VaultDepositor::new(&vault, Pubkey::default(), Pubkey::default(), now);

        let mut vault_equity: u64 = 100 * QUOTE_PRECISION_U64;
        let amount: u64 = 100 * QUOTE_PRECISION_U64;
        vd.deposit(
            amount,
            vault_equity,
            &mut vault,
            &mut Some(vp.borrow_mut()),
            &mut None,
            now,
            0,
        )
        .unwrap();
        assert_eq!(vd.vault_shares_base, 0);
        assert_eq!(vd.checked_vault_shares(&vault).unwrap(), 100000000);
        assert_eq!(vault.user_shares, 100000000);
        assert_eq!(vault.total_shares, 200000000);

        vault.profit_share = 100_000; // 10% profit share
        vp.borrow_mut().protocol_profit_share = 50_000; // 5% profit share
        vault.redeem_period = 3600; // 1 hour
        vault_equity = 200 * QUOTE_PRECISION_U64;

        // assert_eq!(vd.checked_vault_shares(vault).unwrap(), 100000000);
        // assert_eq!(vd.cumulative_profit_share_amount, 0); // $0
        // assert_eq!(vault.user_shares, 100000000);
        // assert_eq!(vault.total_shares, 200000000);
        println!("vault shares: {}", vd.checked_vault_shares(&vault).unwrap());
        println!(
            "cum profit share amount: {}",
            vd.cumulative_profit_share_amount
        );
        println!("user shares: {}", vault.user_shares);
        println!("total shares: {}", vault.total_shares);

        // let vault_before = vault;
        vd.realize_profits(
            vault_equity,
            &mut vault,
            &mut Some(vp.borrow_mut()),
            &mut None,
            now,
            0,
        )
        .unwrap(); // should be noop

        // request withdraw all
        vd.request_withdraw(
            PERCENTAGE_PRECISION_U64,
            WithdrawUnit::SharesPercent,
            vault_equity,
            &mut vault,
            &mut Some(vp.borrow_mut()),
            &mut None,
            now + 20,
            0,
        )
        .unwrap();
        // assert_eq!(vd.checked_vault_shares(vault).unwrap(), 100000000);
        println!("request shares: {}", vd.last_withdraw_request.shares);

        assert_eq!(vd.last_withdraw_request.value, 100000000);
        assert_eq!(vd.last_withdraw_request.ts, now + 20);

        vault_equity /= 5; // down 80%

        let (withdraw_amount, _ll) = vd
            .withdraw(
                vault_equity,
                &mut vault,
                &mut Some(vp.borrow_mut()),
                &mut None,
                now + 20 + 3600,
                0,
            )
            .unwrap();
        // assert_eq!(vd.checked_vault_shares(vault).unwrap(), 0);
        // assert_eq!(vd.vault_shares_base, 0);
        // assert_eq!(vault.user_shares, 0);
        // assert_eq!(vault.total_shares, 100000000);
        assert_eq!(withdraw_amount, 20000000); // getting back 20% of deposit
                                               // assert_eq!(vd.cumulative_profit_share_amount, 0); // $0
        println!("vault shares: {}", vd.checked_vault_shares(&vault).unwrap());
        println!("shares base: {}", vd.vault_shares_base);
        println!("user shares: {}", vault.user_shares);
        println!("total shares: {}", vault.total_shares);
        println!(
            "cum profit share amount: {}",
            vd.cumulative_profit_share_amount
        );
    }

    // At a high share price a positive profit-share fee can floor to zero shares. The fee must
    // not settle in that case (OtterSec #104). Moving zero shares records the fee as paid while
    // nothing transfers. Taking a whole share takes far more value than the fee owes. The fee is
    // deferred instead. Transfer nothing, leave the high-water mark alone, and charge the fee
    // later, once accrued profit makes it worth at least one share.
    #[test]
    fn test_apply_profit_share_defers_sub_share_fee() {
        use crate::state::VaultDepositorBase;
        let now = 1000;

        // A fee below one share defers. No share moves and the high-water mark stays.
        let mut vault = Vault::default();
        let mut vp = None;
        vault.profit_share = 100_000; // 10%
        vault.total_shares = 100; // a high share price, few shares against large equity
        vault.user_shares = 100;

        let vd = &mut VaultDepositor::new(&vault, Pubkey::default(), Pubkey::default(), now);
        vd.set_vault_shares(100);
        vd.net_deposits = 999_999_900; // profit of 100 tokens -> fee of 10 tokens

        let vault_equity: u64 = 1_000_000_000; // ~1e7 per share, a 10-token fee floors to 0
        let shares_before = vd.get_vault_shares();
        let (mgr, proto) = vd
            .apply_profit_share(vault_equity, &mut vault, &mut vp, now)
            .unwrap();

        assert_eq!(
            shares_before,
            vd.get_vault_shares(),
            "a sub-share profit-share fee must not confiscate a whole share"
        );
        assert_eq!((mgr, proto), (0, 0), "deferred fee reports zero charged");
        assert_eq!(
            vd.profit_share_fee_paid, 0,
            "deferred fee must not be recorded as paid"
        );
        assert_eq!(
            vd.cumulative_profit_share_amount, 0,
            "deferred fee must not advance the high-water mark"
        );
        assert_eq!(vault.user_shares, 100, "vault.user_shares unchanged");

        // A fee worth one share or more is charged.
        let mut vault2 = Vault::default();
        let mut vp2 = None;
        vault2.profit_share = 100_000; // 10%
        vault2.total_shares = 100;
        vault2.user_shares = 100;

        let vd2 = &mut VaultDepositor::new(&vault, Pubkey::default(), Pubkey::default(), now);
        vd2.set_vault_shares(100);
        vd2.net_deposits = 0; // profit of 1e9 -> fee of 1e8 tokens = 10 shares

        let shares_before2 = vd2.get_vault_shares();
        let (mgr2, _) = vd2
            .apply_profit_share(vault_equity, &mut vault2, &mut vp2, now)
            .unwrap();
        assert_eq!(
            shares_before2 - vd2.get_vault_shares(),
            10,
            "a fee worth >= 1 share transfers the floored share count"
        );
        assert!(mgr2 > 0 && vd2.profit_share_fee_paid > 0);
    }

    // The signerless rebase, apply_rebase_public, must not floor an active depositor's shares to
    // zero (OtterSec #106).
    #[test]
    fn test_apply_rebase_public_rejects_flooring_to_zero() {
        use crate::state::VaultDepositorBase;
        let now = 1000;

        // A small share balance floors to zero, so the rebase is rejected.
        {
            let mut vault = Vault::default();
            let mut vp = None;
            vault.total_shares = 200_000_000;
            vault.user_shares = 200_000_000;
            let vd = &mut VaultDepositor::new(&vault, Pubkey::default(), Pubkey::default(), now);
            vd.set_vault_shares(10);
            let vault_equity: u64 = 2; // divisor 1e7 -> 10 shares floor to 0
            let res = vd.apply_rebase_public(&mut vault, &mut vp, vault_equity);
            assert!(
                res.is_err(),
                "public rebase must reject flooring a nonzero depositor to zero"
            );
        }

        // A large enough balance survives the rebase.
        {
            let mut vault = Vault::default();
            let mut vp = None;
            vault.total_shares = 200_000_000;
            vault.user_shares = 200_000_000;
            let vd = &mut VaultDepositor::new(&vault, Pubkey::default(), Pubkey::default(), now);
            vd.set_vault_shares(100_000_000);
            let vault_equity: u64 = 2; // divisor 1e7 -> 1e8 shares -> 10 (nonzero)
            let res = vd.apply_rebase_public(&mut vault, &mut vp, vault_equity);
            assert!(res.is_ok(), "public rebase should succeed: {:?}", res.err());
            assert_eq!(vd.get_vault_shares_base(), vault.shares_base);
        }
    }

    // transfer_shares must survive a fee-induced vault rebase (OtterSec #107).
    #[test]
    fn test_transfer_shares_after_fee_induced_rebase() {
        use crate::state::VaultDepositorBase;
        let now = 0;
        let mut vault = Vault::default();
        let vp = RefCell::new(VaultProtocol::default());
        vault.management_fee = 990_000; // 99%
        vault.last_fee_update_ts = 0;

        let vd1 = &mut VaultDepositor::new(&vault, Pubkey::new_unique(), Pubkey::new_unique(), now);
        let vd2 = &mut VaultDepositor::new(&vault, Pubkey::new_unique(), Pubkey::new_unique(), now);

        let vault_equity: u64 = 100 * QUOTE_PRECISION_U64;
        let amount: u64 = 100 * QUOTE_PRECISION_U64;
        vd1.deposit(
            amount,
            vault_equity,
            &mut vault,
            &mut Some(vp.borrow_mut()),
            &mut None,
            now,
            0,
        )
        .unwrap();

        let vault_equity = 200 * QUOTE_PRECISION_U64;
        let res = vd1.transfer_shares(
            vd2,
            &mut vault,
            &mut Some(vp.borrow_mut()),
            &mut None,
            PERCENTAGE_PRECISION_U64 / 2, // 50%
            WithdrawUnit::SharesPercent,
            vault_equity,
            now + 31_536_000, // ~1 year
            0,
        );
        assert!(
            res.is_ok(),
            "transfer_shares froze after fee rebase: {:?}",
            res.err()
        );
        assert!(vault.shares_base > 0);
        assert_eq!(vd1.get_vault_shares_base(), vault.shares_base);
        assert_eq!(vd2.get_vault_shares_base(), vault.shares_base);
    }
    /// A Token-unit share transfer must move the cost basis by the value of the shares that
    /// transfer, not by the caller's raw token request (OtterSec #138).
    ///
    /// For `WithdrawUnit::Token`, `get_withdraw_value_and_shares` returns
    /// `withdraw_value = withdraw_amount` unchanged, and it floors `n_shares` out of that
    /// amount. Crediting the recipient with the unfloored request hands them more basis than
    /// their new shares are worth. That shelters the same amount of future profit from the
    /// manager fee and the protocol performance fee, and it debits the sender by too much.
    /// `Shares` and `SharesPercent` already derive their value from `n_shares`, so all three
    /// units now agree.
    #[test]
    fn test_token_unit_transfer_moves_basis_by_transferred_shares() {
        use {
            crate::state::VaultDepositorBase,
            velocity::math::insurance::if_shares_to_vault_amount as depositor_shares_to_vault_amount,
        };
        let now = 0;
        let mut vault = Vault::default();
        let vp = RefCell::new(VaultProtocol::default());

        let vd1 = &mut VaultDepositor::new(&vault, Pubkey::new_unique(), Pubkey::new_unique(), now);
        let vd2 = &mut VaultDepositor::new(&vault, Pubkey::new_unique(), Pubkey::new_unique(), now);

        let vault_equity: u64 = 100 * QUOTE_PRECISION_U64;
        vd1.deposit(
            vault_equity,
            vault_equity,
            &mut vault,
            &mut Some(vp.borrow_mut()),
            &mut None,
            now,
            0,
        )
        .unwrap();

        // Raise the share price above 1, so a token request floors to fewer shares than it
        // nominally buys. Equity triples against the same share supply.
        let vault_equity = 300 * QUOTE_PRECISION_U64;

        // A request that cannot divide evenly into shares at this price.
        let request: u64 = 7;

        let from_basis_before = vd1.get_net_deposits();
        let to_basis_before = vd2.get_net_deposits();

        vd1.transfer_shares(
            vd2,
            &mut vault,
            &mut Some(vp.borrow_mut()),
            &mut None,
            request,
            WithdrawUnit::Token,
            vault_equity,
            now,
            0,
        )
        .unwrap();

        let shares_moved = vd2.get_vault_shares();
        assert!(shares_moved > 0, "fixture must move some shares");

        // The value the shares are worth, floored the same way the transfer floors it.
        let expected =
            depositor_shares_to_vault_amount(shares_moved, vault.total_shares, vault_equity)
                .unwrap()
                .min(vault_equity);

        let to_basis_delta = vd2.get_net_deposits() - to_basis_before;
        let from_basis_delta = from_basis_before - vd1.get_net_deposits();

        assert_eq!(
            to_basis_delta, expected as i64,
            "recipient basis must match the value of the shares received, not the raw \
             {} token request",
            request
        );
        assert_eq!(
            from_basis_delta, expected as i64,
            "sender basis must be debited by the same amount (conservation)"
        );
        assert!(
            to_basis_delta < request as i64,
            "fixture must actually floor, else it does not reproduce #138 ({} !< {})",
            to_basis_delta,
            request
        );
    }
}
