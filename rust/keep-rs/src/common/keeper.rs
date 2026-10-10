//! Handles shared by the keeper bots
//!
//! Every bot reads the client account cache, sends through one tx worker and reports to one
//! metrics registry. `Keeper` bundles those three handles, so a pass takes one argument instead
//! of threading each handle through, and builds the transactions every bot starts from.

use {
    crate::common::{metrics::Metrics, tx::TxSender},
    std::{borrow::Cow, sync::Arc},
    velocity_rs::{types::accounts::User, Pubkey, TransactionBuilder, VelocityClient},
};

#[derive(Clone)]
pub(crate) struct Keeper {
    pub velocity: &'static VelocityClient,
    pub tx: TxSender,
    pub metrics: Arc<Metrics>,
}

impl Keeper {
    /// A user account from the client cache. `None` when the account is not cached, which the
    /// callers treat as a reason to skip the action, not an error.
    pub fn cached_user(&self, pubkey: &Pubkey) -> Option<User> {
        self.velocity.try_get_account::<User>(pubkey).ok()
    }

    /// A transaction signed by the bot subaccount `signer`, with the priority fee and the
    /// compute unit limit as its first two instructions.
    pub fn tx_builder<'a>(
        &self,
        signer: Pubkey,
        signer_account: Cow<'a, User>,
        priority_fee: u64,
        cu_limit: u32,
    ) -> TransactionBuilder<'a> {
        TransactionBuilder::new(self.velocity.program_data(), signer, signer_account, false)
            .with_priority_fee(priority_fee, Some(cu_limit))
    }
}

/// Wall-clock milliseconds since the unix epoch.
pub(crate) fn unix_now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::SystemTime::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64
}
