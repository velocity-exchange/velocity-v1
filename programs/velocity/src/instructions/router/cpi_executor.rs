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
            clob_resting_prefix, read_clob_u32, ClobUserRefV0, Direction, ExecuteArgsV0,
            ExecuteResponseV0, ExternalQuoterExecutor, QuoterSubjects, QuoterType, QuoterUserSetV0,
            QuoterV0, CLOB_MARKET_INDEX_OFFSET,
        },
        validate,
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
    /// account the pre-execute clamp sizes Custom books against, and the only
    /// subject a Custom entry's response may name.
    pub quoter_users: Vec<Pubkey>,
    /// Registry `response_account` per entry. For a CLOB entry that is the
    /// book, which is where the entry's permitted subjects are read from.
    pub response_accounts: Vec<Pubkey>,
    /// The perp market being filled — every entry must serve it.
    pub market_index: u16,
    /// Union of the quoters' registered CPI accounts (plus their programs),
    /// keyed by pubkey — the caller's leftover remaining accounts.
    pub account_map: &'a BTreeMap<Pubkey, AccountInfo<'info>>,
    pub velocity_signer: Pubkey,
    pub signer_nonce: u8,
    /// The loaded-user set forwarded on every execute (quoters must not fill
    /// anyone else), in the wire's derivable form.
    pub users: QuoterUserSetV0,
    /// The taker — forwarded so quoters skip the taker's own resting
    /// liquidity (self-trade prevention).
    pub taker: ClobUserRefV0,
    pub slot: u64,
    pub now: i64,
}

impl ExternalQuoterExecutor for CpiQuoterExecutor<'_, '_> {
    fn quoter_type(&self, index: usize) -> QuoterType {
        self.types.get(index).copied().unwrap_or(QuoterType::Custom)
    }

    fn quoter_user(&self, index: usize) -> Pubkey {
        self.quoter_users.get(index).copied().unwrap_or_default()
    }

    fn subjects(
        &self,
        index: usize,
        direction: Direction,
        size: u64,
    ) -> VelocityResult<QuoterSubjects> {
        if self.quoter_type(index) != QuoterType::Clob {
            return Ok(QuoterSubjects::Account(self.quoter_user(index)));
        }
        let book_key = self.response_accounts.get(index).ok_or_else(|| {
            msg!("router executor index {} out of range", index);
            ErrorCode::DefaultError
        })?;
        let book = self.account_map.get(book_key).ok_or_else(|| {
            msg!("clob book {} missing from the account map", book_key);
            ErrorCode::DefaultError
        })?;
        let data = book.try_borrow_data().map_err(|_| {
            msg!("clob book {} is already borrowed", book_key);
            ErrorCode::DefaultError
        })?;
        // The registry says this account holds the entry's responses; that it
        // is also the book serving this market is re-derived from its bytes,
        // so a misregistered entry reads as an error rather than as an empty
        // — and therefore permissive-of-nothing — book.
        validate!(
            read_clob_u32(&data, CLOB_MARKET_INDEX_OFFSET) == Some(self.market_index as u32),
            ErrorCode::InvalidQuoterConfig,
            "clob entry's response account {} is not the book for market {}",
            book_key,
            self.market_index
        )?;
        Ok(QuoterSubjects::Book(clob_resting_prefix(
            &data,
            direction.clob_side(),
            size,
            &self.users,
            &self.taker,
            self.slot,
            self.now,
        )))
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
                self.market_index,
                ExecuteArgsV0 {
                    direction,
                    size,
                    users: self.users,
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
