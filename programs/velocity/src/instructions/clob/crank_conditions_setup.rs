//! Stands up a market's CLOB cranks: the one condition velocity hosts, and
//! the registration that tells the book which resolver answers each of its
//! own. Not an instruction: standing up the cranks is part of attaching a
//! CLOB to a market (`update_perp_market_clob_quoter` creates the conditions
//! account `init_if_needed` and calls this), so a new market needs no separate
//! ceremony and re-attaching re-prices the crank.
//!
//! # Where each condition lives
//!
//! A condition is a wake and an answer, and the two have different owners.
//! The wake is a fact about an account, so it belongs to the program that
//! writes that account: an expiry, an activation, a side at its eviction
//! threshold and a crossed book are the book's, and the book keeps their
//! wakes current in the same instruction that changes what they describe. The
//! answer is what to do about it, and every one of these removes an order,
//! which releases a maker's margin reservation, pays a reward and frees a
//! trigger slot — none of which the book holds.
//!
//! So [`register_clob_crank_conditions`] hands the book velocity's resolvers
//! and velocity's account list, and [`write_clob_crank_conditions`] keeps the
//! one wake that is not about the book: the poll that catches a cross a
//! PropAMM created by repricing.
//!
//! # The resolver account list
//!
//! One list serves every condition, in the fixed order the resolvers'
//! `#[derive(Accounts)]` expects: the shared scratch account (writable — the
//! staging region lives on it, at index 0 as [`ClobCrankConditionsV0::stage`]
//! encodes), the CLOB market, the conditions account, the quoter registry
//! entry, the state, and the CLOB program.
//!
//! The program is there because a resolver asks the book what work it has
//! rather than reading the answer out of the book's bytes, and the book is
//! writable because one of those questions is `quote_l3_v0`, which streams its
//! answer into the market account's own response tail. Both are
//! simulation-only calls: nothing a resolver sends ever lands.

use {
    crate::{
        error::ErrorCode,
        state::{
            clob_crank::{
                ClobCrankConditionsV0, CrankPaymentsV0, CLOB_CRANK_CROSS_FALLBACK,
                CLOB_CRANK_REFILL,
            },
            prop_amm::{ClobCrankAccountV0, ClobCrankConditionsArgsV0, ClobCrankResolverV0},
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
    /// The book's own program. A resolver asks the book what work it has
    /// rather than reading its arena, so it needs the program to call.
    pub clob_program: Pubkey,
    pub quoter: Pubkey,
    pub state: Pubkey,
    /// The perp market's oracle, stored on the conditions account so the
    /// cross resolver can stage the executor's map section without holding
    /// the perp market account.
    pub oracle: Pubkey,
    pub quote_spot_market_index: u16,
}

impl ClobCrankConditionKeys {
    /// The resolver account list, in the order [`ResolveClobCrank`] declares
    /// it. The book stores the same list, so both sides of the split call
    /// velocity's resolvers with identical accounts.
    ///
    /// [`ResolveClobCrank`]: super::helpers::crank_common::ResolveClobCrank
    fn resolver_accounts(&self) -> [AccountRefV0; 7] {
        [
            AccountRefV0::writable(crate::state::pdas::relay_scratch().to_bytes()),
            AccountRefV0::readonly(self.crank_conditions.to_bytes()),
            AccountRefV0::writable(self.clob_market.to_bytes()),
            AccountRefV0::readonly(self.quoter.to_bytes()),
            AccountRefV0::readonly(self.state.to_bytes()),
            AccountRefV0::readonly(self.clob_program.to_bytes()),
            AccountRefV0::readonly(crate::state::pdas::crank_treasury().to_bytes()),
        ]
    }
}

fn disc8(disc: &[u8]) -> Result<[u8; 8]> {
    disc.try_into().map_err(|_| error!(ErrorCode::DefaultError))
}

/// What the book's four conditions wake into, and what each pays.
///
/// One resolver for all four: relay hands it the condition that fired, so it
/// asks the book only about the work that condition describes. `min_payment`
/// stays per-condition, because that is where relay reads it — a removal is
/// held to a removal's payment, a cross to the cheaper of the two crosses its
/// answer can stage. The dearer one pays more than the floor, which passes,
/// while a floor set to the dearer one would fail the cheaper.
pub fn clob_crank_registration(
    keys: &ClobCrankConditionKeys,
    payments: CrankPaymentsV0,
) -> Result<ClobCrankConditionsArgsV0> {
    let resolver = |min_payment: u32| -> Result<ClobCrankResolverV0> {
        Ok(ClobCrankResolverV0 {
            program: crate::ID.to_bytes(),
            disc: disc8(crate::instruction::ResolveClobCrank::DISCRIMINATOR)?,
            min_payment: u64::from(min_payment),
        })
    };
    let cross = resolver(payments.cross.min(payments.taker_origin_cross))?;
    Ok(ClobCrankConditionsArgsV0 {
        expiry: resolver(payments.removal)?,
        // Activation makes an order matchable with no account change, so the
        // book names the slot and the cross answer resolves it.
        activation: cross,
        capacity: resolver(payments.removal)?,
        cross,
        accounts: keys
            .resolver_accounts()
            .iter()
            .map(|account| ClobCrankAccountV0 {
                address: account.address,
                writable: account.writable,
            })
            .collect(),
    })
}

/// (Re)write velocity's own block: the cross fallback poll, and the market
/// references every staged executor is built from.
pub fn write_clob_crank_conditions(
    conditions: &mut ClobCrankConditionsV0,
    keys: &ClobCrankConditionKeys,
    market_index: u16,
    payments: CrankPaymentsV0,
    min_cross_surplus: u64,
    cross_fallback_slots: u64,
    refill_watermark_lamports: u64,
) -> Result<()> {
    validate!(
        payments.all_priced(),
        ErrorCode::DefaultError,
        "every crank must be priced: turners have no signal to take unpaid work"
    )?;
    validate!(
        cross_fallback_slots > 0,
        ErrorCode::DefaultError,
        "cross fallback interval must be nonzero"
    )?;

    // The block stamps its own account offset before anything points into
    // it. Stored once on the account; the condition below points at it.
    // Index 0 by contract: the staged response pointer names the scratch.
    conditions.init_block()?;
    let resolvers = conditions.write_resolvers(&keys.resolver_accounts())?;

    conditions.market_index = market_index;
    conditions.refill_watermark_lamports = refill_watermark_lamports;
    conditions.crank_payments = payments;
    conditions.min_cross_surplus = min_cross_surplus;
    conditions.oracle = keys.oracle;
    conditions.quote_spot_market_index = keys.quote_spot_market_index;
    conditions.set_condition(
        CLOB_CRANK_CROSS_FALLBACK,
        // A PropAMM crossing the CLOB writes to neither account the book's
        // own cross watch covers, so the poll is that case's liveness floor
        // (the book publisher is the fast path).
        &ConditionV0::every_slots(
            cross_fallback_slots,
            CrankSpecV0 {
                resolver_program: crate::ID.to_bytes(),
                resolver_disc: disc8(crate::instruction::ResolveClobCrank::DISCRIMINATOR)?,
                min_payment: u64::from(payments.cross.min(payments.taker_origin_cross)),
            },
            resolvers,
        ),
    )?;
    // The reservoir's own liveness. A lamport balance is account metadata and
    // a watch reads account data, so the account mirrors its spendable balance
    // into `spendable_mirror` and this wakes on that value falling to the
    // watermark. Watched account and block account are the same one, so the
    // watch that finds this block already covers the value.
    conditions.set_condition(
        CLOB_CRANK_REFILL,
        &ConditionV0::on_value_cross(
            keys.crank_conditions.to_bytes(),
            <u32 as core::convert::TryFrom<usize>>::try_from(
                crate::state::clob_crank::CLOB_CRANK_SPENDABLE_MIRROR_OFFSET,
            )
            .map_err(|_| error!(ErrorCode::DefaultError))?,
            8,
            relay_spec::WatchValue::Unsigned(refill_watermark_lamports),
            // Due when the mirrored balance is at or below the watermark.
            1,
            CrankSpecV0 {
                resolver_program: crate::ID.to_bytes(),
                resolver_disc: disc8(crate::instruction::ResolveClobCrank::DISCRIMINATOR)?,
                // A turner drops any condition advertising less than its own
                // configured floor, so this states what the refill really
                // pays. Priced from the same rails as every other crank and
                // stored on this account, so re-pricing the network re-prices
                // it on the next attach — the treasury holds the lamports, not
                // the price.
                min_payment: u64::from(payments.refill),
            },
            resolvers,
        ),
    )?;
    // A new account's mirror is zero, which reads as below the watermark, so
    // the refill fires as soon as the market is attached. That is the intent:
    // a market funds its own reservoir from the treasury and nobody seeds it
    // by hand.
    Ok(())
}
