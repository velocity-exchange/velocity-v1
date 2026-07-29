use {
    crate::{
        events::{FeeUpdateAction, FeeUpdateRecord},
        state::{vault::validate_fee_policy, FeeUpdateStatus, Vault},
        Size,
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

    pub fn try_update_vault_fees(&mut self, now: i64, vault: &mut Vault) -> Result<()> {
        if !self.is_pending() {
            return Ok(());
        }

        if now >= self.incoming_update_ts {
            // #97: defense-in-depth — never install an out-of-bounds policy, even if a bad update
            // was somehow queued. The combined protocol-sum check needs protocol state (only
            // available via apply_fee), so here we enforce the manager-facing bounds and, for
            // protocol vaults, the hurdle==0 restriction. apply_fee validates the combined sums
            // against live protocol state before this runs.
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

            // #98: treat the fee change as a rate-epoch boundary. Stamp last_fee_update_ts to the
            // activation instant so the new (possibly raised) rate never retroactively prices the
            // pre-activation interval. The pre-activation interval was charged at the old rate on
            // prior interactions; any un-accrued sliver is conservatively forfeited rather than
            // re-priced at the new rate. Uses max() so a timestamp already past the boundary is
            // never moved backward (which would double-charge).
            vault.last_fee_update_ts = vault.last_fee_update_ts.max(self.incoming_update_ts);

            self.reset();
        }

        Ok(())
    }
}
