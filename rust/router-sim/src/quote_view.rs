//! Off-chain assembly and decoding of velocity's `quote_router` view.
//!
//! A caller simulates the view instruction. The instruction calls `quote_v0`
//! on every registered quoter, bridges DLOB makers, ladders the vAMM against
//! the other sources, and writes the verified books into the caller's quote
//! buffer. This module is the off-chain half a book publisher or a router
//! needs. It derives the account list from live chain state, simulates through
//! a [`ChainSource`], and reads the books back out of post-simulation account
//! state. Relay turners read staged resolver payloads the same way, so this
//! works over RPC, a subscription cache, or a pooled in-process SVM alike.
//!
//! The caller supplies the DLOB makers. The view bridges one level per
//! crossing resting order out of the user map the caller hands it. No on-chain
//! code can enumerate that map, because a DLOB order lives in a `User` account.
//! Only a holder of a DLOB view knows which accounts to pass. With no makers
//! passed, the view still returns the CLOB, every PropAMM after the margin
//! clamp, and the vAMM shaded against them.

pub use crate::pdas::{
    perp_market as perp_market_pda, spot_market as spot_market_pda, state as state_pda,
    user_stats as user_stats_pda, velocity_signer as velocity_signer_pda,
};
use {
    crate::{quoter_slab_pda, quoter_slab_slots},
    anchor_lang::{Discriminator, InstructionData, ToAccountMetas},
    anyhow::{anyhow, bail, Context, Result},
    program::{
        instructions::{InitializeRouterQuoteBufferArgs, QuoteRouterArgs},
        state::{
            perp_market::PerpMarket,
            prop_amm::{
                ClobUserRefV0 as UserRefV0, Direction, QuoterSlotV0, QuoterType,
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

/// Read a zero-copy account on the host. The function checks the discriminator,
/// then makes an unaligned pod copy. Account bytes carry no alignment guarantee.
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

/// Fetch one account. `Ok(None)` when it does not exist yet. `what` names the
/// account in the error a failed fetch carries.
pub async fn fetch_maybe_account<S: ChainSource + ?Sized>(
    source: &S,
    key: &Pubkey,
    what: &str,
) -> Result<Option<solana_account::Account>> {
    Ok(source
        .get_multiple_accounts(&[*key])
        .await
        .with_context(|| format!("fetch {what} {key}"))?
        .pop()
        .flatten())
}

/// Fetch one account that must exist.
pub async fn fetch_account<S: ChainSource + ?Sized>(
    source: &S,
    key: &Pubkey,
    what: &str,
) -> Result<solana_account::Account> {
    fetch_maybe_account(source, key, what)
        .await?
        .ok_or_else(|| anyhow!("{what} {key} not found"))
}

/// Fetch one zero-copy account that must exist, and decode it.
pub async fn fetch_zero_copy<S, T>(source: &S, key: &Pubkey, what: &str) -> Result<T>
where
    S: ChainSource + ?Sized,
    T: bytemuck::Pod + Discriminator,
{
    let account = fetch_account(source, key, what).await?;

    read_zero_copy(&account.data)
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
    /// The orders behind the ladder, best price first, with the user each
    /// settles against. A quoter that fills from one account reports its
    /// ladder as rows against that account. A book reports its own orders.
    /// Empty when the quoter reported nothing and the caller has no account for it.
    pub rows: Vec<QuotedRow>,
}

/// One resting order behind a book, as the view reports it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct QuotedRow {
    pub price: u64,
    pub size: u64,
    /// The quoter's handle for the order, zero when the row is not an order.
    pub order_id: u64,
    /// Who the row settles against. The `User` and its `UserStats` both derive
    /// from this value, so the book stores identity in this form.
    pub user: UserRefV0,
    /// `L3_ROW_FLAG_*`, as the quoter reported them.
    pub flags: u8,
}

impl QuotedRow {
    /// The row is a migrated taker remainder. It demands liquidity instead of
    /// offering it, so a cross cannot count the depth it holds.
    pub fn is_taker_origin(&self) -> bool {
        self.flags & program::state::prop_amm::L3_ROW_FLAG_TAKER_ORIGIN != 0
    }
}

/// A decoded quote view. It holds the per-source verified books for one taker
/// direction, in fill order. Externals come first, then DLOB makers, then the
/// vAMM.
#[derive(Debug, Clone)]
pub struct QuoteView {
    pub market: u16,
    /// 0 = long (books are asks), 1 = short (books are bids).
    pub direction: u8,
    pub quoted_size: u64,
    /// Slot the simulation ran at. Consumers use it as the staleness check.
    pub slot: u64,
    pub books: Vec<QuotedBook>,
    /// The row region filled before the buffer described every book, so the
    /// last books carry fewer rows than they hold.
    pub rows_truncated: bool,
}

impl QuoteView {
    /// The users a fill against these books would settle for, in the order the
    /// books would take them. A book stops at the first maker the transaction
    /// did not carry, so a prefix of this list fills and a gap forfeits every
    /// maker behind it.
    pub fn settleable_users(&self) -> Vec<UserRefV0> {
        self.ranked_settleable_users()
            .into_iter()
            .map(|(user, _)| user)
            .collect()
    }

    /// The same owners, flagged for whether each gates the depth behind it.
    /// Gating owners come first, walk order kept inside each group.
    /// A book stops at the first order whose owner the caller did not carry, so
    /// a gating owner gates everything behind it and a non-gating owner wins
    /// only its own size. Truncating this list at an account budget drops the
    /// cheapest owners to lose first, per [`L3_ROW_FLAG_BLOCKS_WALK`].
    pub fn ranked_settleable_users(&self) -> Vec<(UserRefV0, bool)> {
        let mut users: Vec<(UserRefV0, bool)> = Vec::new();
        for row in self.books.iter().flat_map(|book| book.rows.iter()) {
            let gates = row.flags & L3_ROW_FLAG_BLOCKS_WALK != 0;
            match users.iter_mut().find(|(user, _)| *user == row.user) {
                // One gating order makes the owner gating, whichever of its
                // orders the walk reached first.
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

/// Static account keys one pass of the view can carry. The transaction
/// binds this limit, not the buffer. It is what remains of the 1,232-byte
/// packet after the fixed envelope, at 33 bytes per key. An oversized pass
/// is rejected before velocity runs, so the market publishes nothing.
pub const PASS_ACCOUNT_BUDGET: usize = {
    // Signature and its count, the three header bytes, the blockhash, the
    // key and account-index counts, the program index, the data length, and
    // `QuoteRouterArgs` behind its discriminator.
    const ENVELOPE: usize = 64 + 1 + 3 + 32 + 2 + 1 + 2 + 2 + 24;
    (PACKET_DATA_SIZE - ENVELOPE) / (32 + 1)
};

/// The largest transaction the network carries. It is an IPv6 MTU less the IP
/// and UDP headers.
pub const PACKET_DATA_SIZE: usize = 1280 - 40 - 8;

/// Keys every pass carries, whatever it quotes. They are the instruction's own
/// three accounts, the oracle, spot market and perp market map, the market's
/// quoter slab, and velocity as the program the message invokes.
pub const PASS_FIXED_ACCOUNTS: usize = 3 + 3 + 1 + 1;

/// The CPI accounts a set of quoter slots is consulted through, keyed by
/// account and valued by writability.
///
/// Each slot contributes its registered accounts, its response account as
/// writable, and its program as read only. Writability is the OR across
/// slots. The whole registered list rides, not the quote leg's subset, since
/// each leg resolves by index into it, which keeps any registered signer.
pub fn quoter_cpi_union(slots: &[QuoterSlotV0]) -> BTreeMap<Pubkey, bool> {
    let mut union: BTreeMap<Pubkey, bool> = BTreeMap::new();
    for slot in slots {
        for meta in slot.config.registered_accounts() {
            *union.entry(meta.pubkey).or_default() |= meta.is_writable;
        }

        *union.entry(slot.config.response_account).or_default() |= true;
        union.entry(slot.config.program_id).or_default();
    }

    union
}

/// The union as account metas, in key order.
pub fn cpi_account_metas(union: &BTreeMap<Pubkey, bool>) -> Vec<AccountMeta> {
    union
        .iter()
        .map(|(key, writable)| {
            if *writable {
                AccountMeta::new(*key, false)
            } else {
                AccountMeta::new_readonly(*key, false)
            }
        })
        .collect()
}

/// Static account keys a pass carrying `slots` needs.
///
/// This counts the same set [`build_quote_router_ix`] assembles, without
/// building it, so the planner and the builder cannot disagree about which
/// accounts a pass holds. The count still rounds up where the message would
/// deduplicate further, because a pass planned too small publishes nothing.
pub fn pass_account_cost(slots: &[QuoterSlotV0]) -> usize {
    let users: BTreeSet<Pubkey> = slots
        .iter()
        .filter(|slot| slot.config.quoter_type == QuoterType::Custom)
        .map(|slot| slot.config.user)
        .collect();

    PASS_FIXED_ACCOUNTS + quoter_cpi_union(slots).len() + 2 * users.len()
}

/// The instructions that create and initialize a quote buffer for one authority
/// and market. The account is too large for a CPI allocation, so the caller
/// funds and creates it directly. `buffer` must be a fresh keypair and must sign
/// the create. `authority` signs the init.
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
            data: program::instruction::InitializeRouterQuoteBuffer {
                args: InitializeRouterQuoteBufferArgs { market_index },
            }
            .data(),
        },
    ]
}

/// A built `quote_router` instruction, with the entries it carries, in
/// on-chain walk order. That lets a caller match a runtime CPI bracket in
/// a failed simulation's logs back to a registry entry, the only
/// attribution left once a quoter exhausts the compute budget.
pub struct QuoteRouterIx {
    pub instruction: Instruction,
    pub entries: Vec<CarriedEntry>,
}

/// A quoter a pass carried, with what a consumer needs to attribute its
/// levels. `user` is the account a Custom entry settles against, so its
/// levels can appear in an L3 book though it rests no order. For a CLOB
/// entry `user` is the registrant, not a maker. Never attribute CLOB depth to it.
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

/// What one pass of the quote view should carry.
#[derive(Clone, Copy)]
pub struct QuoteRouterParams<'a> {
    pub velocity: &'a Pubkey,
    pub authority: &'a Pubkey,
    pub quote_buffer: &'a Pubkey,
    pub market_index: u16,
    pub direction: Direction,
    pub size: u64,
    /// Quoters to leave out, by staging-entry address. A quoter proven to
    /// break this market's simulation is dropped here, so the rest of the
    /// market still quotes. Without this the only way to route around a bad
    /// quoter is the on-chain approval flags, which no router holds.
    pub exclude: &'a [Pubkey],
    /// Carry only these quoters, by staging-entry address, when set.
    ///
    /// The buffer and the transaction both hold a fixed number of sources,
    /// so a market with more quoters is read in several passes. This names the pass being built.
    pub only: Option<&'a [Pubkey]>,
    /// Quote the vAMM into this pass. Exactly one pass of a market sets this,
    /// because the vAMM shades against the books carried with it.
    pub include_vamm: bool,
    /// Whether the flow this view prices served a protection window. The window
    /// is the swift hold or the book's activation delay. A bumped book and a
    /// protected-flow quoter show no depth when this is false, the same as the
    /// fill's route.
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
        exclude,
        only,
        include_vamm,
        taker_served_window,
    } = *params;
    let perp_market_key = perp_market_pda(velocity, market_index);
    let perp_market: PerpMarket = fetch_zero_copy(source, &perp_market_key, "perp market").await?;
    let quote_spot_market = spot_market_pda(velocity, perp_market.quote_spot_market_index);

    // The slots this pass consults, in slab order. That is the order the
    // on-chain walk quotes them in, and log attribution aligns against it. A
    // pass consults a slot by carrying its response account, so this filter
    // decides what rides the account tail.
    let mut slots: Vec<QuoterSlotV0> = quoter_slab_slots(source, velocity, market_index).await?;
    slots.retain(|slot| {
        slot.quotes()
            && !exclude.contains(&slot.entry)
            && only.map(|only| only.contains(&slot.entry)).unwrap_or(true)
    });

    // The user map the view walks. It holds the custom quoters' users, which
    // the margin clamp needs. The instruction reads one map, so this is one
    // section, and a user named by two quoters appears once.
    let map_users: Vec<Pubkey> = {
        let mut users: Vec<Pubkey> = slots
            .iter()
            .filter(|slot| slot.config.quoter_type == QuoterType::Custom)
            .map(|slot| slot.config.user)
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

    let cpi_union = quoter_cpi_union(&slots);

    let mut accounts = program::accounts::QuoteRouter {
        state: state_pda(velocity),
        authority: *authority,
        quote_buffer: *quote_buffer,
    }
    .to_account_metas(None);
    // Map section: the oracle, the quote spot market, and the writable perp
    // market.
    accounts.push(AccountMeta::new_readonly(perp_market.oracle, false));
    accounts.push(AccountMeta::new(quote_spot_market, false));
    accounts.push(AccountMeta::new(perp_market_key, false));
    // User-map section: the User and UserStats pairs above. They are writable,
    // which matches the fill's convention. The margin clamp opens a fresh
    // maker's position slot the way a fill does, and a read-only user degrades
    // that maker's book to zero.
    for (user, stats) in &user_pairs {
        accounts.push(AccountMeta::new(*user, false));
        accounts.push(AccountMeta::new(*stats, false));
    }

    // Quoter section: the market's slab, then the CPI union. A slot is
    // consulted because its response account is in the union. No entry accounts
    // ride the call.
    accounts.push(AccountMeta::new_readonly(
        quoter_slab_pda(velocity, market_index),
        false,
    ));

    accounts.extend(cpi_account_metas(&cpi_union));

    Ok(QuoteRouterIx {
        instruction: Instruction {
            program_id: *velocity,
            accounts,
            data: program::instruction::QuoteRouter {
                args: QuoteRouterArgs {
                    market_index,
                    direction,
                    size,
                    taker_served_window,
                    include_vamm,
                },
            }
            .data(),
        },

        entries: slots
            .iter()
            .map(|slot| CarriedEntry {
                quoter: slot.entry,
                program: slot.config.program_id,
                user: slot.config.user,
                quoter_type: slot.config.quoter_type,
            })
            .collect(),
    })
}

/// Simulate a built quote view and decode the books out of the buffer's
/// post-simulation state. The transaction is unsigned, so nothing can land.
/// Simulation skips signature verification on every transport.
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

/// A simulation that failed, with the evidence that names the cause.
/// `quote_router` logs `quoter <key> quote failed` per entry whose CPI
/// failed, so a caller can tell a maker's failure from its own. Read the
/// logs with `anyhow::Error::downcast_ref::<QuoteSimFailure>`.
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
/// The compute figure covers the whole view, not one quoter. It still bounds a
/// single quoter, because a view that costs little cannot hold a quoter that
/// costs much.
pub async fn simulate_quote_view_with_cost<S: ChainSource>(
    source: &S,
    instruction: Instruction,
    payer: &Pubkey,
    quote_buffer: &Pubkey,
) -> Result<(QuoteView, u64)> {
    let blockhash = source.latest_blockhash().await?;
    let message = Message::new_with_blockhash(&[instruction], Some(payer), &blockhash.hash);
    // The planner sizes passes to fit this limit. Measuring the built message
    // keeps a disagreement between the two away from the runtime. The runtime
    // answers an oversized transaction with a panic inside its own
    // sanitization, not with an error a caller can read.
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
        .simulate_transaction(&crate::as_versioned(&tx)?, &[*quote_buffer])
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

    /// `gates` is the book's own answer about this order, and the ranking reads
    /// only that. The size tells rows apart.
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

    /// An owner whose orders all sit below the floor cannot end a walk, so it
    /// goes last. A caller that truncates the list at its account budget then
    /// drops the makers that cost the least to lose.
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
    /// reached first. The walk stops at that order either way.
    #[test]
    fn one_gating_order_makes_its_owner_gating() {
        let view = view_of(vec![row(1, 1, false), row(2, 2, false), row(50, 1, true)]);
        assert_eq!(
            view.ranked_settleable_users(),
            vec![(user(1), true), (user(2), false)]
        );
    }

    /// A book with no floor flags every row, so the result keeps the walk order.
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
        // The cap sits below what the book quotes, so the decoder gets a
        // clamped source to report. The book offers 12 base and the cap
        // admits 11.
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
