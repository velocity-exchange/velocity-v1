//! The quoted route of a router fill: the registry entries a transaction
//! carries, and what each of them quoted.
//!
//! Every router fill ends with the same account tail — the `QuoterV0` entries
//! the caller wants consulted, plus the union of their registered CPI accounts
//! (each quoter's program, its response account, the quoter CPI signer).
//! Quoting them is the same work whoever sent the transaction, a keeper
//! cranking someone else's order or a taker routing their own, so it lives here
//! rather than in one entrypoint the other cannot reach.
//!
//! The result is owned because the fill borrows from it: the executor holds the
//! quoted entries and the account tail, and the router's books point at the
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
            prop_amm::{ClobUserRefV0, Direction, PriceLevel, QuoteArgsV0, QuoterType, QuoterV0},
        },
        validate,
    },
    anchor_lang::{prelude::*, Discriminator},
};

/// Whether this account is a `QuoterV0` this program owns.
///
/// One predicate for both passes: the second pass quotes what the first pass
/// counted, and two spellings of "is an entry" would let one see a quoter the
/// other missed.
fn is_quoter_entry(info: &AccountInfo) -> bool {
    info.owner == &crate::ID
        && info
            .try_borrow_data()
            .is_ok_and(|data| data.get(..8) == Some(QuoterV0::DISCRIMINATOR))
}

/// Whether this entry's CPI account lists name another quoter in the
/// transaction.
///
/// Its own three keys are not rivals: an entry legitimately carries its own
/// entry account, its own program, and its own response account — that last one
/// is where velocity reads the answer from, and approval requires it.
fn quoter_reads_a_rival(quoter: &QuoterV0, entry_key: &Pubkey, rivals: &[Pubkey]) -> bool {
    let own = [*entry_key, quoter.program_id, quoter.response_account];
    quoter.quote_accounts[..quoter.quote_accounts_count as usize]
        .iter()
        .chain(quoter.execute_accounts[..quoter.execute_accounts_count as usize].iter())
        .any(|meta| !own.contains(&meta.pubkey) && rivals.contains(&meta.pubkey))
}

/// Quoter entries one transaction may carry.
///
/// Derived from the account-lock budget rather than chosen: a fill spends
/// roughly 15 locks before its first quoter, and each quoter costs three more
/// that nothing else shares — its registry entry, its program, and its response
/// account — against the 64 a transaction can name. Eight leaves room for the
/// maker accounts a fill also carries. A transaction carrying more fails
/// loudly; it could not have paid for them anyway.
pub const MAX_ROUTE_QUOTERS: usize = 8;

/// One entry that quoted, with everything the fill later needs of it.
///
/// One record per entry rather than a column per field: the fill indexes these
/// by book, and holding the fields apart made that alignment a promise in a
/// comment instead of a property of the type.
pub struct QuotedEntry<'info> {
    pub entry: AccountLoader<'info, QuoterV0>,
    /// Captured at quote time so the fill can ask without re-loading the entry.
    pub quoter_type: QuoterType,
    /// The registry `user`: the margin account the pre-execute clamp sizes a
    /// Custom book against, and the only subject its response may name.
    pub user: Pubkey,
    /// For a CLOB entry this is the book, which is where the entry's permitted
    /// subjects are read from.
    pub response_account: Pubkey,
    /// Routing tier at a shared price: lower fills first, pro rata within.
    pub priority: u8,
    /// The entry's declared oracle band, captured with the rest. Zero means it
    /// declared none — see [`QuoterV0::max_oracle_deviation_bps`].
    pub max_oracle_deviation_bps: u32,
    /// What it quoted, best price first, as a run in [`QuotedRoute::levels`].
    /// Held past the quoting CPI because every later allocation and price
    /// check is measured against it, but pooled with every other entry's run
    /// so a route costs one allocation rather than one per book.
    pub levels: core::ops::Range<usize>,
    /// Depth it says it holds at a better price than it quoted, and could not
    /// offer because this transaction does not carry the accounts of the user
    /// who owns it. A zero price means it reached everything it was asked
    /// for. Never fillable — it is the number that keeps a worse-priced
    /// source from taking what the book was standing on.
    pub withheld: PriceLevel,
}

pub struct QuotedRoute<'info> {
    /// The account tail, borrowed straight from the instruction's remaining
    /// accounts. A quoter's registered account list is resolved against this by
    /// scanning it — nothing is cloned and no index is built.
    pub accounts: &'info [AccountInfo<'info>],
    /// The entries that quoted, in book order. Inactive or unapproved entries
    /// are absent — they were carried and skipped.
    pub quoted: Vec<QuotedEntry<'info>>,
    /// Every entry's quoted levels, one run after another.
    /// [`QuotedEntry::levels`] indexes into this.
    levels: Vec<PriceLevel>,
    /// Every entry the caller passed, quoting or not. The mandatory baseline
    /// and the signed route are checked against this, because a dead entry
    /// still satisfies both. A fixed array: bounded by the same budget the
    /// transaction is, so it needs no allocation.
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
    /// The CLOB place authority and its bump — the identity a `Clob` entry's
    /// CPI legs are signed as. Passed in rather than derived because the
    /// entrypoint's named account already carries it, bump included. Every
    /// other entry signs as a key derived from its own registry entry
    /// (`QuoterV0::cpi_signer`), so no quoter ever holds a signature that
    /// authenticates at a book or at another quoter.

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
}

impl<'info> QuotedRoute<'info> {
    /// Classify the leftover accounts, then quote every live entry among them.
    ///
    /// Deactivated or unapproved entries are skipped rather than rejected: a
    /// route signed before an admin pulled approval must not brick the fill. A
    /// market mismatch or a duplicate is a malformed transaction and fails
    /// loudly.
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
            quoted: Vec::with_capacity(MAX_ROUTE_QUOTERS),
            levels: Vec::new(),
            carried: [Pubkey::default(); MAX_ROUTE_QUOTERS],
            carried_len: 0,
        };

        // Every quoter in this transaction, by the three keys that identify one:
        // its entry, its program, and the account it writes its response into.
        // Collected before any quoting so the check below can see entries that
        // come later in the tail.
        let mut rivals = [Pubkey::default(); MAX_ROUTE_QUOTERS * 3];
        let mut rival_len = 0usize;
        for info in tail {
            if !is_quoter_entry(info) {
                continue;
            }
            let Ok(loader) = AccountLoader::<QuoterV0>::try_from(info) else {
                continue;
            };
            let Ok(quoter) = loader.load() else {
                continue;
            };
            for key in [loader.key(), quoter.program_id, quoter.response_account] {
                if rival_len < rivals.len() {
                    rivals[rival_len] = key;
                    rival_len += 1;
                }
            }
        }

        for info in tail {
            if !is_quoter_entry(info) {
                continue;
            }
            let loader = AccountLoader::<QuoterV0>::try_from(info)?;
            let quoted = {
                let quoter = loader.load()?;
                validate!(
                    quoter.market == inputs.market_index,
                    ErrorCode::DefaultError,
                    "quoter entry {} is for market {}, fill is for market {}",
                    loader.key(),
                    quoter.market,
                    inputs.market_index
                )?;
                validate!(
                    quoter.quoter_type != QuoterType::Vamm,
                    ErrorCode::DefaultError,
                    "the vAMM quotes in-program, not through the registry"
                )?;
                validate!(
                    !route.carried().contains(&loader.key()),
                    ErrorCode::DefaultError,
                    "duplicate quoter entry {}",
                    loader.key()
                )?;
                validate!(
                    route.carried_len < MAX_ROUTE_QUOTERS,
                    ErrorCode::DefaultError,
                    "a fill may carry at most {} quoter entries",
                    MAX_ROUTE_QUOTERS
                )?;
                route.carried[route.carried_len] = loader.key();
                route.carried_len += 1;
                if !(quoter.is_active && quoter.is_approved) {
                    continue;
                }
                let entry_key = loader.key();
                // A quoter never sees another quoter in this transaction.
                //
                // Quote order is the order the caller passed the accounts, so
                // a quoter placed last could otherwise read a rival's response
                // account — already written, holding the ladder that rival is
                // about to be held to — and quote one tick better. That is an
                // unbounded last look. Velocity holds its own vAMM's last look
                // to a band for the same reason (`LAST_LOOK_BAND`), and a
                // third party must not get a wider one.
                //
                // Skipped, not filtered: a quoter reads its accounts by
                // position, so removing one shifts every account after it and
                // the quoter answers about the wrong thing. Skipping costs
                // only this entry's turn, and it is the entry that asked for
                // the account.
                if quoter_reads_a_rival(&quoter, &entry_key, &rivals[..rival_len]) {
                    msg!(
                        "quoter {} names another carried quoter in its accounts; skipped",
                        entry_key
                    );
                    continue;
                }
                // Maker priority: a book with a speed bump quotes no depth
                // to an unattested taker. The entry stays carried — the
                // baseline is presence, and the rest leg still uses it — but
                // it offers nothing to execute, so unattested aggression
                // rests through the activation window, where a maker can
                // reprice or cross it first. Skipped like a dead entry
                // rather than failing, so the fill's other sources stand.
                if quoter.quoter_type == QuoterType::Clob
                    && !inputs.taker_served_window
                    && quoter.book_default_activation_delay_slots > 0
                {
                    msg!(
                        "book {} runs a speed bump; no depth for an unattested taker",
                        entry_key
                    );
                    continue;
                }
                let (cpi_signer, cpi_signer_nonce) = quoter.cpi_signer(&entry_key);
                let levels = quoter.quote(
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
                    },
                    &entry_key,
                    &cpi_signer,
                    cpi_signer_nonce,
                    route.accounts,
                    scratch,
                    &mut route.levels,
                )?;
                (
                    quoter.quoter_type,
                    quoter.user,
                    quoter.response_account,
                    quoter.priority,
                    quoter.max_oracle_deviation_bps,
                    levels,
                )
            };
            let (quoter_type, user, response_account, priority, max_oracle_deviation_bps, quoted) =
                quoted;
            route.quoted.push(QuotedEntry {
                entry: loader,
                quoter_type,
                user,
                response_account,
                priority,
                max_oracle_deviation_bps,
                levels: quoted.levels.clone(),
                // Only a book can withhold. A book walks the orders of many
                // owners and stops at one this transaction cannot settle for.
                // Every other quoter fills from the single `user` in its own
                // registry entry, which a fill either carries or does not
                // quote at all, so there is no owner for it to stop at.
                //
                // Dropped here rather than trusted and checked later: the
                // report arms the filler obligation, so a quoter that set it
                // would fail fills that carried it and the error would name
                // the filler. Zeroing it at the source leaves nothing to
                // report and no consumer to remember the rule.
                withheld: if quoter_type == QuoterType::Clob {
                    quoted.withheld
                } else {
                    PriceLevel::default()
                },
            });
        }
        Ok(route)
    }

    /// The entries this transaction carried.
    pub fn carried(&self) -> &[Pubkey] {
        &self.carried[..self.carried_len]
    }

    /// A route can't exclude the public book: when the market names a
    /// canonical CLOB entry, the transaction must carry it. A dead entry
    /// satisfies this — it was carried and skipped at quote time — so killing a
    /// book never bricks fills. The vAMM half of the baseline is inherent:
    /// it's in-program, gated only by oracle validity.
    pub fn require_baseline(&self, required_clob: Pubkey) -> Result<()> {
        validate!(
            required_clob == Pubkey::default() || self.carried().contains(&required_clob),
            ErrorCode::DefaultError,
            "router fill must include the market's CLOB quoter {}",
            required_clob
        )?;
        Ok(())
    }

    /// Carried entries the signed route did not name.
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
    /// Then every claimed entry must be **present** in the transaction, used or
    /// not. Presence rather than participation is the enforceable form: an
    /// entry that is inactive or unapproved is skipped at quote time, and
    /// whether it *should* have won is a question about prices, not accounts.
    /// Extra entries beyond the route are fine — the router allocates by price
    /// and an execute is bound to its own quote, so an uninvited quoter can
    /// only lose. Omitting one the taker asked for is the actual attack.
    pub fn require_signed_route(&self, claimed: &[Pubkey], digest: RouteDigest) -> Result<()> {
        validate!(
            crate::state::order_params::route_digest(claimed) == digest,
            ErrorCode::SignedRouteMismatch,
            "claimed route does not digest to the one the order was signed with"
        )?;
        for entry in claimed {
            validate!(
                self.carried().contains(entry),
                ErrorCode::SignedRouteEntryMissing,
                "signed route names quoter {} but the fill does not carry it",
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
    ) -> &'a [QuoterBook<'a>] {
        for (slot, quoted) in into.iter_mut().zip(self.quoted.iter()) {
            *slot = QuoterBook {
                priority: quoted.priority,
                levels: &self.levels[quoted.levels.clone()],
                withheld: quoted.withheld,
            };
        }
        &into[..self.quoted.len()]
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
            quoted: &self.quoted,
            market_index: inputs.market_index,
            accounts: self.accounts,
            users: inputs.users,
            taker: inputs.taker,
            taker_served_window: inputs.taker_served_window,
            slot,
            now,
        }
    }
}
