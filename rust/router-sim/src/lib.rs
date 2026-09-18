//! Quote a perp market's router liquidity by simulating the fill. This crate
//! does not decode books off chain.
//!
//! The router splits a taker across the vAMM, resting DLOB orders, the CLOB,
//! and any registered PropAMM. It learns each external quoter's prices by CPI
//! into `quote_v0`. A Custom quoter is an arbitrary third-party program, so
//! there is no general way to decode its book from outside. A call is the only
//! way to price it. Simulation is therefore the only correct approach, and it
//! covers the CLOB and the vAMM as well.
//!
//! This crate builds the real `fill_perp_order` transaction, runs it against
//! cached chain state in an in-process SVM ([`relay_chain_source`]), and reads
//! the answer out of logs, return data, and post-simulation account state. The
//! result is the split the on-chain code produces. It includes the margin
//! clamps, the at-or-better rejections, the mandatory CLOB baseline check, and
//! the compute unit cost.
//!
//! The account feed keeps a quote cheap. Subscribe to the CLOB program, the
//! per-market quoter slabs, and each live PropAMM through an unfiltered
//! [`relay_chain_source::ProgramSubscription`]. Every account a fill touches is
//! then resident, so a quote costs microseconds and no RPC call.

use {
    anchor_lang::Discriminator,
    anyhow::{Context, Result},
    program::state::{
        prop_amm::{QuoterSlabV0, QuoterSlotV0, QUOTER_SLAB_PDA_SEED},
        traits::Size,
    },
    relay_chain_source::{AccountFilter, ChainSource, ProgramSubscription, SimOutcome},
    solana_sdk::{pubkey::Pubkey, transaction::Transaction},
};

pub mod health;
pub mod quote_view;

/// PDA of a market's [`QuoterSlabV0`]. There is one per market, and it holds
/// every approved quoter config. Fills and quote views read quoters from it.
pub fn quoter_slab_pda(velocity: &Pubkey, market_index: u16) -> Pubkey {
    Pubkey::find_program_address(
        &[QUOTER_SLAB_PDA_SEED, market_index.to_le_bytes().as_ref()],
        velocity,
    )
    .0
}

/// Account-data offset of `QuoterSlabV0::market`. The offset comes from the
/// struct, so a field reorder cannot turn this filter into a wrong-market match.
pub fn quoter_slab_market_offset() -> usize {
    8 + core::mem::offset_of!(QuoterSlabV0, market)
}

/// The `QuoterSlabV0` account discriminator, from the program.
pub fn quoter_slab_discriminator() -> Vec<u8> {
    QuoterSlabV0::DISCRIMINATOR.to_vec()
}

/// Decode a slab account's slot region. The region holds the fixed header, then
/// `capacity` back-to-back [`QuoterSlotV0`] values. Vacant slots stay in the
/// result, so an index into it is the on-chain slot index. A `crank_cross_match`
/// leg names its quoter by that index.
pub fn decode_quoter_slab_slots(data: &[u8]) -> Result<Vec<QuoterSlotV0>> {
    let header: QuoterSlabV0 = quote_view::read_zero_copy(data)?;
    let slot_size = core::mem::size_of::<QuoterSlotV0>();
    let region = data
        .get(QuoterSlabV0::SLOT_REGION_OFFSET..)
        .and_then(|tail| tail.get(..header.capacity as usize * slot_size))
        .ok_or_else(|| {
            anyhow::anyhow!(
                "quoter slab declares {} slots but is too short to hold them",
                header.capacity
            )
        })?;
    Ok(region
        .chunks_exact(slot_size)
        .map(bytemuck::pod_read_unaligned::<QuoterSlotV0>)
        .collect())
}

/// The market's slab slots, read from the feed. The result is empty when the
/// market has no slab account yet. That reads the same as an all-vacant slab,
/// which means no approved quoters.
pub async fn quoter_slab_slots<S: ChainSource + ?Sized>(
    source: &S,
    velocity_program: &Pubkey,
    market_index: u16,
) -> Result<Vec<QuoterSlotV0>> {
    let slab = quoter_slab_pda(velocity_program, market_index);
    let account = source
        .get_multiple_accounts(&[slab])
        .await
        .context("fetch quoter slab")?
        .pop()
        .flatten();
    match account {
        Some(account) => decode_quoter_slab_slots(&account.data),
        None => Ok(Vec::new()),
    }
}

/// What to subscribe to so router simulations stay off the network.
///
/// The slab query is filtered to `QuoterSlabV0` accounts, and to this market
/// alone when `market_index` is given, so another market's slab never crosses
/// the wire. The CLOB and PropAMM programs are subscribed without a filter,
/// because a fill can touch any of their accounts and all of them must be
/// resident.
pub fn router_subscriptions(
    velocity_program: Pubkey,
    market_index: Option<u16>,
    quoter_programs: &[Pubkey],
) -> Vec<ProgramSubscription> {
    let mut slab_filters = vec![AccountFilter::prefix(quoter_slab_discriminator())];
    if let Some(market) = market_index {
        slab_filters.push(AccountFilter::Memcmp {
            offset: quoter_slab_market_offset(),
            bytes: market.to_le_bytes().to_vec(),
        });
    }
    std::iter::once(ProgramSubscription {
        program: velocity_program,
        filter_sets: vec![slab_filters],
    })
    .chain(
        quoter_programs
            .iter()
            .copied()
            .map(ProgramSubscription::all),
    )
    .collect()
}

/// Byte length of a `RouterQuoteBufferV0` account, from the program.
pub fn quote_buffer_len() -> u64 {
    program::state::router_quote::RouterQuoteBufferV0::SIZE as u64
}

/// The `RouterQuoteBufferV0` account discriminator, from the program.
pub fn quote_buffer_discriminator() -> Vec<u8> {
    program::state::router_quote::RouterQuoteBufferV0::DISCRIMINATOR.to_vec()
}

/// Account-data offset of `RouterQuoteBufferV0::market`. The offset comes from
/// the struct, so a field reorder cannot produce a wrong filter.
pub fn quote_buffer_market_offset() -> usize {
    8 + core::mem::offset_of!(program::state::router_quote::RouterQuoteBufferV0, market)
}

/// Find an existing quote buffer for a market and report the buffer and its
/// authority.
///
/// A quote runs in simulation only, and simulation skips signature
/// verification. A read-only consumer such as an HTTP route handler or a
/// dashboard can therefore quote through any live buffer, which is usually the
/// book publisher's. It names the stored authority as the instruction signer.
/// It needs no key, and it pays none of the roughly 33 KB of rent a buffer
/// costs. A writer that lands quotes on chain still needs its own buffer. The
/// authority gate stops two routers on one market from overwriting each other's
/// reads.
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
    /// Program logs. They carry the router's per-quoter decisions and any
    /// rejection reason verbatim.
    pub logs: Vec<String>,
    /// Compute units the fill consumed, for sizing the real transaction's
    /// CU limit.
    pub units_consumed: u64,
    /// Post-simulation state of the accounts the caller asked for, in the same
    /// order. A taker's `User` shows the position and the fees the fill would
    /// produce, with nothing landed on chain.
    /// This is the 4.x `solana_account::Account`, which is what
    /// relay-chain-source returns. It is not `solana_sdk::account::Account`:
    /// the workspace holds solana-sdk at 3.x for anchor, so the two are
    /// distinct types even though they describe the same account.
    pub accounts: Vec<Option<solana_account::Account>>,
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
/// `fill` is a real `fill_perp_order` transaction, built as it would be sent,
/// including the quoter section. `read_accounts` names the accounts whose
/// post-fill state the caller wants back. That is usually the taker's `User`,
/// the makers' accounts, and the perp market.
///
/// A failed fill is still a successful call. A fill that trips the baseline
/// check, a margin clamp, or at-or-better comes back with `err` set and the
/// reason in `logs`. A router reads that to drop a quoter and try again.
pub async fn simulate_router_fill<S: ChainSource>(
    source: &S,
    fill: &Transaction,
    read_accounts: &[Pubkey],
) -> Result<RouterQuote> {
    let outcome = source
        .simulate_transaction(&as_versioned(fill)?, read_accounts)
        .await
        .context("simulate router fill")?;
    Ok(outcome.into())
}

/// A legacy transaction in the 4.x envelope chain-source takes.
///
/// This workspace builds transactions with solana-sdk 3, because anchor 1.0
/// holds solana-program at 3 and the instruction and pubkey types have to
/// match the program's. chain-source is on the 4.x line, so its
/// `VersionedTransaction` is a different type from anything here, and no
/// `From` impl spans the two. The bytes do span them: a legacy transaction's
/// wire form is the same in both, and a `VersionedTransaction` reads a message
/// with no version prefix as legacy, which is how one comes off the network.
/// So the bridge is the encoding rather than a conversion, and it is here in
/// one place rather than at each call site.
pub fn as_versioned(
    tx: &Transaction,
) -> Result<solana_transaction::versioned::VersionedTransaction> {
    let bytes = bincode::serialize(tx).context("serialize transaction")?;
    bincode::deserialize(&bytes).context("read transaction back as versioned")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The slab filter must match what the chain stores. A memcmp at a wrong
    /// offset matches the wrong accounts instead of failing, so pin both the
    /// discriminator and the offset against the program's own layout.
    #[test]
    fn slab_filter_matches_the_program_layout() {
        assert_eq!(quoter_slab_discriminator().len(), 8);
        // `market` is the header's first field, right after the discriminator.
        // The derived offset moves by itself when the struct changes.
        assert_eq!(quoter_slab_market_offset(), 8);
        assert!(quoter_slab_market_offset() + 2 <= QuoterSlabV0::SLOT_REGION_OFFSET);
    }

    /// The decoder reads the slot region out of raw account bytes, so pin it
    /// against the program's own layout with a round trip.
    #[test]
    fn slab_slots_round_trip_through_the_decoder() {
        let capacity = 3usize;
        let mut header = QuoterSlabV0::default();
        header.market = 7;
        header.capacity = capacity as u16;
        let mut slots = vec![QuoterSlotV0::default(); capacity];
        slots[0].entry = Pubkey::new_unique();
        slots[0].config.market = 7;
        slots[2].entry = Pubkey::new_unique();
        slots[2].suspended = true;

        let mut data = QuoterSlabV0::DISCRIMINATOR.to_vec();
        data.extend_from_slice(bytemuck::bytes_of(&header));
        data.extend_from_slice(bytemuck::cast_slice(&slots));

        let decoded = decode_quoter_slab_slots(&data).unwrap();
        assert_eq!(decoded, slots);

        // A slab shorter than its declared capacity is refused rather than
        // read past the end.
        data.truncate(data.len() - 1);
        assert!(decode_quoter_slab_slots(&data).is_err());
    }
}

/// The program's own split, reachable off chain.
///
/// Selection evaluates the split over candidate subsets of quoters. The number
/// of subsets makes one simulated fill per subset impractical. This crate
/// depends on the program as a host library, so the router runs the same
/// `split_across_quoters` the chain runs. There is no port and no mirror that
/// can drift.
pub use program::math::router::{split_across_quoters, QuoterAllocation, QuoterBook};
pub use program::state::prop_amm::{Direction, PriceLevel};

/// Ask a book which makers rest on it.
///
/// A fill settles only for users whose accounts the transaction carries. A book
/// stores each maker as an authority and a sub-account on an order, so whoever
/// assembles a fill must learn which accounts to bring. Reading the book from
/// outside makes every caller a second reader of a layout the book may change.
///
/// The book answers for itself instead, through the optional `quote_l3_v0` leg
/// of the quoter interface. This crate asks it by simulation, the same way it
/// asks a quoter anything else. A caller that already holds a
/// [`quote_view::QuoteView`] wants
/// [`quote_view::QuoteView::settleable_users`], because the rows are already in
/// that view.
pub mod l3 {
    use {
        super::*,
        anyhow::{anyhow, bail},
        program::state::prop_amm::{
            ClobUserRefV0, Direction, L3ArgsV0, L3ResponseV0, QuoterConfigV0, ResponsePointerV0,
        },
        solana_sdk::{
            instruction::{AccountMeta, Instruction},
            message::Message,
        },
    };

    /// Distinct makers a taker of `size` would sweep off a quoter's book, best
    /// price first. `config` is the quoter's approved config, read from its slab
    /// slot.
    ///
    /// The order matters. The book stops at the first maker the transaction did
    /// not carry, so a prefix of this list fills and a gap forfeits every maker
    /// behind it. `limit` bounds how many rows the caller wants. Every maker
    /// costs two accounts.
    ///
    /// The result is empty when the entry declares no L3 leg. That is every
    /// quoter that fills from the one account its registry entry names. The
    /// caller already holds that account.
    pub async fn resting_makers<S: ChainSource + ?Sized>(
        source: &S,
        config: &QuoterConfigV0,
        direction: Direction,
        size: u64,
        limit: usize,
    ) -> Result<Vec<ClobUserRefV0>> {
        if config.quote_l3_v0_discriminator == [0u8; 8] {
            return Ok(Vec::new());
        }
        let mut data = config.quote_l3_v0_discriminator.to_vec();
        program::state::prop_amm::write_l3_args(
            &mut data,
            &L3ArgsV0 {
                direction,
                size,
                max_rows: limit.min(u16::MAX as usize) as u16,
                // A view of what a caller may take, so the depth a taker
                // remainder claims stays out of it.
                consume_reservation: false,
            },
        )
        .map_err(|err| anyhow!("serialize l3 args: {err}"))?;

        let book = config.response_account;
        let payer = Pubkey::new_unique();
        let blockhash = source.latest_blockhash().await?;
        let tx = Transaction::new_unsigned(Message::new_with_blockhash(
            &[Instruction {
                program_id: config.program_id,
                accounts: vec![AccountMeta::new(book, false)],
                data,
            }],
            Some(&payer),
            &blockhash.hash,
        ));
        // The rows land in the book's own account and return data carries only
        // the pointer, so the simulation has to hand back the account too.
        let outcome = source
            .simulate_transaction(&crate::as_versioned(&tx)?, &[book])
            .await?;
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
