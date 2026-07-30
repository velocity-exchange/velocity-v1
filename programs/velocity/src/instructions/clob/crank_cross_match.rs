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
            user_map::{load_user_map, load_user_maps, UserStatsMap},
        },
        validate,
    },
    anchor_lang::{prelude::*, Discriminator},
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
    makers_include_stats: bool,
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
    // Direct callers pass (User, UserStats) pairs for full maker attribution;
    // relay-staged calls pass Users only, because a resolver cannot derive
    // stats PDAs (see `cross_match`'s parameter note).
    let (makers_and_referrer, makers_and_referrer_stats) = if makers_include_stats {
        let (users, stats) = load_user_maps(remaining_accounts_iter, true)?;
        (users, Some(stats))
    } else {
        (load_user_map(remaining_accounts_iter, true)?, None)
    };

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

    let taker_key = ctx.accounts.taker.key();
    let users: Vec<Pubkey> = makers_and_referrer.0.keys().copied().collect();
    let mut executor = CpiQuoterExecutor {
        quoters: &quoters,
        types,
        quoter_users,
        account_map: &account_map,
        velocity_signer: state.signer,
        signer_nonce: state.signer_nonce,
        users,
        taker: taker_key,
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
        makers_and_referrer_stats.as_ref(),
        &mut executor,
        &perp_market_map,
        &spot_market_map,
        &mut oracle_map,
        &clock,
    )?;

    // The keeper's fee, so relay's assert_paid_v0 has a balance to measure.
    let payment = {
        let conditions = load!(ctx.accounts.crank_conditions)?;
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
