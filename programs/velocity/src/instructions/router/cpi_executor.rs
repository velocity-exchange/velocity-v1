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
            find_account, ClobCancelAllArgsV0, ClobCancelAllOutcomeV0, ClobCancelSides, ClobMarket,
            ClobUserRefV0, Direction, ExecuteArgsV0, ExternalQuoterExecutor, PriceLevel,
            QuoteArgsV0, QuoterSlabExt, QuoterSlabV0, QuoterSlotV0, QuoterSubjects, QuoterType,
            ResponseLocationV0,
        },
    },
    anchor_lang::prelude::*,
};

pub struct CpiQuoterExecutor<'a, 'info> {
    /// The market's slab. `None` when the transaction consulted nothing
    /// external, in which case `slots` is empty and every accessor answers
    /// its default.
    pub slab: Option<&'a AccountLoader<'info, QuoterSlabV0>>,
    /// The slab slot behind each of the route's external books, in book
    /// order: book `i` executes through slot `slots[i]`. Everything else
    /// about a quoter is read out of the slab on demand — the slab is the one
    /// copy of every approved config, so nothing is captured ahead of time.
    pub slots: &'a [usize],
    /// The perp market being filled — every entry must serve it.
    pub market_index: u16,
    /// The fill's account tail: the quoters' registered CPI accounts and their
    /// programs, searched by key.
    pub accounts: &'a [AccountInfo<'info>],
    /// The CPI buffers every leg of this fill reuses. One set for the whole
    /// instruction: velocity's heap never reclaims, so a buffer per leg is a
    /// buffer for the rest of the fill.
    pub scratch: &'a mut crate::state::prop_amm::QuoterCpiScratch<'info>,
    /// Forwarded on every quote and execute leg — see
    /// [`crate::instructions::QuoteInputs::taker_served_window`].
    pub taker_served_window: bool,
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

impl<'a, 'info> CpiQuoterExecutor<'a, 'info> {
    /// The slab, or the error every leg gives when the route carried none.
    fn slab(&self) -> VelocityResult<&'a AccountLoader<'info, QuoterSlabV0>> {
        self.slab.ok_or_else(|| {
            msg!("router executor holds no quoter slab");
            ErrorCode::DefaultError
        })
    }

    /// Book `index`'s slab slot.
    fn slot_index(&self, index: usize) -> VelocityResult<usize> {
        self.slots.get(index).copied().ok_or_else(|| {
            msg!("router executor index {} out of range", index);
            ErrorCode::DefaultError
        })
    }

    /// Read one thing off book `index`'s slot. A short borrow — the slot
    /// region is free again before any CPI leg runs.
    fn read<T>(&self, index: usize, read: impl FnOnce(&QuoterSlotV0) -> T) -> Option<T> {
        let slot = *self.slots.get(index)?;
        let slots = self.slab?.slots().ok()?;
        slots.get(slot).map(read)
    }
}

impl<'info> ExternalQuoterExecutor<'info> for CpiQuoterExecutor<'_, 'info> {
    fn quoter_type(&self, index: usize) -> QuoterType {
        self.read(index, |slot| slot.config.quoter_type)
            .unwrap_or(QuoterType::Custom)
    }

    fn quoter_user(&self, index: usize) -> Pubkey {
        self.read(index, |slot| slot.config.user)
            .unwrap_or_default()
    }

    fn quoter_key(&self, index: usize) -> Pubkey {
        self.read(index, |slot| slot.entry).unwrap_or_default()
    }

    fn oracle_band(&self, index: usize, market_margin_ratio_initial: u32) -> u32 {
        self.read(index, |slot| {
            slot.config.oracle_band(market_margin_ratio_initial)
        })
        .unwrap_or(market_margin_ratio_initial)
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
        let slab = self.slab()?;
        let slot = self.slot_index(index)?;
        let slots = slab.slots().map_err(|_| {
            msg!("router executor failed to load the quoter slab");
            ErrorCode::DefaultError
        })?;
        let config = &slots[slot].config;
        // The book's own `quote_v0`, with the identities and budgets the
        // execute below carries. Quote and execute are held to spending the
        // same set the same way, so a ladder taken here is the one that
        // execute fills — which is what makes it a bound the fill can be
        // checked against.
        // Quoted once here, so the pool is the list this returns.
        let mut levels = Vec::new();
        Ok(Some(
            config
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
                        taker_served_window: self.taker_served_window,
                    },
                    slab,
                    self.accounts,
                    self.scratch,
                    &mut levels,
                )
                .map_err(|_| {
                    msg!("clob quote for entry {} failed", slots[slot].entry);
                    ErrorCode::DefaultError
                })
                .map(|_| levels)?,
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
        let slab = self.slab()?;
        let slot = self.slot_index(index)?;
        let (book, program) = {
            let slots = slab.slots().map_err(|_| {
                msg!("router executor failed to load the quoter slab");
                ErrorCode::DefaultError
            })?;
            let config = &slots[slot].config;
            let book = find_account(self.accounts, &config.response_account).ok_or_else(|| {
                msg!(
                    "clob book {} missing from the account map",
                    config.response_account
                );
                ErrorCode::DefaultError
            })?;
            let program = find_account(self.accounts, &config.program_id).ok_or_else(|| {
                msg!(
                    "clob program {} missing from the account map",
                    config.program_id
                );
                ErrorCode::DefaultError
            })?;
            (book, program)
        };
        let clob = ClobMarket::from_slab(slab, self.market_index, book, program)
            .map_err(|_| ErrorCode::DefaultError)?;
        clob.cancel_all(ClobCancelAllArgsV0 {
            user,
            sides,
            force: false,
        })
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
        let slab = self.slab()?;
        let slot = self.slot_index(index)?;
        let slots = slab.slots().map_err(|_| {
            msg!("router executor failed to load the quoter slab");
            ErrorCode::DefaultError
        })?;
        let config = &slots[slot].config;
        config
            .execute(
                self.market_index,
                ExecuteArgsV0 {
                    caps: self.caps,
                    reference_price: self.reference_price,
                    direction,
                    size,
                    users: self.users,
                    taker: Some(self.taker),
                    taker_served_window: self.taker_served_window,
                },
                slab,
                self.accounts,
                self.scratch,
            )
            .map_err(|e| {
                // Name the entry, not just the failure. A quoter program
                // serves many registry entries, so the program id an
                // off-chain router reads out of the runtime's CPI brackets
                // does not identify which entry failed. The key does.
                msg!("quoter {} execute failed: {}", slots[slot].entry, e);
                ErrorCode::DefaultError
            })
    }
}
