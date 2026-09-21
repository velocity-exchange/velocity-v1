//! Fixed keys the router trusts at compile time.

/// Only this key may run the one-time `initialize`, so the singleton cannot be front-run.
/// Reuses Velocity's `state_init_authority` key; gated to mainnet-beta builds like Velocity does.
/// A plain const, not `declare_id!`: a second `declare_id!` in the crate overwrites the
/// program address anchor writes into the generated IDL.
pub mod init_authority {
    use anchor_lang::prelude::{pubkey, Pubkey};

    pub const ID: Pubkey = pubkey!("prpHJmuXnqdaz92tBVdwsqmqyhqPLuq5Km35a5QWco3");

    pub const fn id() -> Pubkey {
        ID
    }
}
