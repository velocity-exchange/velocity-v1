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
            prop_amm::{Direction, QuoterType, QuoterV0},
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
    std::collections::BTreeMap,
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
}

/// Decode a `RouterQuoteBufferV0` account's post-simulation bytes.
pub fn decode_quote_buffer(data: &[u8]) -> Result<QuoteView> {
    let buffer: RouterQuoteBufferV0 = read_zero_copy(data)?;
    let books = (0..buffer.source_count as usize)
        .map(|i| {
            let source = &buffer.sources[i];
            QuotedBook {
                key: source.key,
                kind: source.kind,
                priority: source.priority,
                clamped: source.clamped,
                levels: buffer.levels[i][..source.level_count as usize].to_vec(),
            }
        })
        .collect();
    Ok(QuoteView {
        market: buffer.market,
        direction: buffer.direction,
        quoted_size: buffer.quoted_size,
        slot: buffer.slot,
        books,
    })
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

/// Build the `quote_router` instruction for a market from live chain state:
/// the market's oracle, every active + approved registry entry, each Custom
/// quoter's `(User, UserStats)` pair (the margin clamp reads them), and the
/// union of the entries' registered quote-leg CPI accounts.
pub async fn build_quote_router_ix<S: ChainSource>(
    source: &S,
    velocity: &Pubkey,
    authority: &Pubkey,
    quote_buffer: &Pubkey,
    market_index: u16,
    direction: Direction,
    size: u64,
    // `User` accounts of DLOB makers to bridge, from whatever holds the
    // caller's DLOB view. Each costs the transaction two accounts, so a
    // caller passes candidates rather than the whole book; the split reports
    // which of them the fill would actually reach.
    dlob_makers: &[Pubkey],
) -> Result<Instruction> {
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
    entries.retain(|(_, entry)| entry.is_active && entry.is_approved);
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

    Ok(Instruction {
        program_id: *velocity,
        accounts,
        data: program::instruction::QuoteRouter {
            args: QuoteRouterArgs {
                market_index,
                direction,
                size,
                quoter_count: entries.len() as u8,
            },
        }
        .data(),
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
    let blockhash = source.latest_blockhash().await?;
    let message = Message::new_with_blockhash(&[instruction], Some(payer), &blockhash.hash);
    let tx = Transaction::new_unsigned(message);
    let outcome = source
        .simulate_transaction(&tx, &[*quote_buffer])
        .await
        .context("simulate quote_router")?;
    if let Some(err) = outcome.err {
        bail!("quote_router failed: {err}; logs: {:?}", outcome.logs);
    }
    let account = outcome
        .accounts
        .first()
        .cloned()
        .flatten()
        .ok_or_else(|| anyhow!("simulation returned no quote buffer state"))?;
    decode_quote_buffer(&account.data)
}

#[cfg(test)]
mod tests {
    use {super::*, program::state::router_quote::MAX_QUOTED_SOURCES};

    #[test]
    fn quote_buffer_round_trips_through_the_decoder() {
        let mut buffer: RouterQuoteBufferV0 = bytemuck::Zeroable::zeroed();
        buffer.market = 3;
        buffer.begin(1, 500, 42);
        let quoter = Pubkey::new_unique();
        buffer
            .push(
                QuotedSourceKind::Quoter,
                quoter,
                10,
                true,
                &[
                    program::state::prop_amm::PriceLevel { price: 99, size: 5 },
                    program::state::prop_amm::PriceLevel { price: 98, size: 7 },
                ],
            )
            .unwrap();

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
        assert_eq!(book.levels[1].size, 7);
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
