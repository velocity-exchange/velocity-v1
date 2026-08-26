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
        state::prop_amm::{
            ClobUserRefV0, Direction, PriceLevel, QuoteArgsV0, QuoterType, QuoterV0,
        },
        validate,
    },
    anchor_lang::{prelude::*, Discriminator},
};

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
    /// What it quoted, best price first. Owned because every later allocation
    /// and price check is held to it, long after the quoting CPI returned.
    pub levels: Vec<PriceLevel>,
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
    pub clob_authority: Pubkey,
    pub clob_authority_nonce: u8,
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
    ) -> Result<QuotedRoute<'info>> {
        let mut route = QuotedRoute {
            accounts: tail,
            // The one heap holder left, deliberately: a fixed array of these
            // is about a kilobyte, and this fill's stack frame is four — the
            // program has overflowed it before on a struct this size.
            quoted: Vec::with_capacity(MAX_ROUTE_QUOTERS),
            carried: [Pubkey::default(); MAX_ROUTE_QUOTERS],
            carried_len: 0,
        };

        for info in tail {
            let is_entry = info.owner == &crate::ID
                && info
                    .try_borrow_data()
                    .is_ok_and(|data| data.get(..8) == Some(QuoterV0::DISCRIMINATOR));
            if !is_entry {
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
                let (cpi_signer, cpi_signer_nonce) = quoter.cpi_signer(
                    &entry_key,
                    (inputs.clob_authority, inputs.clob_authority_nonce),
                );
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
                    },
                    &entry_key,
                    &cpi_signer,
                    cpi_signer_nonce,
                    route.accounts,
                )?;
                (
                    quoter.quoter_type,
                    quoter.user,
                    quoter.response_account,
                    quoter.priority,
                    levels,
                )
            };
            let (quoter_type, user, response_account, priority, quoted) = quoted;
            route.quoted.push(QuotedEntry {
                entry: loader,
                quoter_type,
                user,
                response_account,
                priority,
                levels: quoted.levels,
                withheld: quoted.withheld,
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
    pub fn require_signed_route(&self, claimed: &[Pubkey], digest: [u8; 4]) -> Result<()> {
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
                levels: &quoted.levels,
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
    ) -> CpiQuoterExecutor<'a, 'info> {
        CpiQuoterExecutor {
            caps: inputs.caps,
            reference_price: inputs.reference_price,
            quoted: &self.quoted,
            market_index: inputs.market_index,
            accounts: self.accounts,
            clob_authority: inputs.clob_authority,
            clob_authority_nonce: inputs.clob_authority_nonce,
            users: inputs.users,
            taker: inputs.taker,
            slot,
            now,
        }
    }
}
