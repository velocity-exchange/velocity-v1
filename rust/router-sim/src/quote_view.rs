//! Off-chain assembly and decoding of velocity's `quote_router` view.
//!
//! The view instruction is meant to be *simulated*: it CPIs `quote_v0` on
//! every registered quoter, bridges DLOB makers, ladders the vAMM against
//! everything else, and writes the verified books into the caller's quote
//! buffer. This module is the off-chain half a book publisher (or router)
//! needs: derive the account list from live chain state, simulate through a
//! [`ChainSource`], and read the books back out of post-simulation account
//! state — the same mechanism relay turners use for staged resolver
//! payloads, so it works identically over RPC, a subscription cache, or a
//! pooled in-process SVM.
//!
//! DLOB makers are the caller's to supply. The view bridges one level per
//! crossing resting order out of the user map it is handed, and nothing
//! on-chain can enumerate that map for it — a DLOB order lives in a `User`
//! account, so knowing which accounts to pass is the whole problem, and only
//! something holding a DLOB view can answer it. Pass none and the view still
//! returns the CLOB, every PropAMM (margin-clamped), and the vAMM shaded
//! against them.

use {
    crate::quoter_entries,
    anchor_lang::{Discriminator, InstructionData, ToAccountMetas},
    anyhow::{anyhow, bail, Context, Result},
    program::{
        instructions::QuoteRouterArgs,
        state::{
            perp_market::PerpMarket,
            prop_amm::{
                ClobUserRefV0 as UserRefV0, Direction, QuoterType, QuoterV0,
                L3_ROW_FLAG_BLOCKS_WALK,
            },
            router_quote::{QuotedLevelV0, QuotedSourceKind, RouterQuoteBufferV0},
            traits::Size,
            user::User,
        },
    },
    relay_chain_source::ChainSource,
    solana_sdk::{
        instruction::{AccountMeta, Instruction},
        message::Message,
        pubkey::Pubkey,
        transaction::Transaction,
    },
    solana_system_interface::instruction as system_instruction,
    std::collections::{BTreeMap, BTreeSet},
    velocity_quoter_health::EntryRef,
};

pub fn state_pda(velocity: &Pubkey) -> Pubkey {
    Pubkey::find_program_address(&[b"velocity_state"], velocity).0
}

pub fn velocity_signer_pda(velocity: &Pubkey) -> Pubkey {
    Pubkey::find_program_address(&[b"velocity_signer"], velocity).0
}

pub fn perp_market_pda(velocity: &Pubkey, market_index: u16) -> Pubkey {
    Pubkey::find_program_address(
        &[b"perp_market", market_index.to_le_bytes().as_ref()],
        velocity,
    )
    .0
}

pub fn spot_market_pda(velocity: &Pubkey, market_index: u16) -> Pubkey {
    Pubkey::find_program_address(
        &[b"spot_market", market_index.to_le_bytes().as_ref()],
        velocity,
    )
    .0
}

pub fn user_stats_pda(velocity: &Pubkey, authority: &Pubkey) -> Pubkey {
    Pubkey::find_program_address(&[b"user_stats", authority.as_ref()], velocity).0
}

/// Read a zero-copy account host-side: discriminator check, then an
/// unaligned pod copy (account bytes carry no alignment guarantee).
pub fn read_zero_copy<T: bytemuck::Pod + Discriminator>(data: &[u8]) -> Result<T> {
    let size = core::mem::size_of::<T>();
    if data.len() < 8 + size {
        bail!(
            "account holds {} bytes, {} needs {}",
            data.len(),
            core::any::type_name::<T>(),
            8 + size
        );
    }
    if &data[..8] != T::DISCRIMINATOR {
        bail!("wrong discriminator for {}", core::any::type_name::<T>());
    }
    Ok(bytemuck::pod_read_unaligned(&data[8..8 + size]))
}

/// One source's verified book, decoded from the quote buffer.
#[derive(Debug, Clone)]
pub struct QuotedBook {
    /// `QuoterV0` entry for a quoter, the maker's `User` for a DLOB order,
    /// the perp market for the vAMM.
    pub key: Pubkey,
    pub kind: QuotedSourceKind,
    /// Routing tier: lower fills first at a shared price.
    pub priority: u8,
    /// Margin verification reduced this book below what the source quoted.
    pub clamped: bool,
    pub levels: Vec<QuotedLevelV0>,
    /// The orders behind the ladder, best price first, each with the user it
    /// settles against. A quoter that fills from one account reports its
    /// ladder as rows against that account; a book reports its orders. Empty
    /// when the quoter said nothing and the caller has no account for it.
    pub rows: Vec<QuotedRow>,
}

/// One resting order behind a book, as the view reports it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct QuotedRow {
    pub price: u64,
    pub size: u64,
    /// The quoter's handle for the order, zero when the row is not an order.
    pub order_id: u64,
    /// Who the row settles against. Both the `User` and its `UserStats`
    /// derive from this, which is why the book stores identity this way.
    pub user: UserRefV0,
    /// `L3_ROW_FLAG_*`, as the quoter reported them.
    pub flags: u8,
}

impl QuotedRow {
    /// The row is a migrated taker remainder: it demands liquidity rather
    /// than offering it, so a cross cannot count the depth it holds.
    pub fn is_taker_origin(&self) -> bool {
        self.flags & program::state::prop_amm::L3_ROW_FLAG_TAKER_ORIGIN != 0
    }
}

/// A decoded quote view: per-source verified books for one taker direction,
/// in fill order (externals, DLOB makers, vAMM last).
#[derive(Debug, Clone)]
pub struct QuoteView {
    pub market: u16,
    /// 0 = long (books are asks), 1 = short (books are bids).
    pub direction: u8,
    pub quoted_size: u64,
    /// Slot the simulation ran at — the staleness check for consumers.
    pub slot: u64,
    pub books: Vec<QuotedBook>,
    /// The row region filled before every book had been described, so the
    /// last books carry fewer rows than they hold.
    pub rows_truncated: bool,
}

impl QuoteView {
    /// The users a fill against these books would settle for, in the order
    /// the books would take them.
    ///
    /// The order is the answer, not a detail of it: a book stops at the first
    /// maker the transaction did not carry, so a prefix of this list fills
    /// and a gap forfeits everything behind it.
    pub fn settleable_users(&self) -> Vec<UserRefV0> {
        self.ranked_settleable_users()
            .into_iter()
            .map(|(user, _)| user)
            .collect()
    }

    /// The same owners, ordered by whether carrying one buys *reach* or only
    /// *depth*, and each tagged with which it is.
    ///
    /// A book ends its walk at the first order whose owner the caller did not
    /// carry, so an owner that can end a walk gates every order behind it:
    /// leaving that owner out forfeits the depth past it. An owner none of whose
    /// orders can end a walk gates nothing — carrying it wins that owner's own
    /// size and nothing more.
    ///
    /// A caller has room for a bounded number of users, so it wants the gating
    /// owners first. Within each group the walk order is preserved, because that
    /// is the order the book stops in: a prefix of gating owners fills, and the
    /// first gap forfeits what is behind it. Truncating the result at the
    /// account budget therefore drops the owners that cost the least to lose.
    ///
    /// Which orders can end a walk is the quoter's own answer, carried per row
    /// as [`L3_ROW_FLAG_BLOCKS_WALK`]. Nothing here reimplements the rule, so a
    /// book that changes it does not leave this ordering stale.
    pub fn ranked_settleable_users(&self) -> Vec<(UserRefV0, bool)> {
        let mut users: Vec<(UserRefV0, bool)> = Vec::new();
        for row in self.books.iter().flat_map(|book| book.rows.iter()) {
            let gates = row.flags & L3_ROW_FLAG_BLOCKS_WALK != 0;
            match users.iter_mut().find(|(user, _)| *user == row.user) {
                // One gating order is enough to make an owner gating, whichever
                // of its orders the walk reached first.
                Some((_, seen)) => *seen |= gates,
                None => users.push((row.user, gates)),
            }
        }
        users.sort_by_key(|(_, gates)| !gates);
        users
    }
}

/// Decode a `RouterQuoteBufferV0` account's post-simulation bytes.
pub fn decode_quote_buffer(data: &[u8]) -> Result<QuoteView> {
    let buffer: RouterQuoteBufferV0 = read_zero_copy(data)?;
    let books = (0..buffer.source_count as usize)
        .map(|i| {
            let source = &buffer.sources[i];
            let start = source.row_start as usize;
            QuotedBook {
                key: source.key,
                kind: source.kind,
                priority: source.priority,
                clamped: source.clamped,
                levels: buffer.levels[i][..source.level_count as usize].to_vec(),
                rows: buffer.rows[start..start + source.row_len as usize]
                    .iter()
                    .map(|row| QuotedRow {
                        price: row.price,
                        size: row.size,
                        order_id: row.order_id,
                        user: UserRefV0 {
                            authority: row.authority,
                            sub_account_id: row.sub_account_id,
                        },
                        flags: row.flags,
                    })
                    .collect(),
            }
        })
        .collect();
    Ok(QuoteView {
        market: buffer.market,
        direction: buffer.direction,
        quoted_size: buffer.quoted_size,
        slot: buffer.slot,
        books,
        rows_truncated: buffer.rows_truncated,
    })
}

/// Static account keys one pass of the view can carry.
///
/// The binding resource is the transaction, not the buffer. A pass is one
/// legacy message, and a message spends 32 bytes on each key plus the byte
/// that indexes it in the instruction; what is left of the 1,232-byte packet
/// after the signature, the header, the blockhash, the compact counts and the
/// instruction's own data is the budget below. Overrunning it is not a
/// degraded book — the runtime rejects the transaction before velocity runs,
/// so the market publishes nothing.
pub const PASS_ACCOUNT_BUDGET: usize = {
    // Signature and its count, the three header bytes, the blockhash, the
    // key and account-index counts, the program index, the data length, and
    // `QuoteRouterArgs` behind its discriminator.
    const ENVELOPE: usize = 64 + 1 + 3 + 32 + 2 + 1 + 2 + 2 + 24;
    (PACKET_DATA_SIZE - ENVELOPE) / (32 + 1)
};

/// What one transaction may weigh: an IPv6 MTU less the UDP and IP headers.
pub const PACKET_DATA_SIZE: usize = 1280 - 40 - 8;

/// Keys every pass carries whatever it quotes: the instruction's own three
/// accounts, the oracle/spot/perp map, the quoter signer, and velocity
/// itself as the program the message invokes.
pub const PASS_FIXED_ACCOUNTS: usize = 3 + 3 + 1 + 1;

/// Static account keys a pass carrying `entries` and `dlob_makers` needs.
///
/// The same set [`build_quote_router_ix`] assembles, counted rather than
/// built: entry keys, the union of their registered CPI accounts, and two
/// accounts for every user a book has to load. It rounds up rather than down
/// where the builder would dedup further — a pass that plans too small is a
/// pass that publishes nothing.
pub fn pass_account_cost(entries: &[QuoterV0], dlob_makers: usize) -> usize {
    let mut cpi: BTreeSet<Pubkey> = BTreeSet::new();
    let mut users: BTreeSet<Pubkey> = BTreeSet::new();
    for entry in entries {
        for meta in &entry.quote_accounts[..entry.quote_accounts_count as usize] {
            cpi.insert(meta.pubkey);
        }
        cpi.insert(entry.response_account);
        cpi.insert(entry.program_id);
        if entry.quoter_type == QuoterType::Custom {
            users.insert(entry.user);
        }
    }
    PASS_FIXED_ACCOUNTS + entries.len() + cpi.len() + 2 * (users.len() + dlob_makers)
}

/// Instructions creating + initializing a quote buffer for `(authority,
/// market)`. The account is too large for a CPI allocation, so the caller
/// funds and creates it directly; `buffer` must sign the create (a fresh
/// keypair), `authority` signs the init.
pub fn create_quote_buffer_ixs(
    velocity: &Pubkey,
    payer: &Pubkey,
    authority: &Pubkey,
    buffer: &Pubkey,
    market_index: u16,
    rent_lamports: u64,
) -> Vec<Instruction> {
    vec![
        system_instruction::create_account(
            payer,
            buffer,
            rent_lamports,
            RouterQuoteBufferV0::SIZE as u64,
            velocity,
        ),
        Instruction {
            program_id: *velocity,
            accounts: program::accounts::InitializeRouterQuoteBuffer {
                quote_buffer: *buffer,
                authority: *authority,
            }
            .to_account_metas(None),
            data: program::instruction::InitializeRouterQuoteBuffer { market_index }.data(),
        },
    ]
}

/// A built `quote_router` instruction, with the entries it carries.
///
/// The entries are in the order the on-chain router walks them. That order is
/// what lets a runtime CPI bracket in a failed simulation's logs be matched
/// back to a registry entry, which is the only attribution available when a
/// quoter exhausts the compute budget and leaves the router no room to log.
pub struct QuoteRouterIx {
    pub instruction: Instruction,
    pub entries: Vec<CarriedEntry>,
}

/// A registry entry a pass carried, with what a consumer needs to attribute
/// its levels.
///
/// `user` is the account a Custom entry settles against: one maker, named at
/// registration. A Custom quoter's levels therefore have a maker even though
/// they have no resting order, which is what lets them appear in an L3 book.
/// For a CLOB entry it is the registrant, not the makers resting on the book,
/// so it must not be used to attribute CLOB depth.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CarriedEntry {
    pub quoter: Pubkey,
    pub program: Pubkey,
    pub user: Pubkey,
    pub quoter_type: QuoterType,
}

impl CarriedEntry {
    /// True when this entry's levels all belong to the one maker it names.
    pub fn attributes_to_one_maker(&self) -> bool {
        self.quoter_type == QuoterType::Custom
    }

    pub fn as_entry_ref(&self) -> EntryRef {
        EntryRef {
            quoter: self.quoter,
            program: self.program,
        }
    }
}

/// Build the `quote_router` instruction for a market from live chain state:
/// the market's oracle, every active + approved registry entry, each Custom
/// quoter's `(User, UserStats)` pair (the margin clamp reads them), and the
/// union of the entries' registered quote-leg CPI accounts.
/// What one pass of the quote view should carry.
#[derive(Clone, Copy)]
pub struct QuoteRouterParams<'a> {
    pub velocity: &'a Pubkey,
    pub authority: &'a Pubkey,
    pub quote_buffer: &'a Pubkey,
    pub market_index: u16,
    pub direction: Direction,
    pub size: u64,
    /// `User` accounts of DLOB makers to bridge, from whatever holds the
    /// caller's DLOB view. Each costs the transaction two accounts and one of
    /// the buffer's source slots, so a caller passes candidates rather than
    /// the whole book; the split reports which of them the fill would reach.
    pub dlob_makers: &'a [Pubkey],
    /// Registry entries to leave out. A quoter proven to break this market's
    /// simulation is dropped here, so the rest of the market still quotes.
    /// Without this the only way to route around a bad quoter is the on-chain
    /// approval flags, which no router holds.
    pub exclude: &'a [Pubkey],
    /// Carry only these entries, when set.
    ///
    /// The buffer holds a fixed number of sources and a transaction a fixed
    /// number of accounts, so a market with more quoters than either allows
    /// is read in several passes. This is how a caller says which pass it is
    /// building.
    pub only: Option<&'a [Pubkey]>,
    /// Quote the vAMM into this pass. Exactly one pass of a market should,
    /// because the vAMM shades against the books carried alongside it.
    pub include_vamm: bool,
    /// Whether the flow this view prices for served a protection window —
    /// the swift hold, or the book's activation delay. A bumped book and a
    /// protected-flow quoter show no depth when this is false, exactly as
    /// the fill's route would.
    pub taker_served_window: bool,
}

pub async fn build_quote_router_ix<S: ChainSource>(
    source: &S,
    params: &QuoteRouterParams<'_>,
) -> Result<QuoteRouterIx> {
    let QuoteRouterParams {
        velocity,
        authority,
        quote_buffer,
        market_index,
        direction,
        size,
        dlob_makers,
        exclude,
        only,
        include_vamm,
        taker_served_window,
    } = *params;
    let perp_market_key = perp_market_pda(velocity, market_index);
    let perp_market_account = source
        .get_multiple_accounts(&[perp_market_key])
        .await?
        .pop()
        .flatten()
        .ok_or_else(|| anyhow!("perp market {market_index} not found"))?;
    let perp_market: PerpMarket = read_zero_copy(&perp_market_account.data)?;
    let quote_spot_market = spot_market_pda(velocity, perp_market.quote_spot_market_index);

    // Live registry entries for the market, in a stable order.
    let mut entries: Vec<(Pubkey, QuoterV0)> = quoter_entries(source, velocity, market_index)
        .await?
        .into_iter()
        .map(|(key, account)| Ok((key, read_zero_copy::<QuoterV0>(&account.data)?)))
        .collect::<Result<_>>()?;
    entries.retain(|(key, entry)| {
        entry.is_active
            && entry.is_approved
            && !exclude.contains(key)
            && only.map(|only| only.contains(key)).unwrap_or(true)
    });
    entries.sort_by_key(|(key, _)| *key);

    // The user map the view walks: custom quoters' users, which the margin
    // clamp needs, and the DLOB makers the caller wants bridged. One section,
    // because the instruction reads one map — a custom quoter that is also a
    // DLOB maker appears once.
    let map_users: Vec<Pubkey> = {
        let mut users: Vec<Pubkey> = entries
            .iter()
            .filter(|(_, entry)| entry.quoter_type == QuoterType::Custom)
            .map(|(_, entry)| entry.user)
            .chain(dlob_makers.iter().copied())
            .collect();
        users.sort();
        users.dedup();
        users
    };
    let user_accounts = source.get_multiple_accounts(&map_users).await?;
    let user_pairs: Vec<(Pubkey, Pubkey)> = map_users
        .iter()
        .zip(user_accounts)
        .map(|(key, account)| {
            let account = account.ok_or_else(|| anyhow!("quote-view user {key} not found"))?;
            let user: User = read_zero_copy(&account.data)?;
            Ok((*key, user_stats_pda(velocity, &user.authority)))
        })
        .collect::<Result<_>>()?;

    // The union of the entries' registered quote-leg accounts, plus the
    // pieces every quoter CPI needs: its response account (written), its
    // program, and the velocity signer. Writability ORs across entries.
    let mut cpi_union: BTreeMap<Pubkey, bool> = BTreeMap::new();
    for (_, entry) in &entries {
        for meta in &entry.quote_accounts[..entry.quote_accounts_count as usize] {
            let writable = cpi_union.entry(meta.pubkey).or_default();
            *writable |= meta.is_writable;
        }
        *cpi_union.entry(entry.response_account).or_default() |= true;
        cpi_union.entry(entry.program_id).or_default();
    }
    cpi_union.entry(velocity_signer_pda(velocity)).or_default();

    let mut accounts = program::accounts::QuoteRouter {
        state: state_pda(velocity),
        authority: *authority,
        quote_buffer: *quote_buffer,
    }
    .to_account_metas(None);
    // Map section: oracle, quote spot market, the (writable) perp market.
    accounts.push(AccountMeta::new_readonly(perp_market.oracle, false));
    accounts.push(AccountMeta::new(quote_spot_market, false));
    accounts.push(AccountMeta::new(perp_market_key, false));
    // User-map section: the (User, UserStats) pairs above. Writable,
    // matching the fill's convention — the margin clamp opens a fresh
    // maker's position slot the way a fill would, and a read-only user
    // degrades that maker's book to zero.
    for (user, stats) in &user_pairs {
        accounts.push(AccountMeta::new(*user, false));
        accounts.push(AccountMeta::new(*stats, false));
    }
    // Quoter section: entries, then the CPI union.
    for (key, _) in &entries {
        accounts.push(AccountMeta::new_readonly(*key, false));
    }
    for (key, writable) in &cpi_union {
        accounts.push(if *writable {
            AccountMeta::new(*key, false)
        } else {
            AccountMeta::new_readonly(*key, false)
        });
    }

    Ok(QuoteRouterIx {
        instruction: Instruction {
            program_id: *velocity,
            accounts,
            data: program::instruction::QuoteRouter {
                args: QuoteRouterArgs {
                    market_index,
                    direction,
                    size,
                    quoter_count: entries.len() as u8,
                    include_vamm,
                    taker_served_window,
                },
            }
            .data(),
        },
        entries: entries
            .iter()
            .map(|(key, entry)| CarriedEntry {
                quoter: *key,
                program: entry.program_id,
                user: entry.user,
                quoter_type: entry.quoter_type,
            })
            .collect(),
    })
}

/// Simulate a built quote view and decode the books out of the buffer's
/// post-simulation state. Never lands anything: the transaction is unsigned
/// (simulation skips signature verification on every transport).
pub async fn simulate_quote_view<S: ChainSource>(
    source: &S,
    instruction: Instruction,
    payer: &Pubkey,
    quote_buffer: &Pubkey,
) -> Result<QuoteView> {
    simulate_quote_view_with_cost(source, instruction, payer, quote_buffer)
        .await
        .map(|(view, _)| view)
}

/// A simulation that did not succeed, with the evidence needed to say who
/// caused it.
///
/// The logs name the quoter: `quote_router` logs `quoter <key> quote failed`
/// for every entry whose CPI it could not use. Rendering them into a message
/// and dropping the vector would leave every caller unable to tell a maker's
/// failure from its own, so they are carried as data. Reach them with
/// `anyhow::Error::downcast_ref::<QuoteSimFailure>`.
#[derive(Debug, Clone)]
pub struct QuoteSimFailure {
    pub err: String,
    pub logs: Vec<String>,
    pub units_consumed: u64,
}

impl std::fmt::Display for QuoteSimFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "quote_router failed: {}; logs: {:?}",
            self.err, self.logs
        )
    }
}

impl std::error::Error for QuoteSimFailure {}

/// Simulate a built quote view and report what the simulation cost.
///
/// The compute figure is the whole view's, not one quoter's. It still bounds
/// a single quoter: a view that costs little cannot hold a quoter that costs
/// much.
pub async fn simulate_quote_view_with_cost<S: ChainSource>(
    source: &S,
    instruction: Instruction,
    payer: &Pubkey,
    quote_buffer: &Pubkey,
) -> Result<(QuoteView, u64)> {
    let blockhash = source.latest_blockhash().await?;
    let message = Message::new_with_blockhash(&[instruction], Some(payer), &blockhash.hash);
    // The planner sizes passes to fit this; measuring the built message is
    // what keeps a disagreement between the two from reaching the runtime,
    // which answers an oversized transaction with a panic in its own
    // sanitization rather than an error a caller can read.
    let wire = message.serialize().len() + 1 + 64;
    if wire > PACKET_DATA_SIZE {
        bail!(
            "quote pass needs {wire} bytes over the wire, past the {PACKET_DATA_SIZE}-byte \
             packet: {} accounts",
            message.account_keys.len()
        );
    }
    let tx = Transaction::new_unsigned(message);
    let outcome = source
        .simulate_transaction(&tx, &[*quote_buffer])
        .await
        .context("simulate quote_router")?;
    if let Some(err) = outcome.err {
        return Err(anyhow::Error::new(QuoteSimFailure {
            err,
            logs: outcome.logs,
            units_consumed: outcome.units_consumed,
        }));
    }
    let account = outcome
        .accounts
        .first()
        .cloned()
        .flatten()
        .ok_or_else(|| anyhow!("simulation returned no quote buffer state"))?;
    decode_quote_buffer(&account.data).map(|view| (view, outcome.units_consumed))
}

#[cfg(test)]
mod tests {
    use {super::*, program::state::router_quote::MAX_QUOTED_SOURCES};

    fn user(n: u8) -> UserRefV0 {
        UserRefV0 {
            authority: Pubkey::new_from_array([n; 32]),
            sub_account_id: 0,
        }
    }

    /// `gates` is the book's own answer about this order, which is what the
    /// ranking reads — the size is carried only to tell rows apart.
    fn row(size: u64, owner: u8, gates: bool) -> QuotedRow {
        QuotedRow {
            price: 100,
            size,
            order_id: size,
            user: user(owner),
            flags: if gates { L3_ROW_FLAG_BLOCKS_WALK } else { 0 },
        }
    }

    fn view_of(rows: Vec<QuotedRow>) -> QuoteView {
        QuoteView {
            market: 0,
            direction: 0,
            quoted_size: 0,
            slot: 0,
            rows_truncated: false,
            books: vec![QuotedBook {
                key: Pubkey::new_unique(),
                kind: QuotedSourceKind::Quoter,
                priority: 10,
                clamped: false,
                levels: vec![],
                rows,
            }],
        }
    }

    /// An owner all of whose orders sit below the floor cannot end a walk, so
    /// it goes last: truncating the list at the account budget then drops the
    /// makers that cost the least to lose.
    #[test]
    fn ranking_puts_the_owners_that_gate_depth_first() {
        // Walk order: dust(1), gating(2), dust(3), gating(4).
        let view = view_of(vec![
            row(1, 1, false),
            row(50, 2, true),
            row(2, 3, false),
            row(80, 4, true),
        ]);

        let ranked = view.ranked_settleable_users();
        assert_eq!(
            ranked,
            vec![
                (user(2), true),
                (user(4), true),
                (user(1), false),
                (user(3), false),
            ],
            "gating owners first, walk order kept inside each group"
        );
    }

    /// One gating order is enough, whichever of an owner's orders the walk
    /// reached first — the walk stops at that order either way.
    #[test]
    fn one_gating_order_makes_its_owner_gating() {
        let view = view_of(vec![row(1, 1, false), row(2, 2, false), row(50, 1, true)]);
        assert_eq!(
            view.ranked_settleable_users(),
            vec![(user(1), true), (user(2), false)]
        );
    }

    /// A book with no floor flags every row, and the order is then the walk's
    /// own — which is what this endpoint answered before any floor existed.
    #[test]
    fn a_book_that_gates_everything_leaves_walk_order_untouched() {
        let view = view_of(vec![row(1, 3, true), row(50, 1, true), row(2, 2, true)]);
        assert_eq!(
            view.ranked_settleable_users(),
            vec![(user(3), true), (user(1), true), (user(2), true)]
        );
        assert_eq!(view.settleable_users(), vec![user(3), user(1), user(2)]);
    }

    #[test]
    fn quote_buffer_round_trips_through_the_decoder() {
        let mut buffer: RouterQuoteBufferV0 = bytemuck::Zeroable::zeroed();
        buffer.market = 3;
        buffer.begin(1, 500, 42);
        let quoter = Pubkey::new_unique();
        // Capped below what the book quotes, so the decoder is given a
        // clamped source to report: 12 base offered, 11 admitted.
        let clamped = buffer
            .push_capped(
                QuotedSourceKind::Quoter,
                quoter,
                10,
                &[
                    program::state::prop_amm::PriceLevel { price: 99, size: 5 },
                    program::state::prop_amm::PriceLevel { price: 98, size: 7 },
                ],
                11,
            )
            .unwrap();
        assert!(clamped);

        let mut data = RouterQuoteBufferV0::DISCRIMINATOR.to_vec();
        data.extend_from_slice(bytemuck::bytes_of(&buffer));

        let view = decode_quote_buffer(&data).unwrap();
        assert_eq!(view.market, 3);
        assert_eq!(view.direction, 1);
        assert_eq!(view.quoted_size, 500);
        assert_eq!(view.slot, 42);
        assert_eq!(view.books.len(), 1);
        let book = &view.books[0];
        assert_eq!(book.key, quoter);
        assert_eq!(book.kind, QuotedSourceKind::Quoter);
        assert_eq!(book.priority, 10);
        assert!(book.clamped);
        assert_eq!(book.levels.len(), 2);
        assert_eq!(book.levels[0].price, 99);
        assert_eq!(book.levels[1].size, 6, "the second level took the cap");
        let _ = MAX_QUOTED_SOURCES;
    }

    #[test]
    fn truncated_or_mislabeled_buffers_are_rejected() {
        assert!(decode_quote_buffer(&[0u8; 16]).is_err());
        let mut data = vec![0u8; RouterQuoteBufferV0::SIZE];
        data[..8].copy_from_slice(&[9; 8]);
        assert!(decode_quote_buffer(&data).is_err());
    }
}
