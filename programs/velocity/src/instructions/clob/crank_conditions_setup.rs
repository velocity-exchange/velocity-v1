//! Writes a market's [`ClobCrankConditionsV0`] block — the relay conditions
//! turners read to discover the market's CLOB crank work. Not an instruction:
//! standing up the cranks is part of attaching a CLOB to a market
//! (`update_perp_market_clob_quoter` creates the account `init_if_needed` and
//! calls this), so a new market needs no separate conditions ceremony and
//! re-attaching re-prices the crank.
//!
//! The three conditions share one resolver account list, in the fixed order
//! the resolvers' `#[derive(Accounts)]` expects: the conditions account
//! (writable — the staging region lives on it, at index 0 as
//! [`ClobCrankConditionsV0::stage`] encodes), the CLOB market, the quoter
//! registry entry, and the state.

use {
    crate::{
        error::ErrorCode,
        state::{
            clob_crank::{
                ClobCrankConditionsV0, CLOB_CRANK_CROSS, CLOB_CRANK_CROSS_ACTIVATION,
                CLOB_CRANK_CROSS_FALLBACK, CLOB_CRANK_EVICT, CLOB_CRANK_EXPIRE,
                CLOB_CRANK_EXPIRE_FALLBACK,
            },
            prop_amm::{CLOB_BEST_BID_OFFSET, CLOB_BID_COUNT_OFFSET},
        },
        validate,
    },
    anchor_lang::prelude::*,
    relay_spec::{AccountRefV0, ConditionV0, CrankSpecV0},
    std::convert::TryInto,
};

/// The accounts every staged crank references, in resolver-account order,
/// plus the market references resolvers derive staged executors from.
pub struct ClobCrankConditionKeys {
    pub crank_conditions: Pubkey,
    pub clob_market: Pubkey,
    pub quoter: Pubkey,
    pub state: Pubkey,
    /// The perp market's oracle, stored on the conditions account so the
    /// cross resolver can stage the executor's map section without holding
    /// the perp market account.
    pub oracle: Pubkey,
    pub quote_spot_market_index: u16,
}

fn disc8(disc: &[u8]) -> Result<[u8; 8]> {
    disc.try_into().map_err(|_| error!(ErrorCode::DefaultError))
}

/// (Re)write the full condition block: evict, expire (+ fallback), and
/// cross (+ fallback). `initial_expire_wake_ts` preserves a live hint across
/// a re-price (`i64::MAX` on first write).
pub fn write_clob_crank_conditions(
    conditions: &mut ClobCrankConditionsV0,
    keys: &ClobCrankConditionKeys,
    market_index: u16,
    keeper_payment_lamports: u64,
    expire_fallback_slots: u64,
    initial_expire_wake_ts: i64,
    initial_activation_wake_slot: u64,
) -> Result<()> {
    validate!(
        keeper_payment_lamports > 0,
        ErrorCode::DefaultError,
        "keeper payment must be nonzero: turners have no signal to take unpaid work"
    )?;
    validate!(
        expire_fallback_slots > 0,
        ErrorCode::DefaultError,
        "expire fallback interval must be nonzero"
    )?;

    // Stored once on the account; every condition below points at it.
    // Index 0 by contract: `ClobCrankConditionsV0::stage` points there.
    let resolvers = conditions.write_resolvers(&[
        AccountRefV0::writable(crate::state::pdas::relay_scratch().to_bytes()),
        AccountRefV0::readonly(keys.crank_conditions.to_bytes()),
        AccountRefV0::readonly(keys.clob_market.to_bytes()),
        AccountRefV0::readonly(keys.quoter.to_bytes()),
        AccountRefV0::readonly(keys.state.to_bytes()),
    ])?;
    let spec = |resolver_disc: &[u8], executor_disc: &[u8]| -> Result<CrankSpecV0> {
        Ok(CrankSpecV0 {
            resolver_program: crate::ID.to_bytes(),
            resolver_disc: disc8(resolver_disc)?,
            executor_program: crate::ID.to_bytes(),
            executor_disc: disc8(executor_disc)?,
            min_payment: keeper_payment_lamports,
        })
    };
    let evict_spec = spec(
        crate::instruction::ResolveCrankClobEvict::DISCRIMINATOR,
        crate::instruction::CrankClobEvict::DISCRIMINATOR,
    )?;
    let expire_spec = spec(
        crate::instruction::ResolveCrankClobRemoveExpired::DISCRIMINATOR,
        crate::instruction::CrankClobRemoveExpired::DISCRIMINATOR,
    )?;

    conditions.market_index = market_index;
    conditions.keeper_payment_lamports = keeper_payment_lamports;
    conditions.oracle = keys.oracle;
    conditions.quote_spot_market_index = keys.quote_spot_market_index;
    conditions.init_block()?;
    conditions.set_condition(
        CLOB_CRANK_EVICT,
        // Both u32 counts, `bid_count` then `ask_count`, in one 8-byte watch.
        &ConditionV0::on_account_change(
            keys.clob_market.to_bytes(),
            CLOB_BID_COUNT_OFFSET as u32,
            8,
            evict_spec,
            resolvers,
        ),
    )?;
    conditions.set_condition(
        CLOB_CRANK_EXPIRE,
        &ConditionV0::at_timestamp(initial_expire_wake_ts, expire_spec, resolvers),
    )?;
    conditions.set_condition(
        CLOB_CRANK_EXPIRE_FALLBACK,
        &ConditionV0::every_slots(expire_fallback_slots, expire_spec, resolvers),
    )?;
    let cross_spec = spec(
        crate::instruction::ResolveCrankCrossMatch::DISCRIMINATOR,
        crate::instruction::CrankCrossMatch::DISCRIMINATOR,
    )?;
    conditions.set_condition(
        CLOB_CRANK_CROSS,
        // Both u32 side heads, `best_bid` then `best_ask`, in one 8-byte
        // watch — a crossing order is always a new best.
        &ConditionV0::on_account_change(
            keys.clob_market.to_bytes(),
            CLOB_BEST_BID_OFFSET as u32,
            8,
            cross_spec,
            resolvers,
        ),
    )?;
    conditions.set_condition(
        CLOB_CRANK_CROSS_FALLBACK,
        // A PropAMM crossing the CLOB has no single account to watch — the
        // poll is that case's liveness floor (the book publisher is the
        // fast path).
        &ConditionV0::every_slots(expire_fallback_slots, cross_spec, resolvers),
    )?;
    let activation = ConditionV0::at_slot(initial_activation_wake_slot, cross_spec, resolvers);
    conditions.set_condition(
        CLOB_CRANK_CROSS_ACTIVATION,
        // Activation-slot maturation makes an order matchable with no
        // account change — but it is exactly when makers who lined up
        // against a speed-bumped order expect the cross, so the program
        // names the slot precisely: min-folded at placement, repaired by
        // every landing crank's scan.
        &activation,
    )?;
    Ok(())
}
