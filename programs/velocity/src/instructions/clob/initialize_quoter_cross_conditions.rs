//! Stand up (or re-price) a Custom quoter's relay cross-discovery
//! conditions. Permissionless — every input is validated against the
//! registry and the market, and the only thing the caller "gains" is
//! paying the rent: attach requires the entry active + approved, the
//! market's canonical CLOB attached, and the conditions PDA derives from
//! the entry key. Re-running re-prices (a new keeper payment, a changed
//! watch declaration, a rotated CLOB) — the same idempotent shape as the
//! market attach.

use {
    crate::{
        error::ErrorCode,
        state::{
            clob_crank::{ClobCrankConditionsV0, CLOB_CRANK_CONDITIONS_PDA_SEED},
            perp_market::PerpMarket,
            prop_amm::{QuoterType, QuoterV0},
            quoter_cross::{
                QuoterCrossConditionsV0, QUOTER_CROSS_CLOB, QUOTER_CROSS_CONDITIONS_PDA_SEED,
                QUOTER_CROSS_FALLBACK, QUOTER_CROSS_WATCH,
            },
            state::State,
        },
        validate,
    },
    anchor_lang::{prelude::*, Discriminator},
    relay_spec::{AccountRefV0, ConditionV0, CrankSpecV0},
    std::convert::TryInto,
};

#[derive(Accounts)]
pub struct InitializeQuoterCrossConditions<'info> {
    #[account(mut)]
    pub payer: Signer<'info>,
    pub state: AccountLoader<'info, State>,
    /// The Custom entry to discover crosses for.
    pub quoter: AccountLoader<'info, QuoterV0>,
    #[account(
        seeds = [b"perp_market", quoter.load()?.market.to_le_bytes().as_ref()],
        bump
    )]
    pub perp_market: AccountLoader<'info, PerpMarket>,
    /// The market's canonical CLOB entry — the other leg of every staged
    /// cross.
    #[account(address = perp_market.load()?.clob_quoter)]
    pub clob_quoter: AccountLoader<'info, QuoterV0>,
    /// The market's crank conditions: the keeper-payment source of truth.
    #[account(
        seeds = [
            CLOB_CRANK_CONDITIONS_PDA_SEED,
            quoter.load()?.market.to_le_bytes().as_ref(),
        ],
        bump
    )]
    pub market_conditions: AccountLoader<'info, ClobCrankConditionsV0>,
    #[account(
        init_if_needed,
        seeds = [QUOTER_CROSS_CONDITIONS_PDA_SEED, quoter.key().as_ref()],
        space = QuoterCrossConditionsV0::SIZE,
        bump,
        payer = payer
    )]
    pub cross_conditions: AccountLoader<'info, QuoterCrossConditionsV0>,
    pub rent: Sysvar<'info, Rent>,
    pub system_program: Program<'info, System>,
}

pub fn handle_initialize_quoter_cross_conditions(
    ctx: Context<InitializeQuoterCrossConditions>,
    expire_fallback_slots: u64,
) -> Result<()> {
    validate!(
        expire_fallback_slots > 0,
        ErrorCode::DefaultError,
        "fallback interval must be nonzero"
    )?;
    let quoter = ctx.accounts.quoter.load()?;
    validate!(
        quoter.quoter_type == QuoterType::Custom,
        ErrorCode::InvalidQuoterConfig,
        "cross conditions are for Custom quoters (the CLOB's are on the market conditions)"
    )?;
    validate!(
        quoter.is_active && quoter.is_approved,
        ErrorCode::InvalidQuoterConfig,
        "quoter must be active and approved to attach cross discovery"
    )?;
    let clob = ctx.accounts.clob_quoter.load()?;
    validate!(
        clob.quoter_type == QuoterType::Clob && clob.market == quoter.market,
        ErrorCode::InvalidQuoterConfig,
        "market's canonical CLOB entry required"
    )?;

    let keeper_payment_lamports = {
        let market_conditions = ctx.accounts.market_conditions.load()?;
        market_conditions.keeper_payment_lamports
    };
    let (oracle, quote_spot_market_index) = {
        let market = ctx.accounts.perp_market.load()?;
        (market.oracle, market.quote_spot_market_index)
    };
    let clob_market = clob.response_account;

    // The resolver's account list: the conditions (index 0 — where the
    // response pointer says the payload lives), the CLOB book, the state,
    // the entry, its quoted user, then the entry's full registered quote
    // surface and program — everything the generic quote CPI needs.
    let mut resolver_accounts = vec![
        AccountRefV0::writable(ctx.accounts.cross_conditions.key().to_bytes()),
        AccountRefV0::readonly(clob_market.to_bytes()),
        AccountRefV0::readonly(ctx.accounts.state.key().to_bytes()),
        AccountRefV0::readonly(ctx.accounts.quoter.key().to_bytes()),
        AccountRefV0::readonly(quoter.user.to_bytes()),
    ];
    for meta in &quoter.quote_accounts[..quoter.quote_accounts_count as usize] {
        resolver_accounts.push(if meta.is_writable {
            AccountRefV0::writable(meta.pubkey.to_bytes())
        } else {
            AccountRefV0::readonly(meta.pubkey.to_bytes())
        });
    }
    resolver_accounts.push(AccountRefV0::readonly(quoter.program_id.to_bytes()));
    validate!(
        resolver_accounts.len() <= relay_spec::MAX_RESOLVER_ACCOUNTS,
        ErrorCode::InvalidQuoterConfig,
        "quoter's registered quote surface exceeds the resolver account cap"
    )?;

    let disc8 = |disc: &[u8]| -> Result<[u8; 8]> {
        disc.try_into().map_err(|_| error!(ErrorCode::DefaultError))
    };
    let spec = CrankSpecV0 {
        resolver_program: crate::ID.to_bytes(),
        resolver_disc: disc8(crate::instruction::ResolveCrankCrossMatchQuoter::DISCRIMINATOR)?,
        executor_program: crate::ID.to_bytes(),
        executor_disc: disc8(crate::instruction::CrankCrossMatch::DISCRIMINATOR)?,
        min_payment: keeper_payment_lamports,
    };

    let mut conditions = ctx.accounts.cross_conditions.load_init().or_else(|_| {
        // Re-attach: the account already exists; re-price in place.
        ctx.accounts.cross_conditions.load_mut()
    })?;
    conditions.quoter = ctx.accounts.quoter.key();
    conditions.clob_quoter = ctx.accounts.clob_quoter.key();
    conditions.clob_market = clob_market;
    conditions.clob_program = clob.program_id;
    conditions.oracle = oracle;
    conditions.market_index = quoter.market;
    conditions.quote_spot_market_index = quote_spot_market_index;
    conditions.init_header()?;

    // The maker-declared reprice watch; inactive when nothing is declared
    // (the fallback poll is then the only wake for this side).
    if quoter.watch_len > 0 {
        conditions.write_condition(
            QUOTER_CROSS_WATCH,
            &ConditionV0::on_account_change(
                quoter.watch_account.to_bytes(),
                quoter.watch_offset,
                quoter.watch_len,
                spec,
                &resolver_accounts,
            ),
        )?;
    } else {
        conditions.write_condition(
            QUOTER_CROSS_WATCH,
            &relay_spec::bytemuck::Zeroable::zeroed(),
        )?;
    }
    conditions.write_condition(
        QUOTER_CROSS_CLOB,
        // Both u32 side heads, `best_bid` then `best_ask`, in one 8-byte
        // watch — a crossing order is always a new best.
        &ConditionV0::on_account_change(
            clob_market.to_bytes(),
            crate::state::prop_amm::CLOB_BEST_BID_OFFSET as u32,
            8,
            spec,
            &resolver_accounts,
        ),
    )?;
    conditions.write_condition(
        QUOTER_CROSS_FALLBACK,
        &ConditionV0::every_slots(expire_fallback_slots, spec, &resolver_accounts),
    )?;
    Ok(())
}
