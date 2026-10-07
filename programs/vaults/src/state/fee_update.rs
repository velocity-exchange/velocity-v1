use {
    crate::{
        error::ErrorCode,
        events::{FeeUpdateAction, FeeUpdateRecord},
        state::{vault::validate_fee_policy, FeeUpdateStatus, Vault},
        validate, Size,
    },
    anchor_lang::prelude::*,
    static_assertions::const_assert_eq,
    velocity_macros::assert_no_slop,
};

#[assert_no_slop]
#[account(zero_copy(unsafe))]
#[derive(Default, Eq, PartialEq, Debug)]
#[repr(C)]
pub struct FeeUpdate {
    pub padding: [u128; 10],
    pub incoming_update_ts: i64,
    pub incoming_management_fee: i64,
    pub incoming_profit_share: u32,
    pub incoming_hurdle_rate: u32,
    pub padding2: [u8; 8],
}

impl Size for FeeUpdate {
    const SIZE: usize = 192 + 8;
}
const_assert_eq!(FeeUpdate::SIZE, std::mem::size_of::<FeeUpdate>() + 8);

impl FeeUpdate {
    pub fn reset(&mut self) {
        self.incoming_update_ts = 0;
        self.incoming_management_fee = 0;
        self.incoming_profit_share = 0;
        self.incoming_hurdle_rate = 0;
    }

    pub fn is_pending(&self) -> bool {
        self.incoming_update_ts > 0
    }

    /// Install a matured update.
    ///
    /// The management fee accrues over an interval, so the rate must change only at an instant
    /// where the vault is settled. Otherwise the new rate prices the interval that was earned
    /// under the old rate (OtterSec #98). [`Vault::apply_fee`] is the only caller. It settles
    /// the interval first and stamps `last_fee_update_ts`, which the check below requires.
    pub fn try_update_vault_fees(&mut self, now: i64, vault: &mut Vault) -> Result<()> {
        if !self.is_pending() {
            return Ok(());
        }

        if now >= self.incoming_update_ts {
            validate!(
                vault.last_fee_update_ts == now,
                ErrorCode::InvalidVaultUpdate,
                "vault fees must be settled to the current time before a fee update installs"
            )?;

            // Never install an out-of-bounds policy, even if a bad update reached the queue
            // (OtterSec #97). The combined protocol-sum check needs protocol state, which only
            // apply_fee holds. This call therefore checks the manager-facing bounds, and for a
            // protocol vault it also checks that the hurdle rate is zero. apply_fee validates
            // the combined sums against live protocol state before this runs.
            validate_fee_policy(
                self.incoming_management_fee,
                self.incoming_profit_share,
                self.incoming_hurdle_rate,
                vault.vault_protocol,
                0,
                0,
            )?;

            emit!(FeeUpdateRecord {
                ts: now,
                action: FeeUpdateAction::Applied,
                timelock_end_ts: self.incoming_update_ts,
                vault: vault.pubkey,
                old_management_fee: vault.management_fee,
                old_profit_share: vault.profit_share,
                old_hurdle_rate: vault.hurdle_rate,
                new_management_fee: self.incoming_management_fee,
                new_profit_share: self.incoming_profit_share,
                new_hurdle_rate: self.incoming_hurdle_rate,
            });

            vault.management_fee = self.incoming_management_fee;
            vault.profit_share = self.incoming_profit_share;
            vault.hurdle_rate = self.incoming_hurdle_rate;

            vault.fee_update_status = FeeUpdateStatus::None as u8;

            self.reset();
        }

        Ok(())
    }
}
