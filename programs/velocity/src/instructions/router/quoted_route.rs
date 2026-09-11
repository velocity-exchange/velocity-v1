//! The quoted route of a router fill: which of the market's approved quoters
//! this transaction consults, and what each of them quoted.
//!
//! Every router fill ends with the same account tail — the market's
//! [`QuoterSlabV0`] plus, per consulted quoter, the union of its registered
//! CPI accounts (its program, its response account; the slab itself is the
//! CPI signer).
//! The slab holds every approved config; the transaction names the slots it
//! consults by *carrying their response accounts* — a slot whose response
//! account is absent is simply not consulted, and carrying one is the intent
//! to consult it (its remaining registered accounts must then be present, as
//! a CPI with a partial list would answer about the wrong thing). Quoting is
//! the same work whoever sent the transaction, a keeper cranking someone
//! else's order or a taker routing their own, so it lives here rather than in
//! one entrypoint the other cannot reach.
//!
//! The result is owned because the fill borrows from it: the executor holds
//! the quoted slots and the account tail, and the router's books point at the
//! levels each quote returned. A function that built the executor itself would
//! be returning references to its own locals, so the caller keeps this alive
//! and borrows ([`QuotedRoute::books`], [`QuotedRoute::executor`]).

use {
    super::cpi_executor::CpiQuoterExecutor,
    crate::{
        error::ErrorCode,
        math::router::QuoterBook,
        state::{
            order_params::{RouteDigest, NO_ROUTE_DIGEST},
            prop_amm::{
                slot_for_entry, ClobUserRefV0, Direction, PriceLevel, QuoteArgsV0, QuoterSlabExt,
                QuoterSlabV0, QuoterSlotV0, QuoterType, MAX_ROUTE_QUOTERS,
            },
        },
        validate,
    },
    anchor_lang::{prelude::*, Discriminator},
};

/// Whether this account is a [`QuoterSlabV0`] this program owns.
fn is_quoter_slab(info: &AccountInfo) -> bool {
    info.owner == &crate::ID
        && info
            .try_borrow_data()
            .is_ok_and(|data| data.get(..8) == Some(QuoterSlabV0::DISCRIMINATOR))
}

/// Whether this slot's CPI account list names another consulted quoter in the
/// transaction.
///
/// Its own three keys are not rivals: a slot legitimately carries its own
/// program and its own response account — that last one is where velocity
/// reads the answer from, and approval requires it.
fn quoter_reads_a_rival(slot: &QuoterSlotV0, rivals: &[Pubkey]) -> bool {
    let own = [
        slot.entry,
        slot.config.program_id,
        slot.config.response_account,
    ];
    slot.config
        .registered_accounts()
        .iter()
        .any(|meta| !own.contains(&meta.pubkey) && rivals.contains(&meta.pubkey))
}

pub struct QuotedRoute<'info> {
    /// The account tail, borrowed straight from the instruction's remaining
    /// accounts. A quoter's registered account list is resolved against this by
    /// scanning it — nothing is cloned and no index is built.
    pub accounts: &'info [AccountInfo<'info>],
    /// The slab slots that quoted, by index, in slab order. Slots that quote
    /// nothing (suspended, deactivated, skipped) are absent.
    pub quoted_slots: Vec<usize>,
    /// What each quoted slot answered, aligned with `quoted_slots`: the
    /// slot's run in `levels`, and the depth it withheld.
    ladders: Vec<crate::state::prop_amm::QuotedLadderV0>,
    /// Every slot's quoted levels, one run after another; each ladder's
    /// range indexes into this.
    levels: Vec<PriceLevel>,
    /// The market's slab, when the transaction carried one. The mandatory
    /// baseline and the signed route are answered from it: whether an absent
    /// quoter *could* have quoted is a fact about the approved set.
    slab: Option<AccountLoader<'info, QuoterSlabV0>>,
    /// Every consulted slot's entry, quoting or not: a carried slot that was
    /// skipped (dead, rival-reading, speed-bumped) still counts as consulted
    /// for the signed route and the baseline. A fixed array: bounded by the
    /// same budget the transaction is, so it needs no allocation.
    carried: [Pubkey; MAX_ROUTE_QUOTERS],
    carried_len: usize,
}

/// What quoting needs: the taker's side and size, plus the identities
/// forwarded on the wire.
pub struct QuoteInputs<'a> {
    pub market_index: u16,
    pub direction: Direction,
    pub size: u64,
    /// The loaded-user set quoters must not fill outside of.
    pub users: &'a [ClobUserRefV0],
    /// Per-user room, carried here so the quote and the execute that binds to
    /// it cannot be given different numbers: the executor is built from these
    /// same inputs, so the two walks skip identically by construction.
    pub caps: crate::state::prop_amm::QuoterUserCapsV0,
    /// The mark a quoter prices a capped maker's loss against. The quote and
    /// the execute must be handed the same one, or a quoter that spends
    /// budgets passes over a different set of orders than it quoted.
    pub reference_price: i64,
    pub taker: ClobUserRefV0,
    /// The worst price this fill will accept, or zero for no bound. A quoter
    /// that honours it stops its walk where the router would have discarded
    /// the rest. Advisory: see [`QuoteArgsV0::limit_price`].
    pub limit_price: u64,
    /// Whether the taker's flow served a protection window: the swift hold
    /// (the flow authority signed a swift-built transaction as a named
    /// account, or signed a detached attestation over the order's own
    /// signature for a keeper-built fill), or the book's
    /// activation delay (a protocol crank fills an order that rested
    /// through it, so the cranks pass `true`). Forwarded to every quoter on
    /// the wire — a quoter that only serves protected flow trusts this the
    /// way it trusts `users` and `caps`. It also drives the maker-priority
    /// skip in [`QuotedRoute::assemble`]: a book with a nonzero default
    /// activation delay quotes no depth when this is false.
    pub taker_served_window: bool,
    /// Whether this fill settles a crossing taker remainder itself, and so
    /// reads a book with every crossing reservation ignored.
    ///
    /// A remainder claims the depth it crosses, and that depth leaves the
    /// matchable set for everyone else. Only the crank that owes the taker its
    /// improvement may take it, so only that crank passes `true`. Every other
    /// route must pass `false`, or a caller could fill the cover a remainder is
    /// waiting on and take the improvement itself.
    pub consume_reservation: bool,
}

impl<'info> QuotedRoute<'info> {
    /// Find the market's slab among the leftover accounts, then quote every
    /// consulted slot on it.
    ///
    /// Suspended or deactivated slots are skipped rather than rejected: a
    /// route signed before an admin pulled a quoter must not brick the fill.
    pub fn assemble(
        tail: &'info [AccountInfo<'info>],
        inputs: &QuoteInputs<'_>,
        scratch: &mut crate::state::prop_amm::QuoterCpiScratch<'info>,
    ) -> Result<QuotedRoute<'info>> {
        let mut route = QuotedRoute {
            accounts: tail,
            // The one heap holder left, deliberately: a fixed array of these
            // is about a kilobyte, and this fill's stack frame is four — the
            // program has overflowed it before on a struct this size.
            quoted_slots: Vec::with_capacity(MAX_ROUTE_QUOTERS),
            ladders: Vec::with_capacity(MAX_ROUTE_QUOTERS),
            levels: Vec::new(),
            slab: None,
            carried: [Pubkey::default(); MAX_ROUTE_QUOTERS],
            carried_len: 0,
        };

        // The market's slab. A tail without one consults nothing external.
        for info in tail {
            if !is_quoter_slab(info) {
                continue;
            }
            let loader = AccountLoader::<QuoterSlabV0>::try_from(info)?;
            validate!(
                loader.load()?.market == inputs.market_index,
                ErrorCode::DefaultError,
                "quoter slab {} is for market {}, fill is for market {}",
                loader.key(),
                loader.load()?.market,
                inputs.market_index
            )?;
            route.slab = Some(loader);
            break;
        }
        let Some(slab_loader) = route.slab.clone() else {
            return Ok(route);
        };

        // Collected before any quoting, so the rival check below can see
        // slots that come later in the slab.
        let consulted = slab_loader.consulted_slots(tail)?;
        let mut rivals = [Pubkey::default(); MAX_ROUTE_QUOTERS * 3];
        let mut rival_len = 0usize;
        {
            let slots = slab_loader.slots()?;
            for &index in &consulted {
                let slot = &slots[index];
                route.carried[route.carried_len] = slot.entry;
                route.carried_len += 1;
                for key in [
                    slot.entry,
                    slot.config.program_id,
                    slot.config.response_account,
                ] {
                    rivals[rival_len] = key;
                    rival_len += 1;
                }
            }
        }

        for index in consulted {
            let (ladder, quoter_type) = {
                let slots = slab_loader.slots()?;
                let slot = &slots[index];
                if !slot.quotes() {
                    continue;
                }
                let entry_key = slot.entry;
                // A quoter never sees another quoter in this transaction.
                //
                // Quote order is slab order, so a slot placed later could
                // otherwise read a rival's response account — already
                // written, holding the ladder that rival is about to be held
                // to — and quote one tick better. That is an unbounded last
                // look. Velocity holds its own vAMM's last look to a band for
                // the same reason (`LAST_LOOK_BAND`), and a third party must
                // not get a wider one.
                //
                // Skipped, not filtered: a quoter reads its accounts by
                // position, so removing one shifts every account after it and
                // the quoter answers about the wrong thing. Skipping costs
                // only this slot's turn, and it is the slot that asked for
                // the account.
                if quoter_reads_a_rival(slot, &rivals[..rival_len]) {
                    msg!(
                        "quoter {} names another consulted quoter in its accounts; skipped",
                        entry_key
                    );
                    continue;
                }
                // Maker priority: a book with a speed bump quotes no depth
                // to an unattested taker. The slot stays consulted — the
                // baseline is presence, and the rest leg still uses it — but
                // it offers nothing to execute, so unattested aggression
                // rests through the activation window, where a maker can
                // reprice or cross it first. Skipped like a dead slot
                // rather than failing, so the fill's other sources stand.
                if slot.config.quoter_type == QuoterType::Clob
                    && !inputs.taker_served_window
                    && slot.config.book_default_activation_delay_slots > 0
                {
                    msg!(
                        "book {} runs a speed bump; no depth for an unattested taker",
                        entry_key
                    );
                    continue;
                }
                let levels = slot.config.quote(
                    inputs.market_index,
                    QuoteArgsV0 {
                        caps: inputs.caps,
                        reference_price: inputs.reference_price,
                        direction: inputs.direction,
                        size: inputs.size,
                        users: inputs.users,
                        taker: Some(inputs.taker),
                        limit_price: inputs.limit_price,
                        taker_served_window: inputs.taker_served_window,
                        consume_reservation: inputs.consume_reservation,
                    },
                    &slab_loader,
                    route.accounts,
                    scratch,
                    &mut route.levels,
                )?;
                (levels, slot.config.quoter_type)
            };
            // Only a book can withhold. A book walks the orders of many
            // owners and stops at one this transaction cannot settle for.
            // Every other quoter fills from the single `user` in its own
            // registry slot, which a fill either carries or does not quote
            // at all, so there is no owner for it to stop at.
            //
            // Dropped here rather than trusted and checked later: the report
            // arms the filler obligation, so a quoter that set it would fail
            // fills that carried it and the error would name the filler.
            // Zeroing it at the source leaves nothing to report and no
            // consumer to remember the rule.
            let withheld = if quoter_type == QuoterType::Clob {
                ladder.withheld
            } else {
                PriceLevel::default()
            };
            route.ladders.push(crate::state::prop_amm::QuotedLadderV0 {
                levels: ladder.levels,
                withheld,
            });
            route.quoted_slots.push(index);
        }
        Ok(route)
    }

    /// The quoters this transaction consulted, by entry address.
    pub fn carried(&self) -> &[Pubkey] {
        &self.carried[..self.carried_len]
    }

    /// A route can't exclude the public book: when the market names a
    /// canonical CLOB entry whose slab slot can quote, the transaction must
    /// consult it — which also means carrying the slab at all. A suspended or
    /// deactivated slot satisfies this without being consulted, so killing a
    /// book never bricks fills. The vAMM half of the baseline is inherent:
    /// it's in-program, gated only by oracle validity.
    pub fn require_baseline(&self, required_clob: Pubkey) -> Result<()> {
        if required_clob == Pubkey::default() {
            return Ok(());
        }
        // Without the slab a filler could dodge the book (and its withheld-
        // depth protections) by omitting one account, so naming a book makes
        // the slab mandatory.
        let Some(slab) = &self.slab else {
            msg!(
                "the market names CLOB quoter {}; the fill must carry the quoter slab",
                required_clob
            );
            return Err(ErrorCode::DefaultError.into());
        };
        let slots = slab.slots()?;
        // The market names its book by the account the book answers on, which
        // approval holds equal to that entry's response account. The slab is
        // keyed by entry, and a route reports the entries it consulted, so the
        // lookup goes through the response account to learn which entry is the
        // book. Matching the book's address against entry keys finds nothing
        // and quietly excuses every fill from the baseline.
        let book = crate::state::prop_amm::occupied_slots(&slots)
            .find(|(_, slot)| slot.config.response_account == required_clob);
        // No slot at all is a book that was never approved or was revoked,
        // which is the dead-book case: nothing to consult.
        let Some((_, slot)) = book else {
            return Ok(());
        };
        validate!(
            !slot.quotes() || self.carried().contains(&slot.entry),
            ErrorCode::DefaultError,
            "router fill must include the market's CLOB quoter {}",
            required_clob
        )?;
        Ok(())
    }

    /// Consulted quoters the signed route did not name.
    ///
    /// Zero when no route was signed: the taker named nothing, so nothing is
    /// uninvited. `require_signed_route` has already refused a claimed set
    /// that does not digest to the order's, so `claimed` here is the taker's
    /// own list.
    ///
    /// The count, not a boolean, so the error can say how many.
    pub fn unrouted_quoters(&self, claimed: &[Pubkey], digest: RouteDigest) -> usize {
        if digest == NO_ROUTE_DIGEST {
            return 0;
        }
        self.carried()
            .iter()
            .filter(|entry| !claimed.contains(entry))
            .count()
    }

    /// Hold the transaction to the route the order's signer chose.
    ///
    /// `claimed` is what the filler says the signer picked; `digest` is what
    /// the order carries. The digest check means a filler cannot substitute a
    /// route, and it covers the unrouted case for free — an empty route
    /// digests to zero, which is what a directly-placed order holds.
    ///
    /// Then every claimed entry must be **consulted**, unless its slab slot
    /// cannot quote anyway (revoked, suspended, deactivated, or never
    /// approved) — a route signed before an admin pulled a quoter must not
    /// brick the fill, and whether a dead quoter *should* have won is a
    /// question about prices, not accounts. Extra consulted quoters beyond
    /// the route are fine — the router allocates by price and an execute is
    /// bound to its own quote, so an uninvited quoter can only lose. Omitting
    /// one the taker asked for is the actual attack.
    pub fn require_signed_route(&self, claimed: &[Pubkey], digest: RouteDigest) -> Result<()> {
        validate!(
            crate::state::order_params::route_digest(claimed) == digest,
            ErrorCode::SignedRouteMismatch,
            "claimed route does not digest to the one the order was signed with"
        )?;
        for entry in claimed {
            if self.carried().contains(entry) {
                continue;
            }
            let live = match &self.slab {
                Some(slab) => {
                    let slots = slab.slots()?;
                    slot_for_entry(&slots, entry).is_some_and(|index| slots[index].quotes())
                }
                // No slab in the tail: liveness cannot be answered, and a
                // fill that omits the slab omits every quoter on it — treat
                // the named quoter as live and refuse.
                None => true,
            };
            validate!(
                !live,
                ErrorCode::SignedRouteEntryMissing,
                "signed route names quoter {} but the fill does not consult it",
                entry
            )?;
        }
        Ok(())
    }

    /// The router's view of what quoted, written into storage the caller owns.
    ///
    /// Takes a buffer rather than returning one: the books are a reshape of
    /// what this struct already holds, and allocating a second list to say the
    /// same thing is the kind of cost that only looks free.
    pub fn books<'a>(
        &'a self,
        into: &'a mut [QuoterBook<'a>; MAX_ROUTE_QUOTERS],
    ) -> Result<&'a [QuoterBook<'a>]> {
        if let Some(slab) = &self.slab {
            let slots = slab.slots()?;
            for (book, (&index, ladder)) in into
                .iter_mut()
                .zip(self.quoted_slots.iter().zip(self.ladders.iter()))
            {
                *book = QuoterBook {
                    priority: slots[index].config.priority,
                    levels: &self.levels[ladder.levels.clone()],
                    withheld: ladder.withheld,
                };
            }
        }
        Ok(&into[..self.quoted_slots.len()])
    }

    /// The execute leg, borrowing what quoting already gathered.
    pub fn executor<'a>(
        &'a self,
        inputs: &'a QuoteInputs<'_>,
        slot: u64,
        now: i64,
        scratch: &'a mut crate::state::prop_amm::QuoterCpiScratch<'info>,
    ) -> CpiQuoterExecutor<'a, 'info> {
        CpiQuoterExecutor {
            scratch,
            caps: inputs.caps,
            reference_price: inputs.reference_price,
            slab: self.slab.as_ref(),
            slots: &self.quoted_slots,
            market_index: inputs.market_index,
            accounts: self.accounts,
            users: inputs.users,
            taker: inputs.taker,
            taker_served_window: inputs.taker_served_window,
            consume_reservation: inputs.consume_reservation,
            slot,
            now,
        }
    }
}
