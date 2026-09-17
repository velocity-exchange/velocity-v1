use {
    crate::{
        error::{ErrorCode, VaultResult},
        validate, Vault,
    },
    anchor_lang::{prelude::*, solana_program},
    bytemuck::Zeroable,
    solana_program::msg,
    static_assertions::const_assert_eq,
    velocity::math::{
        insurance::{
            if_shares_to_vault_amount as depositor_shares_to_vault_amount,
            vault_amount_to_if_shares as vault_amount_to_depositor_shares,
        },
        safe_math::SafeMath,
    },
    velocity_macros::assert_no_slop,
};

#[assert_no_slop]
#[derive(
    Default, AnchorSerialize, AnchorDeserialize, Copy, Clone, Eq, PartialEq, Debug, Zeroable,
)]
#[repr(C)]
pub struct WithdrawRequest {
    /// request shares of vault withdraw
    pub shares: u128,
    /// requested value (in vault spot_market_index) of shares for withdraw
    pub value: u64,
    /// request ts of vault withdraw
    pub ts: i64,
}

impl WithdrawRequest {
    pub fn pending(&self) -> bool {
        self.shares != 0 || self.value != 0
    }

    pub fn rebase(&mut self, rebase_divisor: u128) -> VaultResult {
        self.shares = self.shares.safe_div(rebase_divisor)?;
        Ok(())
    }

    pub fn calculate_shares_lost(&self, vault: &Vault, vault_equity: u64) -> VaultResult<u128> {
        let n_shares = self.shares;

        // A pending request that covers the entire share supply forfeits nothing. The
        // redeem-period forfeiture accrues to the depositors who stay, and here there are
        // none.
        //
        // This case must return before the guard below. The guard prices `self.value` against
        // a post-removal pool of `total_shares - n_shares`, which is 0 shares here. So
        // `new_n_shares` floors to 0 and the guard rejects the cancel. That rejection is
        // permanent, because a pending request blocks both a new request and a deposit. The
        // depositor would have no exit except `withdraw` at the stale frozen value, which
        // forfeits every later gain to newly issued manager shares (OtterSec #126, a
        // regression introduced by the OtterSec #93 guard).
        //
        // `cancel_withdraw_request` keeps its `user_owns_entire_vault` check. That check
        // covers the wider case of owning every share while requesting only part of them,
        // where the math below still computes a forfeiture to the depositor itself.
        if n_shares >= vault.total_shares {
            return Ok(0);
        }

        let amount = depositor_shares_to_vault_amount(n_shares, vault.total_shares, vault_equity)?;

        let vault_shares_lost = if amount > self.value {
            let new_n_shares = vault_amount_to_depositor_shares(
                self.value,
                vault.total_shares.safe_sub(n_shares)?,
                vault_equity.safe_sub(self.value)?,
            )?;

            validate!(
                new_n_shares <= n_shares,
                ErrorCode::InvalidVaultSharesDetected,
                "Issue calculating delta if_shares after canceling request {} < {}",
                new_n_shares,
                n_shares
            )?;

            // A positive frozen request value must not floor the retained shares
            // to zero. When equity rises far enough that `self.value` rounds to
            // less than one share of the post-removal pool, `new_n_shares` floors
            // to 0. The depositor would then forfeit its whole stake on cancel,
            // and not only the gain that the redeem-period forfeiture recovers.
            // Blocking vault-owned revenue-share sweeps removes the cheap way to
            // donate equity into this state (OtterSec #93). Reject the transition
            // anyway, so no equity increase can burn a positive claim. The
            // depositor can still `withdraw` at its frozen value.
            validate!(
                new_n_shares > 0 || self.value == 0,
                ErrorCode::InvalidVaultSharesDetected,
                "canceling would burn the entire {}-share claim (frozen value {}, equity {})",
                n_shares,
                self.value,
                vault_equity
            )?;

            n_shares.safe_sub(new_n_shares)?
        } else {
            0
        };

        Ok(vault_shares_lost)
    }

    pub fn set(
        &mut self,
        current_shares: u128,
        withdraw_shares: u128,
        withdraw_value: u64,
        vault_equity: u64,
        now: i64,
    ) -> VaultResult {
        validate!(
            self.value == 0,
            ErrorCode::VaultWithdrawRequestInProgress,
            "withdraw request is already in progress"
        )?;

        validate!(
            withdraw_shares <= current_shares,
            ErrorCode::InvalidVaultWithdrawSize,
            "shares requested exceeds vault_shares {} > {}",
            withdraw_shares,
            current_shares
        )?;

        self.shares = withdraw_shares;

        validate!(
            withdraw_value == 0 || withdraw_value <= vault_equity,
            ErrorCode::InvalidVaultWithdrawSize,
            "Requested withdraw value {} is not equal or below vault_equity {}",
            withdraw_value,
            vault_equity
        )?;

        self.value = withdraw_value;

        self.ts = now;

        Ok(())
    }

    pub fn reset(&mut self, now: i64) -> VaultResult {
        // reset vault_depositor withdraw request info
        self.shares = 0;
        self.value = 0;
        self.ts = now;

        Ok(())
    }

    pub fn check_redeem_period_finished(&self, vault: &Vault, now: i64) -> VaultResult {
        let time_since_withdraw_request = now.safe_sub(self.ts)?;

        validate!(
            time_since_withdraw_request >= vault.redeem_period,
            ErrorCode::CannotWithdrawBeforeRedeemPeriodEnd
        )?;

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// When equity rises far enough that a pending request's frozen value rounds
    /// to less than one share of the post-removal pool, the retained-share
    /// calculation floors to zero. A cancel would then burn the depositor's whole
    /// stake. That transition must revert rather than forfeit a positive claim
    /// (OtterSec #93).
    #[test]
    fn calculate_shares_lost_rejects_full_claim_burn() {
        let mut vault = Vault::default();
        vault.total_shares = 2;
        let req = WithdrawRequest {
            shares: 1,
            value: 100, // small frozen request value
            ts: 0,
        };
        // The one frozen share is worth about half the equity, far more than 100.
        // The shares that `value` buys in the post-removal pool floor to 0.
        let vault_equity: u64 = 1_000_000_000_000;
        assert!(
            req.calculate_shares_lost(&vault, vault_equity).is_err(),
            "cancel that would burn the entire claim must revert"
        );
    }

    /// The guard above must not fire for a depositor whose pending request covers every
    /// share in `total_shares` (OtterSec #126, on the OtterSec #93 guard). The post-removal
    /// pool then holds zero shares, so `new_n_shares` floors to 0 for a structural reason
    /// and not a rounding one. Rejecting that cancel strands the depositor. A pending
    /// request blocks both a new request and a deposit, which leaves only a `withdraw` at
    /// the stale frozen value.
    #[test]
    fn calculate_shares_lost_sole_depositor_full_request_can_cancel() {
        let mut vault = Vault::default();
        vault.total_shares = 100;
        let req = WithdrawRequest {
            shares: 100, // the entire share supply
            value: 100,  // frozen at request time
            ts: 0,
        };
        // Equity rose during the redeem period, so `amount > value` and the forfeiture
        // branch is reached. A sole depositor has nobody to forfeit to.
        assert_eq!(req.calculate_shares_lost(&vault, 110).unwrap(), 0);
    }

    /// An ordinary cancel with no equity gain forfeits nothing.
    #[test]
    fn calculate_shares_lost_no_gain_forfeits_nothing() {
        let mut vault = Vault::default();
        vault.total_shares = 200;
        let req = WithdrawRequest {
            shares: 100,
            value: 100,
            ts: 0,
        };
        // amount equals value, so there is no gain and the else branch loses no shares.
        assert_eq!(req.calculate_shares_lost(&vault, 200).unwrap(), 0);
    }
}
