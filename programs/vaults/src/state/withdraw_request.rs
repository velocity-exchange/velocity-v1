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

        // A pending request covering the *entire* share supply forfeits nothing: the
        // redeem-period forfeiture accrues to the depositors who stay, and here there are
        // none. This has to be decided BEFORE the conservation guard below, because the
        // restake leg prices `self.value` against a post-removal pool of
        // `total_shares - n_shares == 0` shares, so `new_n_shares` floors to 0 and the guard
        // rejects the cancel outright — permanently, since re-requesting and depositing are
        // both blocked while a request is pending. That left a 100% depositor no exit but
        // `withdraw` at the stale frozen value, forfeiting every subsequent gain to
        // synthesized manager shares (finding #126, a regression introduced by the #93
        // guard). `cancel_withdraw_request`'s `user_owns_entire_vault` check stays: it covers
        // the wider case of owning 100% while requesting only part of it, where the math
        // below still computes a (self-directed) forfeiture.
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

            // Conservation-aware guard: a positive frozen request value must not
            // floor the retained shares to zero. When equity rises far enough
            // that `self.value` rounds to <1 share of the post-removal pool,
            // `new_n_shares` floors to 0 and the depositor would forfeit its
            // ENTIRE stake on cancel — not just the gain the redeem-period
            // forfeiture is meant to claw back. The block on vault-owned
            // revenue-share sweeps removes the donation vector that made this
            // reachable cheaply (OtterSec #93), but reject the transition
            // outright so no equity increase (donated or genuine) can burn a
            // positive claim. The depositor can still `withdraw` at its frozen
            // value instead.
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

    /// OtterSec #93 (defense-in-depth): when equity rises far enough that a
    /// pending request's frozen value rounds to < 1 share of the post-removal
    /// pool, the retained-share calc floors to zero and cancel would burn the
    /// depositor's ENTIRE stake. That transition must revert, not silently
    /// forfeit a positive claim.
    #[test]
    fn calculate_shares_lost_rejects_full_claim_burn() {
        let mut vault = Vault::default();
        vault.total_shares = 2;
        let req = WithdrawRequest {
            shares: 1,
            value: 100, // small frozen request value
            ts: 0,
        };
        // Huge equity: the 1 frozen share is worth ~equity/2 (>> 100), and the
        // shares worth `value` at the post-removal pool floor to 0.
        let vault_equity: u64 = 1_000_000_000_000;
        assert!(
            req.calculate_shares_lost(&vault, vault_equity).is_err(),
            "cancel that would burn the entire claim must revert"
        );
    }

    /// OtterSec #126: the #93 guard above must not fire for a depositor whose pending
    /// request covers 100% of `total_shares`. There the post-removal pool has zero shares,
    /// so `new_n_shares` floors to 0 for a structural reason rather than a rounding one, and
    /// rejecting the cancel stranded the depositor: re-request and deposit are both blocked
    /// while a request is pending, leaving only a `withdraw` at the stale frozen value.
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

    /// Sanity: an ordinary cancel with no equity gain forfeits nothing.
    #[test]
    fn calculate_shares_lost_no_gain_forfeits_nothing() {
        let mut vault = Vault::default();
        vault.total_shares = 200;
        let req = WithdrawRequest {
            shares: 100,
            value: 100,
            ts: 0,
        };
        // amount == value (no gain) -> else branch -> zero shares lost.
        assert_eq!(req.calculate_shares_lost(&vault, 200).unwrap(), 0);
    }
}
