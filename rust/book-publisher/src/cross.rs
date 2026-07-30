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
        prop_amm::{
            read_clob_node, read_clob_u32, ClobUserRefV0, QuoterV0, CLOB_BEST_ASK_OFFSET,
            CLOB_BEST_BID_OFFSET, CLOB_NIL,
        },
        router_quote::QuotedSourceKind,
        state::State,
        user::User,
    },
    relay_chain_source::ChainSource,
    solana_sdk::{
        instruction::{AccountMeta, Instruction},
        pubkey::Pubkey,
    },
    std::collections::BTreeMap,
    velocity_router_sim::quote_view::{
        perp_market_pda, read_zero_copy, spot_market_pda, state_pda, user_stats_pda,
        velocity_signer_pda, QuoteView, QuotedBook,
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

/// Walk a CLOB side's crossing prefix and collect maker refs, capped. Stops
/// before admitting a maker past the cap and returns the covered size.
fn clob_makers_for(
    data: &[u8],
    head_offset: usize,
    size: u64,
    slot: u64,
    now: i64,
    makers: &mut Vec<ClobUserRefV0>,
) -> Result<u64> {
    let mut covered = 0u64;
    let mut cursor =
        read_clob_u32(data, head_offset).ok_or_else(|| anyhow!("clob header truncated"))?;
    while cursor != CLOB_NIL && covered < size {
        let node = read_clob_node(data, cursor).ok_or_else(|| anyhow!("clob node truncated"))?;
        cursor = node.next;
        if !node.is_matchable(slot, now) {
            continue;
        }
        let user = node.user_ref();
        if !makers.contains(&user) {
            if makers.len() >= MAX_CROSS_MAKERS {
                break;
            }
            makers.push(user);
        }
        covered = covered.saturating_add(node.base_asset_amount);
    }
    Ok(covered.min(size))
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

    // Load both registry entries (they may be the same: an internally
    // crossed CLOB).
    let entry_keys: Vec<Pubkey> = if bid_book.key == ask_book.key {
        vec![bid_book.key]
    } else {
        vec![ask_book.key, bid_book.key]
    };
    let entry_accounts = source.get_multiple_accounts(&entry_keys).await?;
    let entries: Vec<QuoterV0> = entry_keys
        .iter()
        .zip(entry_accounts)
        .map(|(key, account)| {
            let account = account.ok_or_else(|| anyhow!("quoter entry {key} missing"))?;
            read_zero_copy::<QuoterV0>(&account.data)
        })
        .collect::<Result<_>>()?;
    let buy_index = 0u8;
    let sell_index = if entry_keys.len() == 1 { 0u8 } else { 1u8 };

    // Maker pairs per leg, capped; the cross size shrinks to what the
    // staged makers cover.
    let clock = source.clock().await?;
    let mut makers: Vec<ClobUserRefV0> = Vec::new();
    let mut size = cross.size;
    for (leg_entry, head_offset) in [
        (&entries[buy_index as usize], CLOB_BEST_ASK_OFFSET),
        (&entries[sell_index as usize], CLOB_BEST_BID_OFFSET),
    ] {
        if leg_entry.quoter_type == program::state::prop_amm::QuoterType::Clob {
            let book_account = source
                .get_multiple_accounts(&[leg_entry.response_account])
                .await?
                .pop()
                .flatten()
                .ok_or_else(|| anyhow!("clob market account missing"))?;
            let covered = clob_makers_for(
                &book_account.data,
                head_offset,
                size,
                clock.slot,
                clock.unix_timestamp,
                &mut makers,
            )?;
            size = size.min(covered);
        } else {
            let user_account = source
                .get_multiple_accounts(&[leg_entry.user])
                .await?
                .pop()
                .flatten()
                .ok_or_else(|| anyhow!("quoter user account missing"))?;
            let user: User = read_zero_copy(&user_account.data)?;
            let user_ref = ClobUserRefV0 {
                authority: user.authority,
                sub_account_id: user.sub_account_id,
            };
            if !makers.contains(&user_ref) {
                makers.push(user_ref);
            }
        }
    }
    if size == 0 {
        return Ok(None);
    }

    // Assemble the executor call: named accounts, map section, maker
    // (User, UserStats) pairs, quoter entries, then the union of their
    // registered execute-leg CPI accounts.
    let signer = velocity_signer_pda(velocity);
    let protocol_user = Pubkey::find_program_address(
        &[b"user", signer.as_ref(), 0u16.to_le_bytes().as_ref()],
        velocity,
    )
    .0;
    let protocol_user_stats = user_stats_pda(velocity, &signer);

    let mut cpi_union: BTreeMap<Pubkey, bool> = BTreeMap::new();
    for entry in &entries {
        for meta in &entry.execute_accounts[..entry.execute_accounts_count as usize] {
            *cpi_union.entry(meta.pubkey).or_default() |= meta.is_writable;
        }
        *cpi_union.entry(entry.response_account).or_default() |= true;
        cpi_union.entry(entry.program_id).or_default();
    }
    cpi_union.entry(signer).or_default();

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
    for key in &entry_keys {
        accounts.push(AccountMeta::new_readonly(*key, false));
    }
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
