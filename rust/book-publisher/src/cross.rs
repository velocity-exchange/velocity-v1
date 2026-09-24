//! The cross-detection fast path. The publisher already recomputes every
//! source's verified book on each tick, and under a feed on every registered
//! quote-account change. It therefore sees a PropAMM quoting through the CLOB,
//! or an internally crossed CLOB, the moment it lands. This module turns that
//! observation into a `crank_cross_match` submission.
//!
//! The publisher is the fast path, not the only one. The relay cross
//! conditions, an `OnAccountChange` watch over the book's bests plus an
//! `EverySlots` poll, remain the liveness floor for CLOB against CLOB. The
//! executor is its own predicate either way. It reverts unless the legs
//! balance, every unit of the size crossed, and the spread nets positive after
//! both legs' taker fees, so a submission that a fill races only fails a
//! simulation. The size a plan carries is therefore the crossing depth and
//! never more. A larger size runs its tail through levels that do not cross,
//! and the crank refuses it. A PropAMM against a CLOB is discoverable only
//! here, because a fixed four-account relay resolver cannot quote a PropAMM.
//!
//! Maker accounts are derived rather than fetched and parsed. CLOB nodes carry
//! `(authority, sub_account_id)`, so both the `User` and the `UserStats` PDA of
//! every maker in the crossing prefix come straight from book bytes. A PropAMM
//! leg's pair comes from its registry entry's quoted user.

use {
    anyhow::{anyhow, Result},
    program::{
        instructions::CrankCrossMatchArgs,
        state::{
            prop_amm::{ClobUserRefV0, QuoterSlotV0},
            router_quote::QuotedSourceKind,
            state::State,
        },
    },
    relay_chain_source::ChainSource,
    solana_sdk::{
        instruction::{AccountMeta, Instruction},
        pubkey::Pubkey,
    },
    velocity_router_sim::{
        pdas,
        quote_view::{
            cpi_account_metas, fetch_zero_copy, perp_market_pda, quoter_cpi_union, read_zero_copy,
            spot_market_pda, state_pda, user_stats_pda, QuoteView, QuotedBook,
        },
        quoter_slab_pda, quoter_slab_slots,
    },
};

/// Makers a submitted cross may touch, bounded to keep the transaction
/// small. The walk stops before admitting an unstaged maker, so the sized
/// cross is always covered by the passed user set.
const MAX_CROSS_MAKERS: usize = 8;

/// The crossing prefix between one book's bids and another's asks. It holds
/// the matchable size and each leg's gross quote.
struct LevelCross {
    size: u64,
    buy_quote: u128,
    sell_quote: u128,
}

fn cross_levels(bids: &QuotedBook, asks: &QuotedBook, base_precision: u128) -> LevelCross {
    let mut cross = LevelCross {
        size: 0,
        buy_quote: 0,
        sell_quote: 0,
    };
    let mut bid_iter = bids.levels.iter();
    let mut ask_iter = asks.levels.iter();
    let (mut bid, mut ask) = (bid_iter.next(), ask_iter.next());
    let (mut bid_left, mut ask_left) = (
        bid.map(|level| level.size).unwrap_or(0),
        ask.map(|level| level.size).unwrap_or(0),
    );

    while let (Some(b), Some(a)) = (bid, ask) {
        if b.price < a.price {
            break;
        }

        let take = bid_left.min(ask_left);
        cross.size += take;
        cross.buy_quote += a.price as u128 * take as u128 / base_precision;
        cross.sell_quote += b.price as u128 * take as u128 / base_precision;
        bid_left -= take;
        ask_left -= take;
        if bid_left == 0 {
            bid = bid_iter.next();
            bid_left = bid.map(|level| level.size).unwrap_or(0);
        }
        if ask_left == 0 {
            ask = ask_iter.next();
            ask_left = ask.map(|level| level.size).unwrap_or(0);
        }
    }

    cross
}

/// Collect the makers behind a leg's crossing prefix, capped, and report the
/// size that prefix covers.
///
/// The rows come from the quote view, which is where every source says who its
/// depth belongs to. A book says so per order. A quoter that fills from one
/// account says so against that account. Nothing here decodes a book, because
/// the same simulation that priced the cross also named the accounts it needs.
fn makers_from_rows(book: &QuotedBook, size: u64, makers: &mut Vec<ClobUserRefV0>) -> u64 {
    let mut covered = 0u64;
    for row in &book.rows {
        if !makers.contains(&row.user) {
            if makers.len() >= MAX_CROSS_MAKERS {
                break;
            }

            makers.push(row.user);
        }

        // A crossed taker-origin remainder is not depth this cross can count
        // on. The book passes over it while a counterparty crosses it, and that
        // is the situation the crank is resolving.
        if !row.is_taker_origin() {
            covered = covered.saturating_add(row.size);
        }
    }

    covered.min(size)
}

/// A submittable cross. It holds the executor instruction and the estimate that
/// justified it.
pub struct CrossPlan {
    pub instruction: Instruction,
    pub size: u64,
    pub estimated_surplus: u128,
}

/// Look for the most profitable cross between any two quoter books in the
/// tick's views, and build the `crank_cross_match` call for it. Returns `None`
/// when nothing crosses net of the conservative tier-0 fee estimate. That is
/// the common case, and the check is free.
#[allow(clippy::too_many_arguments)]
pub async fn find_cross_plan<S: ChainSource + ?Sized>(
    source: &S,
    velocity: &Pubkey,
    payout: &Pubkey,
    market_index: u16,
    oracle: &Pubkey,
    quote_spot_market_index: u16,
    base_precision: u128,
    asks: &QuoteView,
    bids: &QuoteView,
) -> Result<Option<CrossPlan>> {
    // Candidate legs are quoter books only. The vAMM reprices continuously and
    // cannot rest crossed, and the executor rejects it as a leg.
    let bid_books: Vec<&QuotedBook> = bids
        .books
        .iter()
        .filter(|book| book.kind == QuotedSourceKind::Quoter && !book.levels.is_empty())
        .collect();
    let ask_books: Vec<&QuotedBook> = asks
        .books
        .iter()
        .filter(|book| book.kind == QuotedSourceKind::Quoter && !book.levels.is_empty())
        .collect();
    if bid_books.is_empty() || ask_books.is_empty() {
        return Ok(None);
    }

    // The conservative fee estimate is the tier-0 taker fee on both legs.
    let state_key = state_pda(velocity);
    let state: State = fetch_zero_copy(source, &state_key, "state").await?;
    let tier = state.perp_fee_structure.fee_tiers[0];
    let (fee_numerator, fee_denominator) = (
        tier.fee_numerator as u128,
        (tier.fee_denominator as u128).max(1),
    );

    let mut best: Option<(&QuotedBook, &QuotedBook, LevelCross, u128)> = None;
    for bid_book in &bid_books {
        for ask_book in &ask_books {
            let cross = cross_levels(bid_book, ask_book, base_precision);
            if cross.size == 0 {
                continue;
            }

            let fees = (cross.buy_quote * fee_numerator).div_ceil(fee_denominator)
                + (cross.sell_quote * fee_numerator).div_ceil(fee_denominator);
            let Some(surplus) = cross
                .sell_quote
                .checked_sub(cross.buy_quote.saturating_add(fees))
            else {
                continue;
            };

            if surplus == 0 {
                continue;
            }
            if best.as_ref().is_none_or(|(_, _, _, s)| surplus > *s) {
                best = Some((bid_book, ask_book, cross, surplus));
            }
        }
    }

    let Some((bid_book, ask_book, cross, estimated_surplus)) = best else {
        return Ok(None);
    };

    // Resolve both sides to their slab slots. They may be the same slot, which
    // is an internally crossed CLOB. The crank names no legs, because each of
    // its two fills routes across every source the tail carries. The slots are
    // therefore only for the account union below.
    let slots = quoter_slab_slots(source, velocity, market_index).await?;
    let slot_for = |entry: Pubkey| -> Result<QuoterSlotV0> {
        slots
            .iter()
            .find(|slot| slot.entry == entry)
            .copied()
            .ok_or_else(|| anyhow!("quoter {entry} has no slab slot"))
    };

    // A view book's key is the staging entry.
    let legs: Vec<QuoterSlotV0> = if ask_book.key == bid_book.key {
        vec![slot_for(ask_book.key)?]
    } else {
        vec![slot_for(ask_book.key)?, slot_for(bid_book.key)?]
    };

    // Maker pairs per leg, capped. The cross size shrinks to what the staged
    // makers cover. Both legs answer the same way, because the view describes
    // every source the same way.
    let mut makers: Vec<ClobUserRefV0> = Vec::new();
    let mut size = cross.size;
    for book in [ask_book, bid_book] {
        size = size.min(makers_from_rows(book, size, &mut makers));
    }

    if size == 0 {
        return Ok(None);
    }

    // Assemble the executor call. It carries the named accounts, the map
    // section, the maker (User, UserStats) pairs, and then the union of the
    // legs' registered CPI accounts. The whole registered list rides rather
    // than the execute leg's subset, because each leg resolves its accounts by
    // index into that one list. The slab rides the union as well as being
    // named, because each leg assembles its route from the tail and a route
    // without the slab consults nothing external. The perp market is named
    // only.
    let protocol = pdas::protocol_user_pair(velocity);

    // The slab rides read-only on its own, because the executor consults it
    // whether or not a leg registered it.
    let mut cpi_union = quoter_cpi_union(&legs);
    cpi_union
        .entry(quoter_slab_pda(velocity, market_index))
        .or_default();

    // Named accounts through the executor's own client struct, so a change
    // to its `#[derive(Accounts)]` shape breaks this builder at compile time.
    let mut accounts = {
        use anchor_lang::ToAccountMetas;
        program::accounts::CrankCrossMatch {
            state: state_key,
            authority: *payout,
            taker: protocol.user,
            taker_stats: protocol.stats,
            crank_conditions: pdas::clob_crank_conditions(velocity, market_index),
            perp_market: perp_market_pda(velocity, market_index),
            quoter_slab: quoter_slab_pda(velocity, market_index),
            instructions_sysvar: solana_sdk::sysvar::instructions::ID,
        }
        .to_account_metas(None)
    };

    accounts.push(AccountMeta::new_readonly(*oracle, false));
    accounts.push(AccountMeta::new(
        spot_market_pda(velocity, quote_spot_market_index),
        false,
    ));

    for maker in &makers {
        accounts.push(AccountMeta::new(pdas::user_of(velocity, maker), false));
        accounts.push(AccountMeta::new(
            user_stats_pda(velocity, &maker.authority),
            false,
        ));
    }
    accounts.extend(cpi_account_metas(&cpi_union));

    use anchor_lang::InstructionData;
    let data = program::instruction::CrankCrossMatch {
        args: CrankCrossMatchArgs { market_index, size },
    }
    .data();

    Ok(Some(CrossPlan {
        instruction: Instruction {
            program_id: *velocity,
            accounts,
            data,
        },

        size,
        estimated_surplus,
    }))
}
