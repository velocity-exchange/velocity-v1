//! CPI-backed execute leg for the router fill. The fill entrypoint quotes
//! each registered external quoter up front (plain CPI + response read),
//! then threads this executor into the fill controller so allocations that
//! land on those books commit through the quoter's `execute_v0`. Indexing
//! matches the books the entrypoint built: quoter `i` produced book `i`.

use {
    super::quoted_route::QuotedEntry,
    crate::{
        error::{ErrorCode, VelocityResult},
        msg,
        state::prop_amm::{
            clob_resting_prefix, find_account, read_clob_u16, ClobCancelAllArgsV0,
            ClobCancelAllOutcomeV0, ClobCancelSides, ClobMarket, ClobUserRefV0, Direction,
            ExecuteArgsV0, ExternalQuoterExecutor, QuoterSubjects, QuoterType, QuoterUserSetRef,
            ResponseLocationV0, CLOB_MARKET_INDEX_OFFSET,
        },
        validate,
    },
    anchor_lang::prelude::*,
};

pub struct CpiQuoterExecutor<'a, 'info> {
    /// The entries that produced the router's external books, in book order,
    /// each carrying what quoting captured of it. Borrowed: quoting already
    /// owns these, and cloning them per fill spent heap for nothing.
    pub quoted: &'a [QuotedEntry<'info>],
    /// The perp market being filled — every entry must serve it.
    pub market_index: u16,
    /// The fill's account tail: the quoters' registered CPI accounts and their
    /// programs, searched by key.
    pub accounts: &'a [AccountInfo<'info>],
    /// The privilege-free PDA every quoter CPI signs as, and its bump.
    pub quoter_signer: Pubkey,
    pub quoter_signer_nonce: u8,
    /// The loaded-user set forwarded on every execute (quoters must not fill
    /// anyone else), in the wire's derivable form.
    pub users: &'a [ClobUserRefV0],
    /// The same caps the quote was taken with.
    pub caps: crate::state::prop_amm::QuoterUserCapsV0,
    /// The mark those caps were priced against, and the same one the quote
    /// carried: a quoter that spends budgets skips a different set of orders
    /// under a different mark.
    pub reference_price: i64,
    /// The taker — forwarded so quoters skip the taker's own resting
    /// liquidity (self-trade prevention).
    pub taker: ClobUserRefV0,
    pub slot: u64,
    pub now: i64,
}

impl<'info> ExternalQuoterExecutor<'info> for CpiQuoterExecutor<'_, 'info> {
    fn quoter_type(&self, index: usize) -> QuoterType {
        self.quoted
            .get(index)
            .map(|quoted| quoted.quoter_type)
            .unwrap_or(QuoterType::Custom)
    }

    fn quoter_user(&self, index: usize) -> Pubkey {
        self.quoted
            .get(index)
            .map(|quoted| quoted.user)
            .unwrap_or_default()
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
        let book_key = self
            .quoted
            .get(index)
            .map(|quoted| &quoted.response_account)
            .ok_or_else(|| {
                msg!("router executor index {} out of range", index);
                ErrorCode::DefaultError
            })?;
        let book = find_account(self.accounts, book_key).ok_or_else(|| {
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
            read_clob_u16(&data, CLOB_MARKET_INDEX_OFFSET) == Some(self.market_index),
            ErrorCode::InvalidQuoterConfig,
            "clob entry's response account {} is not the book for market {}",
            book_key,
            self.market_index
        )?;
        Ok(QuoterSubjects::Book(clob_resting_prefix(
            &data,
            direction.side(),
            size,
            &self.users,
            &self.caps,
            &self.taker,
            self.slot,
            self.now,
        )))
    }

    fn with_book(&self, index: usize, f: &mut dyn FnMut(&[u8])) -> VelocityResult<bool> {
        if self.quoter_type(index) != QuoterType::Clob {
            return Ok(false);
        }
        let book_key = self
            .quoted
            .get(index)
            .map(|quoted| &quoted.response_account)
            .ok_or(ErrorCode::DefaultError)?;
        let book = find_account(self.accounts, book_key).ok_or(ErrorCode::DefaultError)?;
        let data = book.try_borrow_data().map_err(|_| {
            msg!("clob book {} is already borrowed", book_key);
            ErrorCode::DefaultError
        })?;
        f(&data);
        Ok(true)
    }

    fn cancel_all(
        &mut self,
        index: usize,
        user: ClobUserRefV0,
        sides: ClobCancelSides,
    ) -> VelocityResult<Option<ClobCancelAllOutcomeV0>> {
        if self.quoter_type(index) != QuoterType::Clob {
            return Ok(None);
        }
        let loader = self
            .quoted
            .get(index)
            .map(|quoted| &quoted.entry)
            .ok_or_else(|| {
                msg!("router executor index {} out of range", index);
                ErrorCode::DefaultError
            })?;
        let quoter = loader.load().map_err(|_| {
            msg!("router executor failed to load quoter {}", index);
            ErrorCode::DefaultError
        })?;
        let book = find_account(self.accounts, &quoter.response_account).ok_or_else(|| {
            msg!(
                "clob book {} missing from the account map",
                quoter.response_account
            );
            ErrorCode::DefaultError
        })?;
        let program = find_account(self.accounts, &quoter.program_id).ok_or_else(|| {
            msg!(
                "clob program {} missing from the account map",
                quoter.program_id
            );
            ErrorCode::DefaultError
        })?;
        let signer = find_account(self.accounts, &self.quoter_signer).ok_or_else(|| {
            msg!("quoter signer missing from the account map");
            ErrorCode::DefaultError
        })?;
        let clob = ClobMarket::from_quoter(
            &quoter,
            self.market_index,
            book,
            program,
            signer,
            self.quoter_signer_nonce,
        )
        .map_err(|_| ErrorCode::DefaultError)?;
        clob.cancel_all(ClobCancelAllArgsV0 { user, sides })
            .map(Some)
            .map_err(|e| {
                msg!("clob cancel_all failed: {}", e);
                ErrorCode::DefaultError
            })
    }

    fn execute(
        &mut self,
        index: usize,
        direction: Direction,
        size: u64,
    ) -> VelocityResult<ResponseLocationV0<'info>> {
        let loader = self
            .quoted
            .get(index)
            .map(|quoted| &quoted.entry)
            .ok_or_else(|| {
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
                    caps: self.caps,
                    reference_price: self.reference_price,
                    direction,
                    size,
                    users: QuoterUserSetRef(self.users),
                    taker: Some(self.taker),
                },
                &self.quoter_signer,
                self.quoter_signer_nonce,
                self.accounts,
            )
            .map_err(|e| {
                msg!("external quoter execute failed: {}", e);
                ErrorCode::DefaultError
            })
    }
}
