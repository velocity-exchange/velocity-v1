//! Cross-detection fast path: the publisher already recomputes every
//! source's verified book on each tick (and, under a feed, on every
//! registered quote-account change), so it sees a PropAMM quoting through
//! the CLOB — or an internally crossed CLOB — the moment it lands. This
//! module turns that sight into a `crank_cross_match` submission.
//!
//! The publisher is the *fast* path, not the only one: the relay cross
//! conditions (an `OnAccountChange` watch over the book's bests plus an
//! `EverySlots` poll) remain the liveness floor for CLOB×CLOB, and the
//! executor is its own predicate either way — it reverts unless the legs
//! balance and the spread nets positive after both legs' taker fees, so a
//! submission raced by a fill just fails a simulation. PropAMM×CLOB is
//! *only* discoverable here: a fixed four-account relay resolver cannot
//! quote a PropAMM.
//!
//! Maker accounts are derived, never fetched-and-parsed: CLOB nodes carry
//! `(authority, sub_account_id)`, so both the `User` and `UserStats` PDAs
//! of every maker in the crossing prefix come straight from book bytes;
//! a PropAMM leg's pair comes from its registry entry's quoted user.

use {
    anyhow::{anyhow, Context, Result},
    program::state::{
        prop_amm::{ClobUserRefV0, QuoterSlotV0},
        router_quote::QuotedSourceKind,
        state::State,
    },
    relay_chain_source::ChainSource,
    solana_sdk::{
        instruction::{AccountMeta, Instruction},
        pubkey::Pubkey,
    },
    std::collections::BTreeMap,
    velocity_router_sim::{
        quote_view::{
            perp_market_pda, read_zero_copy, spot_market_pda, state_pda, user_stats_pda,
            velocity_signer_pda, QuoteView, QuotedBook,
        },
        quoter_slab_pda, quoter_slab_slots,
    },
};

/// Makers a submitted cross may touch, bounded to keep the transaction
/// small. The walk stops before admitting an unstaged maker, so the sized
/// cross is always covered by the passed user set.
const MAX_CROSS_MAKERS: usize = 8;

fn user_pda(velocity: &Pubkey, user: &ClobUserRefV0) -> Pubkey {
    Pubkey::find_program_address(
        &[
            b"user",
            user.authority.as_ref(),
            user.sub_account_id.to_le_bytes().as_ref(),
        ],
        velocity,
    )
    .0
}

fn crank_conditions_pda(velocity: &Pubkey, market_index: u16) -> Pubkey {
    Pubkey::find_program_address(
        &[
            b"clob_crank_conditions",
            market_index.to_le_bytes().as_ref(),
        ],
        velocity,
    )
    .0
}

/// The crossing prefix between one book's bids and another's asks:
/// matchable size and each leg's gross quote.
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
/// The rows come from the quote view, which is where every source says who
/// its depth belongs to — a book per order, a quoter that fills from one
/// account against that account. Nothing here decodes a book: the same
/// simulation that priced the cross also named the accounts it needs.
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
        // on: the book passes over it while a counterparty crosses it, which
        // is exactly the situation the crank is resolving.
        if !row.is_taker_origin() {
            covered = covered.saturating_add(row.size);
        }
    }
    covered.min(size)
}

/// A submittable cross: the executor instruction and the estimate that
/// justified it.
pub struct CrossPlan {
    pub instruction: Instruction,
    pub size: u64,
    pub estimated_surplus: u128,
}

/// Look for the most profitable cross between any two quoter books in the
/// tick's views and build the `crank_cross_match` call for it. Returns
/// `None` when nothing crosses net of the (tier-0, conservative) fee
/// estimate — the common case, and free to check.
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
    // Candidate legs are quoter books only (the vAMM reprices continuously
    // and cannot rest crossed; the executor rejects it as a leg).
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

    // Conservative fee estimate: tier-0 taker fee on both legs.
    let state_key = state_pda(velocity);
    let state_account = source
        .get_multiple_accounts(&[state_key])
        .await?
        .pop()
        .flatten()
        .ok_or_else(|| anyhow!("state account missing"))?;
    let state: State = read_zero_copy(&state_account.data)?;
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

    // Resolve both legs to their slab slots (they may be the same slot: an
    // internally crossed CLOB). The executor names legs by slab slot index,
    // and it consults a slot because the tail carries its response account.
    let slots = quoter_slab_slots(source, velocity, market_index).await?;
    let slot_for = |entry: Pubkey| -> Result<(u8, QuoterSlotV0)> {
        slots
            .iter()
            .enumerate()
            .find(|(_, slot)| slot.entry == entry)
            .map(|(index, slot)| (index as u8, *slot))
            .ok_or_else(|| anyhow!("quoter {entry} has no slab slot"))
    };
    // A view book's key is the staging entry — the ask book fills the buy
    // leg and the bid book the sell leg.
    let (buy_index, buy_slot) = slot_for(ask_book.key)?;
    let (sell_index, sell_slot) = slot_for(bid_book.key)?;
    let legs: Vec<QuoterSlotV0> = if buy_index == sell_index {
        vec![buy_slot]
    } else {
        vec![buy_slot, sell_slot]
    };

    // Maker pairs per leg, capped; the cross size shrinks to what the staged
    // makers cover. Both legs answer the same way, because the view
    // describes every source the same way.
    let mut makers: Vec<ClobUserRefV0> = Vec::new();
    let mut size = cross.size;
    for book in [ask_book, bid_book] {
        size = size.min(makers_from_rows(book, size, &mut makers));
    }
    if size == 0 {
        return Ok(None);
    }

    // Assemble the executor call: named accounts, map section, maker
    // (User, UserStats) pairs, then the market's slab and the union of the
    // legs' registered CPI accounts. The whole registered list rides rather
    // than the execute leg's subset: each leg resolves its accounts by index
    // into the one list, so carrying the list is what guarantees the resolve.
    let signer = velocity_signer_pda(velocity);
    let protocol_user = Pubkey::find_program_address(
        &[b"user", signer.as_ref(), 0u16.to_le_bytes().as_ref()],
        velocity,
    )
    .0;
    let protocol_user_stats = user_stats_pda(velocity, &signer);

    let mut cpi_union: BTreeMap<Pubkey, bool> = BTreeMap::new();
    for slot in &legs {
        for meta in slot.config.registered_accounts() {
            *cpi_union.entry(meta.pubkey).or_default() |= meta.is_writable;
        }
        *cpi_union.entry(slot.config.response_account).or_default() |= true;
        cpi_union.entry(slot.config.program_id).or_default();
    }

    // Named accounts through the executor's own client struct, so a change
    // to its `#[derive(Accounts)]` shape breaks this builder at compile time.
    let mut accounts = {
        use anchor_lang::ToAccountMetas;
        program::accounts::CrankCrossMatch {
            state: state_key,
            authority: *payout,
            taker: protocol_user,
            taker_stats: protocol_user_stats,
            crank_conditions: crank_conditions_pda(velocity, market_index),
        }
        .to_account_metas(None)
    };
    accounts.push(AccountMeta::new_readonly(*oracle, false));
    accounts.push(AccountMeta::new(
        spot_market_pda(velocity, quote_spot_market_index),
        false,
    ));
    accounts.push(AccountMeta::new(
        perp_market_pda(velocity, market_index),
        false,
    ));
    for maker in &makers {
        accounts.push(AccountMeta::new(user_pda(velocity, maker), false));
        accounts.push(AccountMeta::new(
            user_stats_pda(velocity, &maker.authority),
            false,
        ));
    }
    accounts.push(AccountMeta::new_readonly(
        quoter_slab_pda(velocity, market_index),
        false,
    ));
    for (key, writable) in &cpi_union {
        accounts.push(if *writable {
            AccountMeta::new(*key, false)
        } else {
            AccountMeta::new_readonly(*key, false)
        });
    }

    use anchor_lang::{AnchorSerialize, Discriminator};
    let mut data = program::instruction::CrankCrossMatch::DISCRIMINATOR.to_vec();
    market_index
        .serialize(&mut data)
        .context("serialize cross args")?;
    size.serialize(&mut data)?;
    buy_index.serialize(&mut data)?;
    sell_index.serialize(&mut data)?;

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
