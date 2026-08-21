//! Quote a perp market's router liquidity by **simulating the fill**, not by
//! decoding books off-chain.
//!
//! The router splits a taker across the vAMM, resting DLOB orders, the CLOB,
//! and any registered PropAMM — and it learns each external quoter's prices
//! by CPI'ing `quote_v0`. A Custom quoter is an arbitrary third-party
//! program, so there is no general way to decode its book from the outside:
//! calling it is the only way to price it. Simulation is therefore not an
//! optimization here, it is the only correct approach, and it subsumes the
//! CLOB and vAMM for free.
//!
//! So this crate does the obvious thing: build the real
//! `fill_perp_order` transaction, run it against cached chain state in an
//! in-process SVM ([`relay_chain_source`]), and read the answer out of
//! logs, return data, and post-simulation account state. What comes back is
//! not an estimate of the split — it is the split, produced by the code that
//! will run on chain, including the margin clamps, the at-or-better
//! rejections, the mandatory-CLOB baseline check, and the CU cost.
//!
//! Cheapness comes from the account feed: subscribe to the CLOB program, the
//! quoter registry, and each live PropAMM (an unfiltered
//! [`relay_chain_source::ProgramSubscription`]) and every account a fill
//! touches is already resident, so a quote costs microseconds and no RPC.

use {
    anchor_lang::Discriminator,
    anyhow::{Context, Result},
    program::state::{prop_amm::QuoterV0, traits::Size},
    relay_chain_source::{AccountFilter, ChainSource, ProgramSubscription, SimOutcome},
    solana_sdk::{pubkey::Pubkey, transaction::Transaction},
};

pub mod health;
pub mod quote_view;

/// Byte length of a `QuoterV0` account, from the program.
pub fn quoter_v0_len() -> u64 {
    QuoterV0::SIZE as u64
}

/// Account-data offset of `QuoterV0::market`, derived from the struct so a
/// field reorder can't silently turn this filter into a wrong-market match.
pub fn quoter_v0_market_offset() -> usize {
    8 + core::mem::offset_of!(QuoterV0, market)
}

/// The `QuoterV0` account discriminator, from the program.
pub fn quoter_v0_discriminator() -> Vec<u8> {
    QuoterV0::DISCRIMINATOR.to_vec()
}

/// What to subscribe to so router simulations stay off the network.
///
/// The registry query is filtered (only `QuoterV0`s, and only this market's
/// when `market_index` is given) so another market's entries never cross the
/// wire; the CLOB and PropAMM programs are subscribed unfiltered, because a
/// fill can touch any of their accounts and residency is the whole point.
pub fn router_subscriptions(
    velocity_program: Pubkey,
    market_index: Option<u16>,
    quoter_programs: &[Pubkey],
) -> Vec<ProgramSubscription> {
    let mut registry_filters = vec![
        AccountFilter::DataSize(quoter_v0_len()),
        AccountFilter::prefix(quoter_v0_discriminator()),
    ];
    if let Some(market) = market_index {
        registry_filters.push(AccountFilter::Memcmp {
            offset: quoter_v0_market_offset(),
            bytes: market.to_le_bytes().to_vec(),
        });
    }
    std::iter::once(ProgramSubscription {
        program: velocity_program,
        filter_sets: vec![registry_filters],
    })
    .chain(
        quoter_programs
            .iter()
            .copied()
            .map(ProgramSubscription::all),
    )
    .collect()
}

/// Every `QuoterV0` registry entry for a market, straight from the feed.
pub async fn quoter_entries<S: ChainSource>(
    source: &S,
    velocity_program: &Pubkey,
    market_index: u16,
) -> Result<Vec<(Pubkey, solana_sdk::account::Account)>> {
    let filters = vec![vec![
        AccountFilter::DataSize(quoter_v0_len()),
        AccountFilter::prefix(quoter_v0_discriminator()),
        AccountFilter::Memcmp {
            offset: quoter_v0_market_offset(),
            bytes: market_index.to_le_bytes().to_vec(),
        },
    ]];
    source
        .get_program_accounts(velocity_program, &filters)
        .await
        .context("fetch quoter registry entries")
}

/// Byte length of a `RouterQuoteBufferV0` account, from the program.
pub fn quote_buffer_len() -> u64 {
    program::state::router_quote::RouterQuoteBufferV0::SIZE as u64
}

/// The `RouterQuoteBufferV0` account discriminator, from the program.
pub fn quote_buffer_discriminator() -> Vec<u8> {
    program::state::router_quote::RouterQuoteBufferV0::DISCRIMINATOR.to_vec()
}

/// Account-data offset of `RouterQuoteBufferV0::market`, derived from the
/// struct so a field reorder can't silently mis-filter.
pub fn quote_buffer_market_offset() -> usize {
    8 + core::mem::offset_of!(program::state::router_quote::RouterQuoteBufferV0, market)
}

/// Find an existing quote buffer for a market and report `(buffer,
/// authority)`.
///
/// Quoting is simulation-only and simulation skips signature verification,
/// so a read-only consumer (an HTTP `/route`, a dashboard) can quote through
/// *any* live buffer — typically the book-publisher's — by naming its stored
/// authority as the instruction's signer, without holding a key or paying
/// the ~33 KB of rent a buffer costs. Writers that land quotes for real
/// still need their own buffer (the authority gate exists so two routers
/// sharing a market don't overwrite each other's reads).
pub async fn find_quote_buffer<S: ChainSource>(
    source: &S,
    velocity_program: &Pubkey,
    market_index: u16,
) -> Result<Option<(Pubkey, Pubkey)>> {
    let filters = vec![vec![
        AccountFilter::DataSize(quote_buffer_len()),
        AccountFilter::prefix(quote_buffer_discriminator()),
        AccountFilter::Memcmp {
            offset: quote_buffer_market_offset(),
            bytes: market_index.to_le_bytes().to_vec(),
        },
    ]];
    let mut buffers = source
        .get_program_accounts(velocity_program, &filters)
        .await
        .context("fetch quote buffers")?;
    // Deterministic pick when several routers share the market.
    buffers.sort_by_key(|(key, _)| *key);
    Ok(buffers.first().map(|(key, account)| {
        let authority = Pubkey::try_from(&account.data[8..40]).expect("32-byte authority");
        (*key, authority)
    }))
}

/// The outcome of simulating a router fill.
#[derive(Debug, Clone)]
pub struct RouterQuote {
    /// `None` when the fill simulated cleanly.
    pub err: Option<String>,
    /// Program logs — the router's per-quoter decisions and any rejection
    /// reason are in here verbatim.
    pub logs: Vec<String>,
    /// Compute units the fill consumed, for sizing the real transaction's
    /// CU limit.
    pub units_consumed: u64,
    /// Post-simulation state of the accounts the caller asked for, in order
    /// — a taker's `User` shows the exact position and fees the fill would
    /// produce, without landing anything.
    pub accounts: Vec<Option<solana_sdk::account::Account>>,
}

impl From<SimOutcome> for RouterQuote {
    fn from(outcome: SimOutcome) -> Self {
        Self {
            err: outcome.err,
            logs: outcome.logs,
            units_consumed: outcome.units_consumed,
            accounts: outcome.accounts,
        }
    }
}

/// Simulate a router fill and report what it would do.
///
/// `fill` is a real `fill_perp_order` transaction — built exactly as it
/// would be sent, quoter section and all. `read_accounts` names the accounts
/// whose post-fill state the caller wants back (typically the taker's
/// `User`, the makers', and the perp market).
///
/// Failure is information, not an error: a fill that trips the baseline
/// check, a margin clamp, or at-or-better comes back with `err` set and the
/// reason in `logs`, which is exactly what a router needs in order to drop a
/// quoter and try again.
pub async fn simulate_router_fill<S: ChainSource>(
    source: &S,
    fill: &Transaction,
    read_accounts: &[Pubkey],
) -> Result<RouterQuote> {
    let outcome = source
        .simulate_transaction(fill, read_accounts)
        .await
        .context("simulate router fill")?;
    Ok(outcome.into())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The registry filter has to match what the chain actually stores, and
    /// a memcmp at a wrong offset silently matches the wrong accounts rather
    /// than failing — so pin both against the program's own layout.
    #[test]
    fn registry_filter_matches_the_program_layout() {
        assert_eq!(quoter_v0_len(), QuoterV0::SIZE as u64);
        assert_eq!(quoter_v0_discriminator().len(), 8);
        // `market` sits after the four pubkeys and the two discriminators
        // and both account arrays; the point of deriving it is that this
        // number moves by itself when the struct changes.
        assert_eq!(
            quoter_v0_market_offset(),
            8 + core::mem::offset_of!(QuoterV0, market)
        );
        assert!(quoter_v0_market_offset() + 2 <= quoter_v0_len() as usize);
    }
}

/// The program's own split, reachable off-chain.
///
/// Selection needs the split evaluated over *candidate subsets* of quoters,
/// which can't be done by simulating fills (combinatorial). Depending on the
/// program as a host library means the router runs the same
/// `split_across_quoters` the chain will run — no port, no mirror to drift.
pub use program::math::router::{split_across_quoters, QuoterAllocation, QuoterBook};
pub use program::state::prop_amm::{Direction, PriceLevel};

/// Asking a book who rests on it.
///
/// A fill settles only for users whose accounts the transaction carries, and
/// a book stores its makers as an authority and a sub-account on an order, so
/// whoever assembles a fill has to learn whose accounts to bring. Reading the
/// book from outside is how that used to work, and it made every caller a
/// second reader of a layout the book is entitled to change.
///
/// So the book answers for itself, through the optional `quote_l3_v0` leg of
/// the quoter interface, and this crate asks it the way it asks a quoter
/// anything: by simulation. A caller already holding a [`quote_view::QuoteView`]
/// wants [`quote_view::QuoteView::settleable_users`] instead — the rows are
/// in the view it already paid for.
pub mod l3 {
    use {
        super::*,
        anyhow::{anyhow, bail},
        program::state::prop_amm::{
            ClobUserRefV0, Direction, L3ArgsV0, L3ResponseV0, QuoterV0, ResponsePointerV0,
        },
        solana_sdk::{
            instruction::{AccountMeta, Instruction},
            message::Message,
        },
    };

    /// Distinct makers a taker of `size` would sweep off `entry`'s book, best
    /// price first.
    ///
    /// The order is the answer, not a detail of it: the book stops at the
    /// first maker the transaction did not carry, so a prefix of this list
    /// fills and a gap forfeits everything behind it. `limit` bounds how many
    /// the caller wants to hear about — every maker costs two accounts.
    ///
    /// Empty when the entry declares no L3 leg, which is every quoter that
    /// fills from the one account its registry entry names: there is nothing
    /// to discover, the caller already has it.
    pub async fn resting_makers<S: ChainSource + ?Sized>(
        source: &S,
        entry: &QuoterV0,
        direction: Direction,
        size: u64,
        limit: usize,
    ) -> Result<Vec<ClobUserRefV0>> {
        if entry.quote_l3_v0_discriminator == [0u8; 8] {
            return Ok(Vec::new());
        }
        let mut data = entry.quote_l3_v0_discriminator.to_vec();
        program::state::prop_amm::write_l3_args(
            &mut data,
            &L3ArgsV0 {
                direction,
                size,
                max_rows: limit.min(u16::MAX as usize) as u16,
            },
        )
        .map_err(|err| anyhow!("serialize l3 args: {err}"))?;

        let book = entry.response_account;
        let payer = Pubkey::new_unique();
        let blockhash = source.latest_blockhash().await?;
        let tx = Transaction::new_unsigned(Message::new_with_blockhash(
            &[Instruction {
                program_id: entry.program_id,
                accounts: vec![AccountMeta::new(book, false)],
                data,
            }],
            Some(&payer),
            &blockhash.hash,
        ));
        // The rows land in the book's own account and return data carries only
        // the pointer, so the simulation has to hand back the account too.
        let outcome = source.simulate_transaction(&tx, &[book]).await?;
        if let Some(err) = outcome.err {
            bail!("l3 simulation failed: {err}");
        }
        let pointer: ResponsePointerV0 = outcome
            .return_data
            .as_deref()
            .map(|bytes| {
                anchor_lang::AnchorDeserialize::deserialize(&mut &bytes[..])
                    .map_err(|_| anyhow!("undecodable l3 response pointer"))
            })
            .transpose()?
            .ok_or_else(|| anyhow!("l3 simulation set no return data"))?;
        let account = outcome
            .accounts
            .first()
            .cloned()
            .flatten()
            .ok_or_else(|| anyhow!("l3 simulation returned no book state"))?;

        let start = pointer.offset as usize;
        let end = start
            .checked_add(pointer.len as usize)
            .ok_or_else(|| anyhow!("l3 response pointer overflows"))?;
        let bytes = account
            .data
            .get(start..end)
            .ok_or_else(|| anyhow!("l3 response pointer out of bounds"))?;
        let response =
            L3ResponseV0::parse(bytes).map_err(|_| anyhow!("undecodable l3 response"))?;

        let mut makers: Vec<ClobUserRefV0> = Vec::new();
        for row in response.rows {
            if makers.len() == limit {
                break;
            }
            if !makers.contains(&row.user) {
                makers.push(row.user);
            }
        }
        Ok(makers)
    }
}
