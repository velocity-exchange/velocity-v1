//! Cross-match crank: fill two crossed resting sources against each other.
//!
//! Nothing else matches two *resting* books — router matching only happens
//! when a taker fills through — so a CLOB bid at/above a CLOB ask, or a
//! PropAMM quoting through the CLOB's best, would rest crossed forever.
//! Trigger placements make the first routine and PropAMM reprices the
//! second; both strand user orders, which is the UX this crank exists for.
//!
//! The protocol `User` is the pass-through taker (the arb bot): buy the
//! crossed ask, sell into the crossed bid, both legs through the standard
//! external-match settlement, so every maker experiences an ordinary fill.
//! The executor is the authoritative predicate — it reverts unless the legs
//! balance exactly and the spread nets positive after both legs' taker
//! fees, so a simulation that succeeds implies a profitable cross and books
//! whose cross is inside the fee gulf simply rest. The surplus lands in the
//! protocol `User` — the sink the crank incentive loop drains — and the
//! caller's `authority` is paid reservoir lamports; no signature is
//! required anywhere (relay turners submit executors unsigned).

use {
    super::crank_common::{
        derive_protocol_user_pdas, derive_user_pdas, next_matchable, no_work, to_account_refs,
        validate_linkage, ResolveClobCrank, MAX_CROSS_MAKERS,
    },
    crate::{
        controller,
        error::ErrorCode,
        instructions::{
            constraints::*,
            optional_accounts::{load_maps, AccountMaps},
            router::cpi_executor::CpiQuoterExecutor,
        },
        load, msg,
        state::{
            clob_crank::{ClobCrankConditionsV0, CLOB_CRANK_CONDITIONS_PDA_SEED},
            perp_market_map::{get_writable_perp_market_set, MarketSet},
            prop_amm::{QuoterType, QuoterV0},
            state::State,
            user::{User, UserStats},
            user_map::load_user_maps,
        },
        validate,
    },
    anchor_lang::{prelude::*, Discriminator},
    relay_spec::{ResolvedCrankV0, KEEPER_PLACEHOLDER},
    std::collections::BTreeMap,
};

#[derive(Accounts)]
#[instruction(market_index: u16)]
pub struct CrankCrossMatch<'info> {
    pub state: AccountLoader<'info, State>,
    /// CHECK: the lamport payout target — relay's keeper-placeholder slot.
    /// No signature: the executor's own profitability predicate is the gate.
    #[account(mut)]
    pub authority: UncheckedAccount<'info>,
    /// The protocol-owned pass-through taker. Locked to the protocol `User`
    /// so the reservoir never pays for someone else's private arb.
    #[account(
        mut,
        constraint = is_protocol_user(&taker, &state)?
    )]
    pub taker: AccountLoader<'info, User>,
    #[account(
        mut,
        constraint = is_stats_for_user(&taker, &taker_stats)?
    )]
    pub taker_stats: AccountLoader<'info, UserStats>,
    /// The market's conditions account: the reservoir that pays the keeper.
    #[account(
        mut,
        seeds = [
            CLOB_CRANK_CONDITIONS_PDA_SEED,
            market_index.to_le_bytes().as_ref(),
        ],
        bump
    )]
    pub crank_conditions: AccountLoader<'info, ClobCrankConditionsV0>,
}

pub fn handle_crank_cross_match<'c: 'info, 'info>(
    ctx: Context<'info, CrankCrossMatch<'info>>,
    market_index: u16,
    size: u64,
    buy_quoter_index: u8,
    sell_quoter_index: u8,
) -> Result<()> {
    let clock = Clock::get()?;
    let state = ctx.accounts.state.load()?;

    let remaining_accounts_iter = &mut ctx.remaining_accounts.iter().peekable();
    let AccountMaps {
        perp_market_map,
        spot_market_map,
        mut oracle_map,
    } = load_maps(
        remaining_accounts_iter,
        &get_writable_perp_market_set(market_index),
        &MarketSet::new(),
        clock.slot,
        Some(state.oracle_guard_rails),
    )?;
    let (makers_and_referrer, makers_and_referrer_stats) =
        load_user_maps(remaining_accounts_iter, true)?;

    // Quoter section: registry entries plus the union of their registered
    // CPI accounts, same shape as the router fill's.
    let leftover: Vec<&AccountInfo<'info>> = remaining_accounts_iter.collect();
    let account_map: BTreeMap<Pubkey, AccountInfo<'info>> = leftover
        .iter()
        .map(|info| (*info.key, (*info).clone()))
        .collect();
    let quoters: Vec<AccountLoader<QuoterV0>> = leftover
        .iter()
        .filter(|info| {
            info.owner == &crate::ID
                && info
                    .try_borrow_data()
                    .is_ok_and(|data| data.get(..8) == Some(QuoterV0::DISCRIMINATOR))
        })
        .map(|info| AccountLoader::try_from(*info))
        .collect::<Result<_>>()?;

    let mut types: Vec<QuoterType> = Vec::with_capacity(quoters.len());
    let mut quoter_users: Vec<Pubkey> = Vec::with_capacity(quoters.len());
    for loader in &quoters {
        let quoter = loader.load()?;
        validate!(
            quoter.market == market_index,
            ErrorCode::DefaultError,
            "quoter entry {} is for market {}, cross is for market {}",
            loader.key(),
            quoter.market,
            market_index
        )?;
        validate!(
            quoter.quoter_type != QuoterType::Vamm,
            ErrorCode::DefaultError,
            "the vAMM reprices continuously and cannot rest crossed"
        )?;
        types.push(quoter.quoter_type);
        quoter_users.push(quoter.user);
    }
    let buy_index = buy_quoter_index as usize;
    let sell_index = sell_quoter_index as usize;
    validate!(
        buy_index < quoters.len() && sell_index < quoters.len(),
        ErrorCode::DefaultError,
        "cross leg index out of range: {} / {} of {}",
        buy_index,
        sell_index,
        quoters.len()
    )?;

    let taker_ref = {
        let taker = load!(ctx.accounts.taker)?;
        crate::state::prop_amm::ClobUserRefV0 {
            authority: taker.authority,
            sub_account_id: taker.sub_account_id,
        }
    };
    let users: Vec<crate::state::prop_amm::ClobUserRefV0> = makers_and_referrer
        .user_ref_index()?
        .into_keys()
        .map(
            |(authority, sub_account_id)| crate::state::prop_amm::ClobUserRefV0 {
                authority,
                sub_account_id,
            },
        )
        .collect();
    let mut executor = CpiQuoterExecutor {
        quoters: &quoters,
        types,
        quoter_users,
        account_map: &account_map,
        velocity_signer: state.signer,
        signer_nonce: state.signer_nonce,
        users,
        taker: taker_ref,
    };

    let (base_matched, surplus) = controller::orders::cross_match(
        &state,
        market_index,
        size,
        buy_index,
        sell_index,
        &ctx.accounts.taker,
        &ctx.accounts.taker_stats,
        &makers_and_referrer,
        &makers_and_referrer_stats,
        &mut executor,
        &perp_market_map,
        &spot_market_map,
        &mut oracle_map,
        &clock,
    )?;

    // Repair the wake hints from the post-match book when a leg was the
    // CLOB (a matched activation is exactly when the activation hint fires;
    // this is what sends it forward to the next pending slot).
    let clob_book = quoters
        .iter()
        .zip(&executor.types)
        .find(|(_, quoter_type)| **quoter_type == QuoterType::Clob)
        .map(|(loader, _)| loader.load().map(|quoter| quoter.response_account))
        .transpose()?;
    let payment = {
        let mut conditions = crate::load_mut!(ctx.accounts.crank_conditions)?;
        if let Some(book_key) = clob_book {
            if let Some(book) = account_map.get(&book_key) {
                let (min_expiry, min_activation) =
                    crate::state::prop_amm::clob_hint_scan(&book.try_borrow_data()?, clock.slot);
                conditions.repair_expiry(min_expiry)?;
                conditions.repair_activation(min_activation)?;
            }
        }
        // The keeper's fee, so relay's assert_paid_v0 has a balance to
        // measure.
        conditions.keeper_payment_lamports
    };
    let conditions_info = ctx.accounts.crank_conditions.to_account_info();
    let rent_minimum = Rent::get()?.minimum_balance(conditions_info.data_len());
    ClobCrankConditionsV0::pay_keeper_lamports(
        &conditions_info,
        &ctx.accounts.authority.to_account_info(),
        payment,
        rent_minimum,
    )?;

    msg!(
        "cross matched {} base for {} quote surplus on market {}",
        base_matched,
        surplus,
        market_index
    );
    Ok(())
}

/// Resolver for the cross conditions: find the book's crossing prefix,
/// estimate profitability with the top (most conservative) taker-fee tier
/// on both legs, and stage the `crank_cross_match` executor — full
/// `(User, UserStats)` pairs, both derived from the node's `(authority,
/// sub_account_id)` identity. Only CLOB×CLOB is discoverable here — a
/// PropAMM crossing the CLOB is the generic quoter-cross resolver's job
/// (it CPIs `quote_v0` through the entry's registered surface), with the
/// book publisher as the fast path. The executor re-verifies profitability
/// exactly either way.
pub fn handle_resolve_crank_cross_match(ctx: Context<ResolveClobCrank>) -> Result<()> {
    validate_linkage(&ctx)?;
    let clock = Clock::get()?;
    let cross = {
        let data = ctx.accounts.clob_market.try_borrow_data()?;
        find_clob_cross(&data, clock.slot, clock.unix_timestamp)?
    };
    if cross.size == 0 {
        return no_work();
    }

    // Conservative estimate: tier-0 taker fee on both legs. The executor
    // measures the real thing; this only avoids staging obvious losers.
    let (fee_numerator, fee_denominator) = {
        let state = ctx.accounts.state.load()?;
        let tier = state.perp_fee_structure.fee_tiers[0];
        (
            tier.fee_numerator as u128,
            (tier.fee_denominator as u128).max(1),
        )
    };
    let fees = (cross.buy_quote * fee_numerator).div_ceil(fee_denominator)
        + (cross.sell_quote * fee_numerator).div_ceil(fee_denominator);
    if cross.sell_quote <= cross.buy_quote.saturating_add(fees) {
        return no_work();
    }

    let (market_index, oracle, quote_spot_market_index) = {
        let conditions = ctx.accounts.crank_conditions.load()?;
        (
            conditions.market_index,
            conditions.oracle,
            conditions.quote_spot_market_index,
        )
    };
    let signer = ctx.accounts.state.load()?.signer;
    let (protocol_user, protocol_user_stats) = derive_protocol_user_pdas(&signer);
    let (perp_market, _) = Pubkey::find_program_address(
        &[b"perp_market", market_index.to_le_bytes().as_ref()],
        &crate::ID,
    );
    let (quote_spot_market, _) = Pubkey::find_program_address(
        &[
            b"spot_market",
            quote_spot_market_index.to_le_bytes().as_ref(),
        ],
        &crate::ID,
    );

    // Named accounts through the executor's own client struct (compile-time
    // shape check), then the remaining sections: maps, maker
    // `(User, UserStats)` pairs, the quoter section. Both legs are the CLOB:
    // entry index 0.
    let mut metas = crate::accounts::CrankCrossMatch {
        state: ctx.accounts.state.key(),
        authority: Pubkey::new_from_array(KEEPER_PLACEHOLDER),
        taker: protocol_user,
        taker_stats: protocol_user_stats,
        crank_conditions: ctx.accounts.crank_conditions.key(),
    }
    .to_account_metas(None);
    use solana_program::instruction::AccountMeta;
    metas.push(AccountMeta::new_readonly(oracle, false));
    metas.push(AccountMeta::new(quote_spot_market, false));
    metas.push(AccountMeta::new(perp_market, false));
    for maker in &cross.makers {
        let (user_pda, stats_pda) = derive_user_pdas(maker);
        metas.push(AccountMeta::new(user_pda, false));
        metas.push(AccountMeta::new(stats_pda, false));
    }
    metas.push(AccountMeta::new_readonly(ctx.accounts.quoter.key(), false));
    metas.push(AccountMeta::new(ctx.accounts.clob_market.key(), false));
    metas.push(AccountMeta::new_readonly(signer, false));
    metas.push(AccountMeta::new_readonly(
        ctx.accounts.quoter.load()?.program_id,
        false,
    ));
    let accounts = to_account_refs(metas);

    let mut args = Vec::with_capacity(12);
    market_index.serialize(&mut args)?;
    cross.size.serialize(&mut args)?;
    0u8.serialize(&mut args)?; // buy leg: the CLOB entry
    0u8.serialize(&mut args)?; // sell leg: the CLOB entry
    let resolved = ResolvedCrankV0 {
        accounts,
        data: args,
    };
    let pointer = crate::load_mut!(ctx.accounts.crank_conditions)?.stage(&resolved)?;
    solana_program::program::set_return_data(&pointer);
    Ok(())
}

/// The crossing prefix of the book: total matchable size, the gross quote
/// of each leg, and the (deduped, capped) makers it touches.
struct ClobCross {
    size: u64,
    buy_quote: u128,
    sell_quote: u128,
    makers: Vec<crate::state::prop_amm::ClobUserRefV0>,
}

/// Two-pointer walk over the crossing prefix (bid price >= ask price),
/// best-first on both sides — exactly the orders the executor's two legs
/// will consume.
fn find_clob_cross(data: &[u8], slot: u64, now: i64) -> Result<ClobCross> {
    let base_precision = crate::math::constants::BASE_PRECISION_U64 as u128;
    let mut cross = ClobCross {
        size: 0,
        buy_quote: 0,
        sell_quote: 0,
        makers: Vec::new(),
    };
    let read = |offset: usize| {
        crate::state::prop_amm::read_clob_u32(data, offset)
            .ok_or_else(|| error!(ErrorCode::DefaultError))
    };
    let mut bid = next_matchable(
        data,
        read(crate::state::prop_amm::CLOB_BEST_BID_OFFSET)?,
        slot,
        now,
    );
    let mut ask = next_matchable(
        data,
        read(crate::state::prop_amm::CLOB_BEST_ASK_OFFSET)?,
        slot,
        now,
    );
    let mut bid_remaining = bid.map(|(_, node)| node.base_asset_amount).unwrap_or(0);
    let mut ask_remaining = ask.map(|(_, node)| node.base_asset_amount).unwrap_or(0);

    while let (Some((_, bid_node)), Some((_, ask_node))) = (bid, ask) {
        if bid_node.price < ask_node.price {
            break;
        }
        // Admit both makers before taking; stop at the cap instead of
        // taking size whose maker is not staged.
        let mut admit =
            |user: crate::state::prop_amm::ClobUserRefV0,
             makers: &mut Vec<crate::state::prop_amm::ClobUserRefV0>| {
                if makers.contains(&user) {
                    true
                } else if makers.len() < MAX_CROSS_MAKERS {
                    makers.push(user);
                    true
                } else {
                    false
                }
            };
        if !admit(bid_node.user_ref(), &mut cross.makers)
            || !admit(ask_node.user_ref(), &mut cross.makers)
        {
            break;
        }

        let take = bid_remaining.min(ask_remaining);
        cross.size = cross.size.saturating_add(take);
        cross.buy_quote = cross
            .buy_quote
            .saturating_add(ask_node.price as u128 * take as u128 / base_precision);
        cross.sell_quote = cross
            .sell_quote
            .saturating_add(bid_node.price as u128 * take as u128 / base_precision);

        bid_remaining -= take;
        ask_remaining -= take;
        if bid_remaining == 0 {
            bid = next_matchable(data, bid_node.next, slot, now);
            bid_remaining = bid.map(|(_, node)| node.base_asset_amount).unwrap_or(0);
        }
        if ask_remaining == 0 {
            ask = next_matchable(data, ask_node.next, slot, now);
            ask_remaining = ask.map(|(_, node)| node.base_asset_amount).unwrap_or(0);
        }
    }
    Ok(cross)
}
