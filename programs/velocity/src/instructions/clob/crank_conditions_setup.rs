//! Creates a market's CLOB cranks. That is the one condition velocity hosts,
//! plus the registration that tells the book which resolver answers each of
//! its own conditions. This is not an instruction. Creating the cranks is part
//! of attaching a CLOB to a market. `update_perp_market_clob_quoter` creates
//! the conditions account with `init_if_needed` and calls this, so a new market
//! needs no separate ceremony, and a re-attach re-prices the cranks.
//!
//! # Where each condition lives
//!
//! A condition is a wake and an answer, and the two have different owners. The
//! wake is a fact about an account, so it belongs to the program that writes
//! that account. An expiry, an activation, a side at its eviction threshold and
//! a crossed book are all the book's facts, and the book keeps their wakes
//! current in the same instruction that changes what they describe. The answer
//! is what to do about the wake. Every one of these answers removes an order,
//! which releases a maker's margin reservation, pays a reward and frees a
//! trigger slot. The book holds none of that state.
//!
//! So [`clob_crank_registration`] gives the book velocity's resolvers and
//! velocity's account list. [`ClobCrankConditionsV0::write_crank_conditions`]
//! keeps the one wake that is not about the book. That wake is the poll which
//! catches a cross a PropAMM created by repricing.
//!
//! # The resolver account list
//!
//! One list serves every condition, in the fixed order the resolvers'
//! `#[derive(Accounts)]` expects: the shared scratch account, the conditions
//! account, the CLOB market, the quoter slab, the state, the CLOB program, and
//! the crank treasury. The scratch account is writable and sits at index 0,
//! which is the index [`crate::state::relay_scratch::RelayScratchV0::stage`]
//! encodes into the response pointer.
//!
//! The program is in the list because a resolver asks the book what work it has
//! rather than reading the answer out of the book's bytes. The book is writable
//! because one of those questions is `quote_l3_v0`, which streams its answer
//! into the market account's own response tail. Both are simulation-only calls.
//! Nothing a resolver sends ever lands.

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
    /// The market's quoter slab, which holds the book's approved config.
    pub quoter_slab: Pubkey,
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
            AccountRefV0::readonly(self.quoter_slab.to_bytes()),
            AccountRefV0::readonly(self.state.to_bytes()),
            AccountRefV0::readonly(self.clob_program.to_bytes()),
            AccountRefV0::readonly(crate::state::pdas::crank_treasury().to_bytes()),
        ]
    }
}

fn disc8(disc: &[u8]) -> Result<[u8; 8]> {
    disc.try_into()
        .map_err(|_| error!(ErrorCode::CastingFailure))
}

/// What the book's four conditions wake into, and what each one pays.
///
/// One resolver serves all four. Relay passes it the condition that fired, so
/// it asks the book only about the work that condition describes. `min_payment`
/// stays per-condition, because that is where relay reads it. A removal is held
/// to a removal's payment. A cross is held to the cheaper of the two crosses
/// its answer can stage. The dearer cross pays more than that floor and passes,
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
        // Activation makes an order matchable with no account change. The book
        // names the slot, and the cross answer resolves it.
        activation: cross,
        capacity: resolver(payments.removal)?,
        cross,
        accounts: keys
            .resolver_accounts()
            .iter()
            .map(|account| ClobCrankAccountV0 {
                address: account.address,
                writable: account.writable != 0,
            })
            .collect(),
    })
}

impl ClobCrankConditionsV0 {
    /// Write velocity's own block. That is the cross fallback poll, plus the
    /// market references every staged executor is built from. A re-attach
    /// writes it again.
    ///
    /// It is declared here rather than in `state::clob_crank` because
    /// everything it writes belongs to the attach. That is the resolver keys,
    /// the payments and the condition specs. The state module only holds the
    /// layout.
    pub fn write_crank_conditions(
        &mut self,
        keys: &ClobCrankConditionKeys,
        market_index: u16,
        payments: CrankPaymentsV0,
        min_cross_surplus: u64,
        cross_fallback_slots: u64,
        refill_watermark_lamports: u64,
    ) -> Result<()> {
        validate!(
            payments.all_priced(),
            ErrorCode::InvalidQuoterConfig,
            "every crank must be priced: turners have no signal to take unpaid work"
        )?;
        validate!(
            cross_fallback_slots > 0,
            ErrorCode::InvalidQuoterConfig,
            "cross fallback interval must be nonzero"
        )?;

        // The block stamps its own account offset before anything points into
        // it. The offset is stored once on the account, and the condition below
        // points at it.
        self.init_block()?;
        let resolvers = self.write_resolvers(&keys.resolver_accounts())?;

        self.market_index = market_index;
        self.refill_watermark_lamports = refill_watermark_lamports;
        self.crank_payments = payments;
        self.min_cross_surplus = min_cross_surplus;
        self.oracle = keys.oracle;
        self.quote_spot_market_index = keys.quote_spot_market_index;
        self.set_condition(
            CLOB_CRANK_CROSS_FALLBACK,
            // A PropAMM that crosses the CLOB writes to neither account the
            // book's own cross watch covers. The poll is the liveness floor for
            // that case. The book publisher is the fast path.
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

        // The reservoir's own liveness. A lamport balance is account metadata,
        // and a watch reads account data. So the account mirrors its spendable
        // balance into `spendable_mirror`, and this condition wakes when that
        // value falls to the watermark. The watched account and the block
        // account are the same account, so the watch that finds this block
        // already covers the value.
        self.set_condition(
            CLOB_CRANK_REFILL,
            &ConditionV0::on_value_cross(
                relay_spec::WatchedRegion::new(
                    keys.crank_conditions.to_bytes(),
                    <u32 as core::convert::TryFrom<usize>>::try_from(
                        crate::state::clob_crank::CLOB_CRANK_SPENDABLE_MIRROR_OFFSET,
                    )
                    .map_err(|_| error!(ErrorCode::CastingFailure))?,
                    8,
                ),
                relay_spec::WatchValue::Unsigned(refill_watermark_lamports),
                // Due when the mirrored balance is at or below the watermark.
                1,
                CrankSpecV0 {
                    resolver_program: crate::ID.to_bytes(),
                    resolver_disc: disc8(crate::instruction::ResolveClobCrank::DISCRIMINATOR)?,
                    // A turner drops conditions below its configured floor. This
                    // is the refill's price, taken from the same rails as every
                    // other crank, so re-pricing it re-prices the refill on the
                    // next attach. The treasury holds the lamports, not this field.
                    min_payment: u64::from(payments.refill),
                },
                resolvers,
            ),
        )?;

        // A new account's mirror is zero, which reads as below the watermark.
        // The refill therefore fires as soon as the market is attached. A market
        // funds its own reservoir from the treasury, and no operator seeds it by
        // hand.
        Ok(())
    }
}
