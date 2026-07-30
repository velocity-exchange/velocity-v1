//! CPI-backed execute leg for the router fill. The fill entrypoint quotes
//! each registered external quoter up front (plain CPI + response read),
//! then threads this executor into the fill controller so allocations that
//! land on those books commit through the quoter's `execute_v0`. Indexing
//! matches the books the entrypoint built: quoter `i` produced book `i`.

use {
    crate::{
        error::{ErrorCode, VelocityResult},
        msg,
        state::prop_amm::{
            ClobUserRefV0, Direction, ExecuteArgsV0, ExecuteResponseV0, ExternalQuoterExecutor,
            QuoterType, QuoterV0,
        },
    },
    anchor_lang::prelude::*,
    std::collections::BTreeMap,
};

pub struct CpiQuoterExecutor<'a, 'info> {
    /// The registry entries that produced the router's external books, in
    /// book order (inactive/unapproved entries were dropped at quote time).
    pub quoters: &'a [AccountLoader<'info, QuoterV0>],
    /// Registry types, captured at quote time so the fill controller can ask
    /// without re-loading the entry.
    pub types: Vec<QuoterType>,
    /// Registry `user` per entry, captured at quote time — the margin
    /// account the pre-execute clamp sizes Custom books against.
    pub quoter_users: Vec<Pubkey>,
    /// Union of the quoters' registered CPI accounts (plus their programs),
    /// keyed by pubkey — the caller's leftover remaining accounts.
    pub account_map: &'a BTreeMap<Pubkey, AccountInfo<'info>>,
    pub velocity_signer: Pubkey,
    pub signer_nonce: u8,
    /// The loaded-user set forwarded on every execute (quoters must not fill
    /// anyone else), in the wire's derivable form.
    pub users: Vec<ClobUserRefV0>,
    /// The taker — forwarded so quoters skip the taker's own resting
    /// liquidity (self-trade prevention).
    pub taker: ClobUserRefV0,
}

impl ExternalQuoterExecutor for CpiQuoterExecutor<'_, '_> {
    fn quoter_type(&self, index: usize) -> QuoterType {
        self.types.get(index).copied().unwrap_or(QuoterType::Custom)
    }

    fn quoter_user(&self, index: usize) -> Pubkey {
        self.quoter_users.get(index).copied().unwrap_or_default()
    }

    fn execute(
        &mut self,
        index: usize,
        direction: Direction,
        size: u64,
    ) -> VelocityResult<ExecuteResponseV0> {
        let loader = self.quoters.get(index).ok_or_else(|| {
            msg!("router executor index {} out of range", index);
            ErrorCode::DefaultError
        })?;
        let quoter = loader.load().map_err(|_| {
            msg!("router executor failed to load quoter {}", index);
            ErrorCode::DefaultError
        })?;
        quoter
            .execute(
                ExecuteArgsV0 {
                    direction,
                    size,
                    users: Some(self.users.clone()),
                    taker: Some(self.taker),
                },
                &self.velocity_signer,
                self.signer_nonce,
                self.account_map,
            )
            .map_err(|e| {
                msg!("external quoter execute failed: {}", e);
                ErrorCode::DefaultError
            })
    }
}
