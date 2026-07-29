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
                ClobCrankConditionsV0, CLOB_CRANK_EVICT, CLOB_CRANK_EXPIRE,
                CLOB_CRANK_EXPIRE_FALLBACK,
            },
            prop_amm::CLOB_BID_COUNT_OFFSET,
        },
        validate,
    },
    anchor_lang::prelude::*,
    relay_spec::{AccountRefV0, ConditionV0, CrankSpecV0},
    std::convert::TryInto,
};

/// The accounts every staged crank references, in resolver-account order.
pub struct ClobCrankConditionKeys {
    pub crank_conditions: Pubkey,
    pub clob_market: Pubkey,
    pub quoter: Pubkey,
    pub state: Pubkey,
}

fn disc8(disc: &[u8]) -> Result<[u8; 8]> {
    disc.try_into().map_err(|_| error!(ErrorCode::DefaultError))
}

/// (Re)write the full condition block. `initial_expire_wake_ts` preserves a
/// live hint across a re-price (`i64::MAX` on first write).
pub fn write_clob_crank_conditions(
    conditions: &mut ClobCrankConditionsV0,
    keys: &ClobCrankConditionKeys,
    market_index: u16,
    keeper_payment_lamports: u64,
    expire_fallback_slots: u64,
    initial_expire_wake_ts: i64,
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

    let resolver_accounts = [
        // Index 0 by contract: `ClobCrankConditionsV0::stage` points there.
        AccountRefV0::writable(keys.crank_conditions.to_bytes()),
        AccountRefV0::readonly(keys.clob_market.to_bytes()),
        AccountRefV0::readonly(keys.quoter.to_bytes()),
        AccountRefV0::readonly(keys.state.to_bytes()),
    ];
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
        crate::instruction::ResolveClobCrankEvict::DISCRIMINATOR,
        crate::instruction::CrankClobEvict::DISCRIMINATOR,
    )?;
    let expire_spec = spec(
        crate::instruction::ResolveClobCrankRemoveExpired::DISCRIMINATOR,
        crate::instruction::CrankClobRemoveExpired::DISCRIMINATOR,
    )?;

    conditions.market_index = market_index;
    conditions.keeper_payment_lamports = keeper_payment_lamports;
    conditions.init_header()?;
    conditions.write_condition(
        CLOB_CRANK_EVICT,
        // Both u32 counts, `bid_count` then `ask_count`, in one 8-byte watch.
        &ConditionV0::on_account_change(
            keys.clob_market.to_bytes(),
            CLOB_BID_COUNT_OFFSET as u32,
            8,
            evict_spec,
            &resolver_accounts,
        ),
    )?;
    conditions.write_condition(
        CLOB_CRANK_EXPIRE,
        &ConditionV0::at_timestamp(initial_expire_wake_ts, expire_spec, &resolver_accounts),
    )?;
    conditions.write_condition(
        CLOB_CRANK_EXPIRE_FALLBACK,
        &ConditionV0::every_slots(expire_fallback_slots, expire_spec, &resolver_accounts),
    )?;
    Ok(())
}
