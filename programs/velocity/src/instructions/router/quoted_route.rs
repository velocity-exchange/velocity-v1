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
//! be returning references to its own locals, so the caller keeps the route
//! alive and takes the fill's books from it ([`RouteQuote::books`]).

use {
    super::{cpi_executor::CpiQuoterExecutor, user_caps::SizedQuote},
    crate::{
        controller::position::PositionDirection,
        error::ErrorCode,
        math::router::{QuoterBook, RouterLeg},
        state::{
            order_params::{RouteDigest, NO_ROUTE_DIGEST},
            prop_amm::{
                slot_for_entry, usable_levels, ClobUserRefV0, Direction, PriceLevel, QuoteArgsV0,
                QuoterSlabExt, QuoterSlabV0, QuoterSlotV0, QuoterType, MAX_ROUTE_QUOTERS,
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

/// Cut a custom quoter's ladder to the depth this fill will settle against.
///
/// Two bounds, both the quoter's own. Its declared oracle band drops the
/// levels that price outside it, and the base its account can carry
/// truncates what is left. The band always cuts the taker-favourable end of
/// the ladder, because that is the end that prices against the maker.
///
/// A book is never trimmed here. Its makers rest depth that was
/// margin-reserved at placement and are sized one per user in the caps the
/// call carries, and the book passes over an owner out of room mid-walk. So
/// the depth behind that owner stays quoted and stays fillable, where a trim
/// out here would cut every order behind them.
///
/// Compacted in place, which the pool's own layout allows: the run is its
/// tail, so the kept levels move down over the dropped ones and the pool
/// shortens. No second list exists to disagree with this one, and no earlier
/// quoter's run moves.
fn trim_to_quoter_room(
    levels: &mut Vec<PriceLevel>,
    run: std::ops::Range<usize>,
    maker_direction: PositionDirection,
    band_oracle_price: i64,
    oracle_band: u32,
    room: u64,
) -> Result<std::ops::Range<usize>> {
    let start = run.start;
    let mut kept = start;
    let mut remaining = room;
    for source in run {
        if remaining == 0 {
            break;
        }
        let level = levels[source];
        if crate::math::orders::limit_price_breaches_maker_oracle_price_bands(
            level.price,
            maker_direction,
            band_oracle_price,
            oracle_band,
        )? {
            continue;
        }
        let size = level.size.min(remaining);
        remaining -= size;
        levels[kept] = PriceLevel {
            price: level.price,
            size,
        };
        kept += 1;
    }
    levels.truncate(kept);
    Ok(start..kept)
}

/// The market's slab, found on the account tail.
///
/// `None` when the tail carries none, which is a fill that consults nothing
/// external. A slab for another market is refused rather than passed over:
/// the caller named it, so it meant to route on it.
///
/// One finder, because the sizing that runs before a quote and the route
/// that takes the quote must agree on which account they mean. Two rules
/// would let a tail size one slab and quote another.
pub fn route_slab<'info>(
    tail: &'info [AccountInfo<'info>],
    market_index: u16,
) -> Result<Option<AccountLoader<'info, QuoterSlabV0>>> {
    for info in tail {
        if !is_quoter_slab(info) {
            continue;
        }
        let loader = AccountLoader::<QuoterSlabV0>::try_from(info)?;
        validate!(
            loader.load()?.market == market_index,
            ErrorCode::DefaultError,
            "quoter slab {} is for market {}, fill is for market {}",
            loader.key(),
            loader.load()?.market,
            market_index
        )?;
        return Ok(Some(loader));
    }
    Ok(None)
}

/// Whether a claimed route entry is one the fill wrongly left out.
///
/// `slot` is what the slab says about the entry: the slot it sits in and
/// whether that slot can quote, or `None` when the entry was never approved.
///
/// Three of the four answers are "no", each for its own reason. An entry the
/// route consulted was carried. An entry the slab never approved cannot be
/// consulted and never could have been, so a route signed against a quoter
/// that was later revoked still fills. An entry whose slot cannot quote
/// would have offered nothing had it been carried, which is the same reason:
/// a route signed before an admin pulled a quoter must not brick the fill.
///
/// What is left — approved, able to quote, and left out — is the omission
/// this rule exists to catch.
fn claimed_entry_is_omitted(slot: Option<(usize, bool)>, consulted: &[usize]) -> bool {
    match slot {
        None => false,
        Some((index, quotes)) => quotes && !consulted.contains(&index),
    }
}

/// What one slot answered, before the route keeps any of it.
struct SlotQuote {
    /// The levels, as a run in the route's own level pool.
    ladder: crate::state::prop_amm::QuotedLadderV0,
    quoter_type: QuoterType,
    /// The band this quoter declared, or the market's initial margin ratio
    /// when it declared none.
    oracle_band: u32,
}

/// What a router fill needs beyond the quoted route itself.
pub struct FillerStanding {
    /// `State::signer`, the authority of the protocol `User`, which no quoter
    /// may name as a fill subject.
    pub protocol_authority: Pubkey,
    /// What the fill knows about the party that built the transaction.
    pub obligation: crate::math::router::FillerObligation,
    /// Whether the caller opens and closes the taker's whole exposure inside
    /// one instruction and asserts the end state itself.
    pub taker_exposure_closed_by_caller: bool,
}

impl<'info> RouteQuote<'_, 'info> {
    /// The books this route quoted, in the form the fill reads them, with the
    /// leg that executes what lands on them.
    ///
    /// A second step rather than part of [`quote_route`], because this
    /// borrows what quoting returned: the books point at the route's level
    /// pool, and the executor borrows the route and the CPI scratch. One
    /// function cannot return both the route and a value that points into it,
    /// so the caller holds the route and takes the books from it.
    pub fn books<'a>(
        &'a self,
        clock: &Clock,
        scratch: &'a mut crate::state::prop_amm::QuoterCpiScratch<'info>,
    ) -> Result<QuotedBooks<'a, 'info>> {
        let mut books =
            [crate::math::router::QuoterBook::default(); crate::state::prop_amm::MAX_ROUTE_QUOTERS];
        let written = self.route.write_books(&mut books)?;
        Ok(QuotedBooks {
            books,
            written,
            executor: self
                .route
                .executor(&self.sized, clock.slot, clock.unix_timestamp, scratch),
        })
    }
}

/// What a router fill executes against: the route's books, reshaped into the
/// router's own list, and the execute leg for the allocations that land on
/// them. Books and executor share indexing, so they are one value.
///
/// The caller holds it for the length of the fill, because the fill's inputs
/// borrow it ([`Self::for_fill`]).
pub struct QuotedBooks<'a, 'info> {
    books: [QuoterBook<'a>; MAX_ROUTE_QUOTERS],
    /// Books the route wrote. The rest of the array is the default book,
    /// which quotes nothing.
    written: usize,
    executor: CpiQuoterExecutor<'a, 'info>,
}

impl<'a, 'info> QuotedBooks<'a, 'info> {
    /// The router leg the fill runs: these books and their execute leg, plus
    /// what the fill needs beyond the route itself.
    ///
    /// The leg borrows the books rather than taking them, because it points
    /// into them. They outlive it, and the caller reads
    /// [`crate::math::router::RouterLeg::worst_fill_price`] back off the leg
    /// once the fill returns.
    pub fn for_fill<'b>(&'b mut self, standing: FillerStanding) -> RouterLeg<'b, 'a, 'info> {
        RouterLeg {
            books: &self.books[..self.written],
            executor: &mut self.executor,
            standing,
            worst_fill_price: None,
        }
    }
}

/// The route an order was signed with: the quoters it named, and the digest
/// the order carries so a filler cannot substitute a different list.
#[derive(Clone, Copy)]
pub struct RouteClaim<'a> {
    pub quoters: &'a [Pubkey],
    pub digest: RouteDigest,
}

/// What the route quoted for one fill, and the sized inputs it was quoted
/// from.
pub struct RouteQuote<'a, 'info> {
    pub route: QuotedRoute<'info>,
    pub sized: SizedQuote<'a, 'info>,
    /// Consulted quoters the order's signed route did not name, which arms
    /// the filler obligation. Zero when the order carries no route.
    pub unrouted_quoters: usize,
}

/// Size every counterparty this fill may settle against, then quote the
/// route against those numbers.
///
/// The only way to a [`QuotedRoute`]. `QuotedRoute::assemble` is private, so
/// a route cannot be quoted from inputs whose caps were never priced, nor
/// used without its baseline and its signer's claim checked — rules six call
/// sites each had to remember.
///
/// `claim` is an argument rather than part of the inputs because it is not
/// something a quoter is told. It is read here and not kept, so a caller can
/// name the order it came from and still mutate that order during the fill.
pub fn quote_route<'a, 'info>(
    tail: &'info [AccountInfo<'info>],
    inputs: QuoteInputs<'a>,
    claim: Option<RouteClaim<'_>>,
    ctx: &mut super::user_caps::CapInputs<'_, 'info>,
    scratch: &mut crate::state::prop_amm::QuoterCpiScratch<'info>,
) -> Result<RouteQuote<'a, 'info>> {
    let clob_market = ctx
        .maps
        .perp_market_map
        .get_ref(&inputs.market_index)?
        .clob_market;
    let sized = super::user_caps::with_counterparty_room(tail, inputs, ctx)?;
    let route = QuotedRoute::assemble(tail, &sized, scratch)?;
    route.require_baseline(clob_market)?;
    let unrouted_quoters = match claim {
        Some(claim) => {
            route.require_signed_route(claim.quoters, claim.digest)?;
            route.unrouted_quoters(claim.quoters, claim.digest)?
        }
        None => 0,
    };
    Ok(RouteQuote {
        route,
        sized,
        unrouted_quoters,
    })
}

/// Why this slot offers no depth to this fill, or `None` when it quotes.
///
/// An empty reason is a slot that is simply not quoting — revoked,
/// suspended, deactivated — which needs no log line. A route signed before
/// an admin pulled a quoter must not brick the fill, so these are all
/// skips.
fn slot_offers_nothing(slot: &QuoterSlotV0, taker_served_window: bool) -> Option<&'static str> {
    if !slot.quotes() {
        return Some("non quoting slot");
    }
    // Maker priority: a book with a speed bump quotes no depth to an
    // unattested taker. The slot stays consulted — the baseline is presence,
    // and the rest leg still uses it — but it offers nothing to execute, so
    // unattested aggression rests through the activation window, where a
    // maker can reprice or cross it first.
    if matches!(slot.config.quoter_type, QuoterType::Clob)
        && !taker_served_window
        && slot.config.book_default_activation_delay_slots > 0
    {
        return Some("runs a speed bump; no depth for an unattested taker");
    }
    None
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
    /// range indexes into this. A run is always the tail of the pool at the
    /// moment its quote returns, because a quote appends.
    ///
    /// One pool, not one list per book. The ladders have to outlive the
    /// borrow each quoter wrote them in — the split reads every book at once
    /// and the execute leg then writes the very accounts the levels sit in —
    /// and velocity's heap is 32 KB and never reclaims, so a list per book
    /// is an allocation per book for the rest of the instruction. A fixed
    /// array is not an option either: the ceiling is
    /// `MAX_ROUTE_QUOTERS * MAX_LEVELS_PER_BOOK` levels, 16 KB, and the
    /// stack frame is 4 KB.
    ///
    /// Grown rather than reserved, which is deliberate and was measured. A
    /// reservation would have to be the ceiling, because nothing says how
    /// deep a quoter will answer until its CPI returns, and 16 KB reserved
    /// costs more than doubling on every route that is not at the ceiling:
    /// a book quoting 128 levels ahead of seven four-rung quoters holds
    /// 2,496 bytes and doubling allocates 6,144 against the ceiling's
    /// 16,384. Sizing the pool from the first ladder instead is worse again
    /// on that shape, which is the common one. What doubling costs is the
    /// ceiling itself: eight books at 128 levels allocate 30,720 bytes to
    /// hold 16,384, and the 14,336 left behind are not given back. Reserve
    /// the ceiling only if that shape becomes reachable in practice.
    levels: Vec<PriceLevel>,
    /// The market's slab, when the transaction carried one. The mandatory
    /// baseline and the signed route are answered from it: whether an absent
    /// quoter *could* have quoted is a fact about the approved set.
    slab: Option<AccountLoader<'info, QuoterSlabV0>>,
    /// Every consulted slab slot, quoting or not: a slot that was skipped
    /// (dead, reading another's response, speed-bumped) still counts as
    /// consulted for the
    /// signed route and the baseline, which is what separates this from
    /// [`Self::quoted_slots`].
    ///
    /// Slot indexes rather than entry keys. `consulted_slots` allocates this
    /// list to find them, so keeping it costs nothing, where a list of the
    /// entries behind them is a second copy of what the slab already holds.
    consulted: Vec<usize>,
}

/// What quoting needs: the taker's side and size, plus the identities
/// forwarded on the wire.
pub struct QuoteInputs<'a> {
    pub market_index: u16,
    pub direction: Direction,
    pub size: u64,
    /// The loaded-user set quoters must not fill outside of.
    pub users: &'a [ClobUserRefV0],
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
    /// The market's initial margin ratio, which a quoter's declared oracle
    /// band defaults to when it sets none.
    pub margin_ratio_initial: u32,
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

impl QuoteInputs<'_> {
    /// The side the quoters rest on: the opposite of the taker's.
    pub fn maker_direction(&self) -> PositionDirection {
        match self.direction {
            Direction::Long => PositionDirection::Short,
            Direction::Short => PositionDirection::Long,
        }
    }
}

impl<'info> QuotedRoute<'info> {
    /// Find the market's slab among the leftover accounts, then quote every
    /// consulted slot on it.
    ///
    /// Suspended or deactivated slots are skipped rather than rejected: a
    /// route signed before an admin pulled a quoter must not brick the fill.
    fn assemble(
        tail: &'info [AccountInfo<'info>],
        sized: &SizedQuote<'_, 'info>,
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
            consulted: Vec::new(),
        };
        // Handed in rather than found again: the caller located it to size
        // this fill's counterparties, and the scan is over the whole tail.
        route.slab = sized.slab.clone();
        let Some(slab) = sized.slab.clone() else {
            return Ok(route);
        };

        route.consulted = slab.consulted_slots(tail)?;

        // By position, because `consulted` stays on the route for the
        // baseline and signed-route rules and cannot be borrowed across a
        // call that takes the route mutably.
        for position in 0..route.consulted.len() {
            let index = route.consulted[position];
            route.quote_slot(&slab, index, sized, scratch)?;
        }
        Ok(route)
    }

    /// Quote one consulted slot and record what it answered.
    ///
    /// A slot that offers nothing is passed over, not refused: the route
    /// still counts it as consulted, so the baseline and the signed-route
    /// rules see it, and the fill's other sources stand.
    fn quote_slot(
        &mut self,
        slab: &AccountLoader<'info, QuoterSlabV0>,
        index: usize,
        sized: &SizedQuote<'_, 'info>,
        scratch: &mut crate::state::prop_amm::QuoterCpiScratch<'info>,
    ) -> Result<()> {
        let Some(quoted) = self.take_quote(slab, index, sized, scratch)? else {
            return Ok(());
        };
        self.record_quote(index, sized, quoted)
    }

    /// CPI the slot's quoter, or `None` when the slot offers nothing.
    ///
    /// The levels land in [`Self::levels`], as the run `SlotQuote::ladder`
    /// names. Nothing else is written until [`Self::record_quote`] accepts
    /// them, so a slot that errors leaves the route as it found it.
    fn take_quote(
        &mut self,
        slab: &AccountLoader<'info, QuoterSlabV0>,
        index: usize,
        sized: &SizedQuote<'_, 'info>,
        scratch: &mut crate::state::prop_amm::QuoterCpiScratch<'info>,
    ) -> Result<Option<SlotQuote>> {
        let inputs = &sized.inputs;
        let slots = slab.slots()?;
        let slot = &slots[index];
        if let Some(reason) = slot_offers_nothing(slot, inputs.taker_served_window) {
            if !reason.is_empty() {
                msg!("quoter {}: {}", slot.entry, reason);
            }
            return Ok(None);
        }
        let located = slot.config.quote_in_place(
            inputs.market_index,
            QuoteArgsV0 {
                caps: sized.caps,
                reference_price: inputs.reference_price,
                direction: inputs.direction,
                size: inputs.size,
                users: inputs.users,
                taker: Some(inputs.taker),
                limit_price: inputs.limit_price,
                taker_served_window: inputs.taker_served_window,
                consume_reservation: inputs.consume_reservation,
            },
            slab,
            self.accounts,
            scratch,
        )?;
        let quoter_type = slot.config.quoter_type;
        let oracle_band = slot.config.oracle_band(inputs.margin_ratio_initial);
        drop(slots);

        // The copy this fill earns, made where the pool lives. The split
        // reads every book at once and the execute leg then writes the very
        // account these levels sit in, so the ladder cannot stay where the
        // quoter wrote it.
        let data = located.borrow()?;
        let response = located.checked_quote_response(&data, inputs.direction)?;
        let ladder = self.append_ladder(usable_levels(response.levels), response.withheld);
        Ok(Some(SlotQuote {
            ladder,
            quoter_type,
            oracle_band,
        }))
    }

    /// Cut one custom quoter's run down to what this fill will settle.
    ///
    /// The rule is [`trim_to_quoter_room`], which stays a free function so it
    /// can be tested without a route; this binds it to the pool it rewrites.
    fn trim_ladder(
        &mut self,
        run: std::ops::Range<usize>,
        sized: &SizedQuote<'_, 'info>,
        oracle_band: u32,
        index: usize,
    ) -> Result<std::ops::Range<usize>> {
        trim_to_quoter_room(
            &mut self.levels,
            run,
            sized.inputs.maker_direction(),
            sized.inputs.reference_price,
            oracle_band,
            sized.rooms.room(index),
        )
    }

    /// Put one quoter's levels in the pool and describe where they landed.
    fn append_ladder(
        &mut self,
        levels: &[PriceLevel],
        withheld: PriceLevel,
    ) -> crate::state::prop_amm::QuotedLadderV0 {
        let start = self.levels.len();
        self.levels.extend_from_slice(levels);
        crate::state::prop_amm::QuotedLadderV0 {
            levels: start..self.levels.len(),
            withheld,
        }
    }

    /// Keep what one slot quoted, cut to what this fill will settle.
    fn record_quote(
        &mut self,
        index: usize,
        sized: &SizedQuote<'_, 'info>,
        quoted: SlotQuote,
    ) -> Result<()> {
        let SlotQuote {
            ladder,
            quoter_type,
            oracle_band,
        } = quoted;

        // A custom quoter's ladder is cut before anything else reads it. One
        // trim, one ladder: the split allocates and the settle checks
        // against the same levels, which two lists cannot promise.
        let levels = if quoter_type == QuoterType::Custom {
            self.trim_ladder(ladder.levels, sized, oracle_band, index)?
        } else {
            ladder.levels
        };

        // Only a book can withhold. A book walks the orders of many owners
        // and stops at one this transaction cannot settle for. Every other
        // quoter fills from the single `user` in its own registry slot,
        // which a fill either carries or does not quote at all, so there is
        // no owner for it to stop at.
        //
        // Dropped here rather than trusted and checked later: the report
        // arms the filler obligation, so a quoter that set it would fail
        // fills that carried it and the error would name the filler. Zeroing
        // it at the source leaves nothing to report and no consumer to
        // remember the rule.
        let withheld = if quoter_type == QuoterType::Clob {
            ladder.withheld
        } else {
            PriceLevel::default()
        };
        self.ladders
            .push(crate::state::prop_amm::QuotedLadderV0 { levels, withheld });
        self.quoted_slots.push(index);
        Ok(())
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
        let Some((index, slot)) = book else {
            return Ok(());
        };
        validate!(
            !slot.quotes() || self.consulted.contains(&index),
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
    ///
    /// The slab resolves each consulted slot to the entry the claim names it
    /// by. A route with no slab consulted nothing, so it has nothing
    /// unrouted.
    pub fn unrouted_quoters(&self, claimed: &[Pubkey], digest: RouteDigest) -> Result<usize> {
        if digest == NO_ROUTE_DIGEST {
            return Ok(0);
        }
        let Some(slab) = &self.slab else {
            return Ok(0);
        };
        let slots = slab.slots()?;
        Ok(self
            .consulted
            .iter()
            .filter(|&&index| !claimed.contains(&slots[index].entry))
            .count())
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
            // One lookup answers both halves: whether the route consulted
            // this entry, and — when it did not — whether the entry could
            // have quoted at all.
            let omitted = match &self.slab {
                Some(slab) => {
                    let slots = slab.slots()?;
                    let slot =
                        slot_for_entry(&slots, entry).map(|index| (index, slots[index].quotes()));
                    claimed_entry_is_omitted(slot, &self.consulted)
                }
                // No slab in the tail: liveness cannot be answered, and a
                // fill that omits the slab omits every quoter on it — treat
                // the named quoter as live and refuse.
                None => true,
            };
            validate!(
                !omitted,
                ErrorCode::SignedRouteEntryMissing,
                "signed route names quoter {} but the fill does not consult it",
                entry
            )?;
        }
        Ok(())
    }

    /// The router's view of what quoted, written into storage the caller owns.
    /// Reports how many books it wrote, which is the length of the prefix the
    /// fill reads.
    ///
    /// Takes a buffer rather than returning one: the books are a reshape of
    /// what this struct already holds, and allocating a second list to say the
    /// same thing is the kind of cost that only looks free.
    pub fn write_books<'a>(
        &'a self,
        into: &mut [QuoterBook<'a>; MAX_ROUTE_QUOTERS],
    ) -> Result<usize> {
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
        Ok(self.quoted_slots.len())
    }

    /// The execute leg, borrowing what quoting already gathered.
    pub fn executor<'a>(
        &'a self,
        sized: &'a SizedQuote<'_, 'info>,
        slot: u64,
        now: i64,
        scratch: &'a mut crate::state::prop_amm::QuoterCpiScratch<'info>,
    ) -> CpiQuoterExecutor<'a, 'info> {
        let inputs = &sized.inputs;
        CpiQuoterExecutor {
            scratch,
            caps: sized.caps,
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

/// What the trim leaves of a custom quoter's ladder.
///
/// The run is always the tail of the level pool, because a quote appends its
/// ladder there. A leading run stands in these cases for the earlier quoter's
/// levels, and every case asserts it survives untouched.
#[cfg(test)]
mod trim_tests {
    use super::*;

    const PRICE: u64 = crate::math::constants::PRICE_PRECISION_U64;
    const BASE: u64 = crate::math::constants::BASE_PRECISION_U64;
    /// Wide enough that no level in these cases breaches it, which is what
    /// isolates the cases that are about the room. A zero band admits
    /// nothing, and production never passes one: a quoter that declares no
    /// band falls back to the market's initial margin ratio.
    const WIDE: u32 = crate::math::constants::MARGIN_PRECISION / 10;

    /// One level of an earlier quoter, which no trim may reach.
    fn pool(ladder: &[(u64, u64)]) -> (Vec<PriceLevel>, std::ops::Range<usize>) {
        let mut levels = vec![PriceLevel { price: 1, size: 1 }];
        let start = levels.len();
        levels.extend(ladder.iter().map(|(price, size)| PriceLevel {
            price: price * PRICE,
            size: size * BASE,
        }));
        let run = start..levels.len();
        (levels, run)
    }

    fn trim(
        ladder: &[(u64, u64)],
        maker_direction: PositionDirection,
        band: u32,
        room: u64,
    ) -> Vec<(u64, u64)> {
        let (mut levels, run) = pool(ladder);
        let kept = trim_to_quoter_room(
            &mut levels,
            run,
            maker_direction,
            (100 * PRICE) as i64,
            band,
            room,
        )
        .unwrap();
        assert_eq!(levels[0], PriceLevel { price: 1, size: 1 });
        assert_eq!(kept.end, levels.len(), "the run is the tail of the pool");
        levels[kept]
            .iter()
            .map(|level| (level.price / PRICE, level.size / BASE))
            .collect()
    }

    #[test]
    fn an_unbounded_room_leaves_the_ladder_alone() {
        assert_eq!(
            trim(
                &[(99, 2), (98, 3)],
                PositionDirection::Short,
                WIDE,
                u64::MAX
            ),
            [(99, 2), (98, 3)]
        );
    }

    #[test]
    fn the_room_truncates_the_far_end() {
        // Four base quoted, three afforded: the best level survives whole and
        // the next is cut to what is left. A prefix, so what the quoter keeps
        // is its own best depth.
        assert_eq!(
            trim(
                &[(99, 2), (98, 2)],
                PositionDirection::Short,
                WIDE,
                3 * BASE
            ),
            [(99, 2), (98, 1)]
        );
    }

    #[test]
    fn no_room_leaves_nothing() {
        // What a quoter quoting for the taker itself is given, so the
        // self-trade never reaches the split.
        assert!(trim(&[(99, 2)], PositionDirection::Short, WIDE, 0).is_empty());
    }

    #[test]
    fn the_band_cuts_the_end_that_prices_against_the_maker() {
        // A maker selling at 100 with a 5% band may not sell below 95. The
        // cheap ask is the taker-favourable one, and it is the one dropped;
        // the levels behind it keep their sizes and their order.
        let band = crate::math::constants::MARGIN_PRECISION / 20;
        assert_eq!(
            trim(
                &[(99, 1), (90, 1), (98, 1)],
                PositionDirection::Short,
                band,
                u64::MAX
            ),
            [(99, 1), (98, 1)]
        );
    }

    #[test]
    fn a_banded_out_level_does_not_spend_the_room() {
        // The band runs first, so a level the fill would refuse anyway costs
        // the quoter no allocation.
        let band = crate::math::constants::MARGIN_PRECISION / 20;
        assert_eq!(
            trim(
                &[(90, 5), (99, 2)],
                PositionDirection::Short,
                band,
                2 * BASE
            ),
            [(99, 2)]
        );
    }
}

/// The four answers the signed-route rule can give about one claimed entry.
#[cfg(test)]
mod claimed_entry_tests {
    use super::claimed_entry_is_omitted;

    #[test]
    fn an_entry_the_route_consulted_is_carried() {
        assert!(!claimed_entry_is_omitted(Some((3, true)), &[1, 3, 5]));
    }

    #[test]
    fn an_entry_the_slab_never_approved_is_not_an_omission() {
        // Signed against a quoter that was later revoked. Refusing here would
        // brick every fill of that order.
        assert!(!claimed_entry_is_omitted(None, &[1, 3, 5]));
    }

    #[test]
    fn an_entry_whose_slot_cannot_quote_is_not_an_omission() {
        // Suspended or deactivated: carrying it would have offered nothing.
        assert!(!claimed_entry_is_omitted(Some((7, false)), &[1, 3, 5]));
    }

    #[test]
    fn a_live_approved_entry_left_out_is_the_omission() {
        assert!(claimed_entry_is_omitted(Some((7, true)), &[1, 3, 5]));
    }
}
