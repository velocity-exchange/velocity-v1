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
            find_account, ClobCancelAllArgsV0, ClobCancelAllOutcomeV0, ClobCancelSides, ClobMarket,
            ClobUserRefV0, Direction, ExecuteArgsV0, ExternalQuoterExecutor, PriceLevel,
            QuoteArgsV0, QuoterSubjects, QuoterType, ResponseLocationV0,
        },
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
    /// The CPI buffers every leg of this fill reuses. One set for the whole
    /// instruction: velocity's heap never reclaims, so a buffer per leg is a
    /// buffer for the rest of the fill.
    pub scratch: &'a mut crate::state::prop_amm::QuoterCpiScratch<'info>,
    /// The CLOB place authority and its bump — what a `Clob` entry's CPI legs
    /// are signed as. Every other entry signs as a key derived from its own
    /// registry entry (`QuoterV0::cpi_signer`).
    pub clob_authority: Pubkey,
    pub clob_authority_nonce: u8,
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

    fn quoter_key(&self, index: usize) -> Pubkey {
        self.quoted
            .get(index)
            .map(|quoted| quoted.entry.key())
            .unwrap_or_default()
    }

    fn resting_levels(
        &mut self,
        index: usize,
        direction: Direction,
        size: u64,
    ) -> VelocityResult<Option<Vec<PriceLevel>>> {
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
        // The book's own `quote_v0`, with the identities and budgets the
        // execute below carries. Quote and execute are held to spending the
        // same set the same way, so a ladder taken here is the one that
        // execute fills — which is what makes it a bound the fill can be
        // checked against.
        Ok(Some(
            quoter
                .quote(
                    self.market_index,
                    QuoteArgsV0 {
                        users: self.users,
                        direction,
                        size,
                        caps: self.caps,
                        reference_price: self.reference_price,
                        taker: Some(self.taker),
                        // The bound is the fill's own, applied by the caller
                        // against the ladder that comes back.
                        limit_price: 0,
                    },
                    &loader.key(),
                    &self.clob_authority,
                    self.clob_authority_nonce,
                    self.accounts,
                    self.scratch,
                )
                .map_err(|_| {
                    msg!("clob quote for entry {} failed", loader.key());
                    ErrorCode::DefaultError
                })?
                .levels,
        ))
    }

    fn subjects(
        &self,
        index: usize,
        _direction: Direction,
        _size: u64,
    ) -> VelocityResult<QuoterSubjects> {
        Ok(if self.quoter_type(index) == QuoterType::Clob {
            QuoterSubjects::Book
        } else {
            QuoterSubjects::Account(self.quoter_user(index))
        })
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
        let signer = find_account(self.accounts, &self.clob_authority).ok_or_else(|| {
            msg!("clob place authority missing from the account map");
            ErrorCode::DefaultError
        })?;
        let clob = ClobMarket::from_quoter(
            &quoter,
            self.market_index,
            book,
            program,
            signer,
            self.clob_authority_nonce,
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
        let entry_key = loader.key();
        let (cpi_signer, cpi_signer_nonce) = quoter
            .cpi_signer(&entry_key, (self.clob_authority, self.clob_authority_nonce));
        quoter
            .execute(
                self.market_index,
                ExecuteArgsV0 {
                    caps: self.caps,
                    reference_price: self.reference_price,
                    direction,
                    size,
                    users: self.users,
                    taker: Some(self.taker),
                },
                &entry_key,
                &cpi_signer,
                cpi_signer_nonce,
                self.accounts,
                self.scratch,
            )
            .map_err(|e| {
                // Name the entry, not just the failure. A quoter program
                // serves many registry entries, so the program id an
                // off-chain router reads out of the runtime's CPI brackets
                // does not identify which entry failed. The key does.
                msg!("quoter {} execute failed: {}", loader.key(), e);
                ErrorCode::DefaultError
            })
    }
}
