//! Relay condition block for a perp market's CLOB cranks.
//!
//! Relay turners find work by reading a condition block. A condition block is
//! a `relay-spec` wire structure. Each condition names when to wake, which
//! instruction to simulate to find work (the resolver), and which instruction
//! does the work (the executor).
//!
//! A condition splits into two halves with different owners. The wake is a
//! fact about an account, so it belongs to the program that writes that
//! account. The resolver is what to do about the wake. Removal of an order
//! adjusts the maker's `User`, both the open-order aggregates and the reward
//! debit, and only velocity can do that. So the book hosts the wakes for its
//! own state, velocity registers the resolvers that answer them, and this
//! account holds the wakes that describe this account.
//!
//! There are two such wakes per market. [`CLOB_CRANK_CROSS_FALLBACK`] is a
//! periodic poll. It catches a cross that a PropAMM created by repricing,
//! which changes nothing on the book and so fires no watch.
//! [`CLOB_CRANK_REFILL`] wakes when this account's reservoir drains to its
//! watermark.
//!
//! The book's own work wakes off conditions that the CLOB hosts on the market
//! account. That work is an expired order, a side at its eviction threshold, a
//! crossed book, or an order that reaches its activation slot.
//! `set_crank_conditions_v0` registers those conditions at attach. They name
//! velocity's resolvers and pay out of this account's reservoir. There is no
//! expiry fallback poll, because a poll covers a hint whose maintenance is
//! best-effort, and the book maintains that hint in the same instruction that
//! changes what the hint describes.
//!
//! Resolvers stage their `ResolvedCrankV0`, the executor account list and its
//! arguments, into the program-wide
//! [`crate::state::relay_scratch::RelayScratchV0`] rather than into this
//! account. A resolver only runs under simulation, so the staged bytes never
//! land on chain and two turners cannot collide. See that module for why the
//! region is shared rather than one region per conditions account.
//!
//! `relay_spec::read_block` and `read_block_mut` reach the block as an opaque
//! byte region instead of typed fields. That keeps `relay-spec`'s pod types
//! out of velocity's zero-copy layout. The region size is the only thing this
//! account commits to. A spec revision that adds a field is then a version
//! bump here, not a layout migration.
//!
//! `relay` is the first field, so it starts at offset 8, past anchor's
//! discriminator. `read_block` requires that 8-aligned offset.

use {
    crate::{
        error::{ErrorCode, VelocityResult},
        math::safe_math::SafeMath,
    },
    anchor_lang::prelude::*,
    relay_anchor::RelayBlock,
    relay_spec::{ConditionBlock, RelayBlockV0},
    std::convert::TryFrom,
};

/// PDA seed: `["clob_crank_conditions", market_index]`.
pub const CLOB_CRANK_CONDITIONS_PDA_SEED: &[u8] = b"clob_crank_conditions";

/// Index of the cross fallback: `WakeKind::EverySlots`. A PropAMM that reprices into a
/// cross with the book changes nothing on the book's account, so no watch on the book
/// fires. This poll is the liveness floor for that case.
pub const CLOB_CRANK_CROSS_FALLBACK: usize = 0;
/// Index of the reservoir refill: `WakeKind::OnValueCross`. Wakes when
/// [`ClobCrankConditionsV0::spendable_mirror`] falls to the treasury's watermark. The
/// watched value sits on this account, so the watch that finds this block covers it.
pub const CLOB_CRANK_REFILL: usize = 1;
/// Conditions hosted per market.
pub const CLOB_CRANK_CONDITIONS: usize = 2;

/// Every condition on this account resolves with the same seven accounts. The
/// capacity is 8, which is [`RelayBlockV0`]'s smallest granularity.
pub const CLOB_CRANK_RESOLVER_CAPACITY: usize = 8;

/// Account-data offset of the relay block (what a `WatchV0` registers at).
pub const CLOB_CRANK_BLOCK_OFFSET: usize = relay_spec::block_offset!(ClobCrankConditionsV0, relay);

/// Account-data offset of the mirrored spendable balance (what the refill
/// condition watches).
pub const CLOB_CRANK_SPENDABLE_MIRROR_OFFSET: usize =
    relay_spec::block_offset!(ClobCrankConditionsV0, spendable_mirror);

/// Seconds past an order's expiry at which its crank pays the full
/// escalation. See [`CrankPaymentsV0::expiry_escalation`].
pub const EXPIRY_ESCALATION_SECONDS: u64 = 300;
/// The most an expiry crank adds to its base payment, in lamports.
pub const EXPIRY_ESCALATION_CEILING: u32 = 5_000;

/// Compute units a liquidation crank's priority fee is reimbursed against. A measured
/// figure, not the limit a transaction asks for, because reimbursing the request would
/// let a caller inflate its own bill.
pub const LIQUIDATION_CRANK_REIMBURSED_UNITS: u32 = 400_000;

/// Least filled quote value a liquidation crank must recover to earn its flat reservoir
/// payment, in `QUOTE_PRECISION`. Ten dollars. Without a floor a keeper stages one
/// liquidation as many small fills and collects the flat payment on each. A crank below
/// the floor still liquidates, so dust is cranked without paying to farm it.
pub const LIQUIDATION_FLAT_PAYMENT_MIN_FILLED_QUOTE: u64 = 10_000_000;

/// Cost units each of a market's cranks requests, one field per crank. Measured figures:
/// a turner simulates the crank, and the rest of the sum comes from the transaction it
/// assembles. The unit is the block-packing cost unit, which
/// `State.transaction_fee_rails` turns into lamports.
#[derive(AnchorSerialize, AnchorDeserialize, Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CrankCostUnitsV0 {
    /// `evict_worst` / `remove_expired`: one book write and a hint repair. The one crank
    /// pair whose payment nothing compares against what the protocol collects. A rails
    /// re-pricing that lifts `removal` past the `flat_filler_fee`'s value in SOL turns
    /// each reservoir into a faucet.
    pub removal: u32,
    /// `crank_cross_match`: two settlement legs through the router.
    pub cross: u32,
    /// `crank_taker_origin_cross`: the same, against a taker remainder.
    pub taker_origin_cross: u32,
    /// `trigger_order` / `trigger_limit_order_v1` in program-keeper mode.
    pub trigger: u32,
    /// `liquidate_perp_with_fill` in program-keeper mode.
    pub liquidation: u32,
    /// `force_cancel_clob_orders`.
    pub force_cancel: u32,
    /// `refill_crank_reservoir`: one lamport move and a mirror write.
    pub refill: u32,
}

/// What each of a market's cranks pays its keeper, in lamports. One figure per crank,
/// because a removal and a two-legged cross differ by an order of magnitude and a single
/// figure would either underpay the cross or overpay every removal. The attach derives
/// them once from [`CrankCostUnitsV0`] and `State.transaction_fee_rails`, because an
/// executor that could re-derive its own terms could re-price its own work.
#[zero_copy(unsafe)]
#[derive(Debug, Default, PartialEq, Eq)]
#[repr(C)]
pub struct CrankPaymentsV0 {
    pub removal: u32,
    pub cross: u32,
    pub taker_origin_cross: u32,
    pub trigger: u32,
    pub liquidation: u32,
    pub force_cancel: u32,
    /// What the treasury pays to have this market's reservoir refilled. Stored with the
    /// market's other crank prices because the refill condition lives here, and a
    /// condition has to advertise a floor a turner can filter on.
    pub refill: u32,
    pub padding: u32,
}

impl CrankPaymentsV0 {
    /// Price every crank from one measurement each and the network's current
    /// fee model.
    ///
    /// A crank transaction carries exactly one signature, the turner's fee
    /// payer. An executor names no signer, because relay refuses to sign a
    /// transaction whose executor account list contains one. So there is never
    /// a second signature.
    pub fn derive(
        rails: &crate::state::state::TransactionFeeRails,
        units: &CrankCostUnitsV0,
    ) -> Result<Self> {
        let price = |cost_units: u32| -> Result<u32> {
            let lamports = rails.transaction_cost(u64::from(cost_units), 1)?;
            u32::try_from(lamports).map_err(|_| {
                msg!("crank payment {} lamports does not fit u32", lamports);
                error!(ErrorCode::MathError)
            })
        };

        Ok(Self {
            removal: price(units.removal)?,
            cross: price(units.cross)?,
            taker_origin_cross: price(units.taker_origin_cross)?,
            trigger: price(units.trigger)?,
            liquidation: price(units.liquidation)?,
            force_cancel: price(units.force_cancel)?,
            refill: price(units.refill)?,
            padding: 0,
        })
    }

    /// Extra lamports an expiry crank pays on top of the base removal payment. The base
    /// covers a quiet market only, so a turner paying any priority fee declines. The
    /// offer climbs linearly to [`EXPIRY_ESCALATION_CEILING`] over
    /// [`EXPIRY_ESCALATION_SECONDS`] until a turner takes it. The period is long because
    /// quote and execute already skip an expired order.
    pub fn expiry_escalation(max_ts: i64, now: i64) -> u32 {
        if max_ts == 0 {
            return 0;
        }

        let late = now.saturating_sub(max_ts).max(0) as u64;
        let scaled = late
            .saturating_mul(u64::from(EXPIRY_ESCALATION_CEILING))
            .saturating_div(EXPIRY_ESCALATION_SECONDS);
        u32::try_from(scaled.min(u64::from(EXPIRY_ESCALATION_CEILING)))
            .unwrap_or(EXPIRY_ESCALATION_CEILING)
    }

    /// The largest figure this market's reservoir pays for any one crank. The reservoir
    /// is held between multiples of this rather than of each crank's own price, so a
    /// market above the watermark can always afford its most expensive crank. The refill
    /// is not counted, because the treasury pays that one.
    pub fn max_payment(&self) -> u64 {
        u64::from(
            self.removal
                .max(self.cross)
                .max(self.taker_origin_cross)
                .max(self.trigger)
                .max(self.liquidation)
                .max(self.force_cancel),
        )
    }

    /// The priority fee this transaction paid for the work a liquidation crank is
    /// reimbursed for. Priced on the lesser of what the transaction asked for and what
    /// the crank was measured to need. A fixed figure would overpay a caller that asked
    /// for less, and the request alone would let a caller inflate the limit.
    ///
    /// `max_price_per_unit` caps the per-unit price, because a caller that builds the
    /// block could otherwise pay the fee to itself. A zero cap disables it.
    pub fn crank_priority_lamports(
        price_per_unit: u64,
        requested_units: u32,
        max_price_per_unit: u64,
    ) -> VelocityResult<u64> {
        let price = price_per_unit.min(max_price_per_unit);
        let units = u64::from(requested_units).min(u64::from(LIQUIDATION_CRANK_REIMBURSED_UNITS));
        u64::try_from(
            u128::from(price)
                .safe_mul(u128::from(units))?
                .safe_div_ceil(1_000_000)?,
        )
        .map_err(|_| ErrorCode::MathError)
    }

    /// What a liquidation crank pays on top of its base figure: the transaction's real
    /// cost, bounded by a share of what the liquidation recovered. The base figure
    /// already covers the signature, so only the priority fee is added back.
    ///
    /// Returns zero when the share is unset, the price is unusable, or the cap is below
    /// what was spent. The last case says the liquidation was too small to be worth
    /// landing at this moment's fee, and it becomes worth landing when fees fall.
    pub fn liquidation_reimbursement(
        filled_quote: u64,
        sol_price: i64,
        priority_lamports: u64,
        share_bps: u16,
    ) -> VelocityResult<u64> {
        if share_bps == 0 || sol_price <= 0 || priority_lamports == 0 {
            return Ok(0);
        }

        // The most quote the protocol spends, then the same figure in
        // lamports. `quote / sol_price` is SOL, and a SOL is `LAMPORTS_PER_SOL`.
        let capped_quote = (filled_quote as u128)
            .safe_mul(u128::from(share_bps))?
            .safe_div(10_000)?;
        let cap_lamports = capped_quote
            .safe_mul(u128::from(crate::math::constants::LAMPORTS_PER_SOL_U64))?
            .safe_div(sol_price as u128)?;
        Ok(u64::try_from(cap_lamports.min(u128::from(priority_lamports))).unwrap_or(0))
    }

    /// The quote value of a lamport figure at the SOL oracle price. The cross cranks pay
    /// their keeper in lamports but net the protocol its surplus in quote, so a cross
    /// must clear the keeper payment converted. Rounded up, so the floor never sits below
    /// the true cost. `None` when the price is unusable, which leaves the admin's floor.
    pub fn lamports_to_quote(lamports: u64, sol_price: i64) -> Option<u64> {
        if sol_price <= 0 || lamports == 0 {
            return None;
        }

        let quote = (lamports as u128)
            .checked_mul(sol_price as u128)?
            .div_ceil(u128::from(crate::math::constants::LAMPORTS_PER_SOL_U64));
        u64::try_from(quote).ok()
    }

    /// The largest reservoir-paid crank price. The reservoir must cover this
    /// figure for every crank on the market to run.
    pub fn max(&self) -> u32 {
        [
            self.removal,
            self.cross,
            self.taker_origin_cross,
            self.trigger,
            self.liquidation,
            self.force_cancel,
        ]
        .iter()
        .copied()
        .max()
        .unwrap_or(0)
    }

    /// True when every crank on this market is priced. A zero payment is a
    /// crank no turner takes, so it is a configuration error rather than a
    /// free crank.
    pub fn all_priced(&self) -> bool {
        self.removal > 0
            && self.cross > 0
            && self.taker_origin_cross > 0
            && self.trigger > 0
            && self.liquidation > 0
            && self.force_cancel > 0
            && self.refill > 0
    }
}

#[account(zero_copy(unsafe))]
#[derive(Debug)]
#[repr(C)]
#[derive(Default)]
pub struct ClobCrankConditionsV0 {
    /// Everything relay hosts, in one field. It holds the `relay-spec` header,
    /// the condition slots, and the resolver account list every condition here
    /// points at. It is the first field, so its watch offset is 8.
    pub relay: RelayBlock<CLOB_CRANK_CONDITIONS, CLOB_CRANK_RESOLVER_CAPACITY>,
    /// The market's oracle, captured at attach time. The resolver account list
    /// is fixed, so the staged executor's map section comes from here rather
    /// than from the perp market account. An admin oracle rotation reaches the
    /// cranks on the next attach.
    pub oracle: Pubkey,
    /// Lamports each executor pays its keeper, mirrored into that crank's `min_payment`.
    /// This account is also the reservoir they come from, which costs no extra account.
    /// [`crate::state::crank_treasury::CrankTreasuryV0`] refills it, and an empty
    /// reservoir fails the crank rather than paying nothing.
    pub crank_payments: CrankPaymentsV0,
    /// Floor on the protocol's quote surplus from a cross-match crank, in
    /// `QUOTE_PRECISION`. The reservoir pays `crank_payments.cross` in SOL, so a cross
    /// that clears by a cent is worth declining. In quote rather than lamports, because
    /// the cross crank carries no SOL oracle. Zero means profitable is enough.
    pub min_cross_surplus: u64,
    /// Where the book's own condition block sits in the market account, as it reported at
    /// attach. A market has two blocks and each needs its own relay watch, so a registrar
    /// that watched only this account would leave the book's cranks unwoken.
    pub clob_block_offset: u32,
    /// The region of the book that changes whenever either side's best moves, as the book
    /// reported it at attach. A crossing order is a new best, so a watch here catches
    /// every cross. The book reports the region rather than velocity deriving it, so
    /// velocity watches without knowing the book's layout.
    pub top_of_book_offset: u32,
    pub top_of_book_len: u32,
    /// The perp market these conditions crank. Also the PDA seed.
    pub market_index: u16,
    /// The market's quote spot market, captured at attach time. The staged
    /// executor's map section needs its PDA.
    pub quote_spot_market_index: u16,
    /// The spendable balance this reservoir wakes its refill at, in lamports. Relay
    /// compares the mirror against this number, so the executor reads it rather than
    /// recomputing. A figure from a program constant would drift from conditions written
    /// before an upgrade, and the market would wake at one level and refuse at another.
    pub refill_watermark_lamports: u64,
    /// This account's spendable lamports as of the last payment or refill, meaning the
    /// balance less rent exemption. A relay watch reads account data and a lamport
    /// balance is metadata, so the mirror is what lets the refill condition wake.
    /// Advisory: the refill instruction reads the real balance.
    pub spendable_mirror: u64,
    /// Tail reserve. It holds 4 bytes of alignment slack plus room for a
    /// captured pubkey and change. A resolver that needs another fixed account
    /// takes it from here, instead of forcing an `extend_account` migration on
    /// every market's conditions.
    pub padding: [u8; 16],
}

impl ClobCrankConditionsV0 {
    /// 8 bytes of discriminator, plus the relay block, plus the trailing
    /// fields. A const, so the alignment invariant below is checked at compile
    /// time.
    pub const SIZE: usize = 8
        + RelayBlockV0::<CLOB_CRANK_CONDITIONS, CLOB_CRANK_RESOLVER_CAPACITY>::SIZE
        + 32
        + 32
        + 8
        + 4
        + 4
        + 4
        + 2
        + 2
        + 8
        + 8
        + 16;

    /// Store the resolver account list and describe where it landed.
    pub fn write_resolvers(
        &mut self,
        refs: &[relay_spec::AccountRefV0],
    ) -> Result<relay_spec::ResolverListV0> {
        self.relay
            .write_resolvers(refs)
            .map_err(|_| error!(ErrorCode::DefaultError))
    }

    /// The block region, for `relay_spec::read_block`.
    pub fn block(&self) -> &[u8] {
        ConditionBlock::block(&self.relay)
    }

    // The methods below wrap [`relay_spec::ConditionBlock`], so handlers keep
    // using `?` with the program's own error type.

    pub fn init_block(&mut self) -> Result<()> {
        self.relay
            .init(CLOB_CRANK_BLOCK_OFFSET as u32)
            .map_err(|_| error!(ErrorCode::DefaultError))
    }

    pub fn set_condition(
        &mut self,
        index: usize,
        condition: &relay_spec::ConditionV0,
    ) -> Result<()> {
        ConditionBlock::write_condition(&mut self.relay, index, condition)
            .map_err(|_| error!(ErrorCode::DefaultError))
    }

    pub fn get_condition(&self, index: usize) -> Result<relay_spec::ConditionV0> {
        ConditionBlock::read_condition(&self.relay, index)
            .map_err(|_| error!(ErrorCode::DefaultError))
    }

    pub fn edit_condition(
        &mut self,
        index: usize,
        f: impl FnOnce(&mut relay_spec::ConditionV0),
    ) -> Result<()> {
        ConditionBlock::update_condition(&mut self.relay, index, f)
            .map_err(|_| error!(ErrorCode::DefaultError))
    }

    pub fn clear_condition(&mut self, index: usize) -> Result<()> {
        ConditionBlock::deactivate_condition(&mut self.relay, index)
            .map_err(|_| error!(ErrorCode::DefaultError))
    }

    /// Pay `amount` out of the reservoir to the keeper.
    ///
    /// The rent minimum is the reservoir's own, so a payment never takes the
    /// account below the balance that keeps it alive.
    pub fn pay_keeper<'info>(
        conditions: &AccountLoader<'info, ClobCrankConditionsV0>,
        keeper: &AccountInfo<'info>,
        amount: u64,
    ) -> Result<u64> {
        let info = conditions.to_account_info();
        let rent_minimum = Rent::get()?.minimum_balance(info.data_len());
        Self::pay_keeper_lamports(&info, keeper, amount, rent_minimum)
    }

    /// Move `amount` from the conditions account to `keeper`, so relay's
    /// `assert_paid_v0` sees the keeper's balance grow. Returns the lamports
    /// paid.
    ///
    /// Velocity owns this PDA, so the debit is a direct lamport mutation. A
    /// system-program transfer would need the PDA to sign, and only the owning
    /// program may decrement an account's lamports. The reservoir must stay
    /// rent-exempt, because a balance below the minimum makes the account
    /// purgeable and takes the market's conditions with it. A reservoir that
    /// cannot cover the payment fails the crank here rather than underpaying.
    /// An underpaid crank fails `assert_paid_v0` after doing the work, which
    /// reverts the same way but reports the failure from relay instead of
    /// naming the empty reservoir.
    pub fn pay_keeper_lamports<'info>(
        conditions: &AccountInfo<'info>,
        keeper: &AccountInfo<'info>,
        amount: u64,
        rent_minimum: u64,
    ) -> Result<u64> {
        if amount == 0 {
            return Ok(0);
        }

        let available = conditions.lamports().saturating_sub(rent_minimum);
        if available < amount {
            msg!(
                "clob crank reservoir {} holds {} spendable lamports, needs {}",
                conditions.key(),
                available,
                amount
            );

            return Err(ErrorCode::InsufficientCrankReservoir.into());
        }

        **conditions.try_borrow_mut_lamports()? = conditions
            .lamports()
            .checked_sub(amount)
            .ok_or(ErrorCode::MathError)?;
        **keeper.try_borrow_mut_lamports()? = keeper
            .lamports()
            .checked_add(amount)
            .ok_or(ErrorCode::MathError)?;
        Self::write_spendable_mirror(conditions, rent_minimum)?;
        Ok(amount)
    }

    /// Restate the spendable balance in account data, so the refill condition sees what
    /// the account holds. Writes raw bytes rather than going through the loader, because
    /// a loader borrow here can collide with one the caller still holds.
    pub fn write_spendable_mirror(conditions: &AccountInfo, rent_minimum: u64) -> Result<()> {
        let spendable = conditions.lamports().saturating_sub(rent_minimum);
        let mut data = conditions.try_borrow_mut_data()?;
        const END: usize = CLOB_CRANK_SPENDABLE_MIRROR_OFFSET + 8;
        data.get_mut(CLOB_CRANK_SPENDABLE_MIRROR_OFFSET..END)
            .ok_or(ErrorCode::DefaultError)?
            .copy_from_slice(&spendable.to_le_bytes());
        Ok(())
    }
}

// The block must start at an 8-aligned offset for `read_block`'s zero-copy
// cast. Anchor's discriminator puts field 0 at offset 8.
const _: () = assert!(CLOB_CRANK_BLOCK_OFFSET.is_multiple_of(8));

// Zero-copy alignment invariant (see docs/alignment-and-native-offsets.md):
// no u128 fields, and `(SIZE - 8) % 16 == 0` so the struct sizes identically
// on x86_64 and SBF.
const _: () = assert!((ClobCrankConditionsV0::SIZE - 8).is_multiple_of(16));

#[cfg(test)]
mod tests {
    use {
        super::*,
        crate::math::constants::{PRICE_PRECISION_I64, QUOTE_PRECISION_U64},
        relay_spec::{bytemuck::Zeroable, ResolvedCrankV0, ResponsePointerV0},
    };

    /// Both conditions here describe this account rather than the book: a poll for a
    /// cross a PropAMM created by repricing, and the reservoir's refill watermark. A
    /// failure means a condition was added that belongs on the book.
    #[test]
    fn the_crank_terms_cost_no_account_space() {
        assert_eq!(CLOB_CRANK_CONDITIONS, 2);
        assert_eq!(std::mem::size_of::<ClobCrankConditionsV0>(), 800);
        assert_eq!(ClobCrankConditionsV0::SIZE, 808);
        // The refill condition watches this offset. A field reordered above
        // the mirror moves it, and every market's condition would then wake on
        // whatever moved into its place.
        assert_eq!(CLOB_CRANK_SPENDABLE_MIRROR_OFFSET, 784);
        // A market that sets no floor requires only that the cross is
        // profitable.
        assert_eq!(ClobCrankConditionsV0::default().min_cross_surplus, 0);
        // An unpriced market arms nothing. Every payment is zero, which
        // `write_crank_conditions` refuses.
        assert!(!ClobCrankConditionsV0::default().crank_payments.all_priced());
    }

    /// The offer climbs with the wait and then stops. A stuck expiry beats the
    /// fee a turner pays, and the protocol still bounds what it offers.
    #[test]
    fn an_expiry_offer_climbs_with_the_delay_and_caps() {
        // Not yet due, and exactly due, add nothing.
        assert_eq!(CrankPaymentsV0::expiry_escalation(1_000, 900), 0);
        assert_eq!(CrankPaymentsV0::expiry_escalation(1_000, 1_000), 0);
        // Linear across the window.
        let half = EXPIRY_ESCALATION_SECONDS as i64 / 2;
        assert_eq!(
            CrankPaymentsV0::expiry_escalation(1_000, 1_000 + half),
            EXPIRY_ESCALATION_CEILING / 2
        );

        // At the window and far past it, the ceiling and no more.
        let full = EXPIRY_ESCALATION_SECONDS as i64;
        assert_eq!(
            CrankPaymentsV0::expiry_escalation(1_000, 1_000 + full),
            EXPIRY_ESCALATION_CEILING
        );
        assert_eq!(
            CrankPaymentsV0::expiry_escalation(1_000, 1_000 + full * 1_000),
            EXPIRY_ESCALATION_CEILING
        );

        // A good-till-cancelled order never escalates: it has no deadline to
        // be late for.
        assert_eq!(CrankPaymentsV0::expiry_escalation(0, i64::MAX), 0);
    }

    /// A liquidation repays what the crank spent, and never more than the
    /// share of the recovery the protocol set aside for it.
    #[test]
    fn a_liquidation_repays_the_fee_up_to_its_share() {
        const SOL: i64 = 200 * PRICE_PRECISION_I64; // $200
        const HUNDRED_DOLLARS: u64 = 100 * QUOTE_PRECISION_U64;

        // 20% of $100 is $20, which at $200/SOL is 0.1 SOL.
        let cap = 100_000_000u64;
        // A fee under the cap is repaid in full. The keeper is made whole and
        // no more, so a higher bid wins it nothing.
        assert_eq!(
            CrankPaymentsV0::liquidation_reimbursement(HUNDRED_DOLLARS, SOL, 30_000, 2_000)
                .unwrap(),
            30_000
        );

        // A fee over the cap is truncated to the cap. The keeper is short, so
        // it declines. This liquidation is not worth landing at that fee.
        assert_eq!(
            CrankPaymentsV0::liquidation_reimbursement(HUNDRED_DOLLARS, SOL, cap * 5, 2_000)
                .unwrap(),
            cap
        );

        // Every input that means "do not reimburse" pays nothing rather than
        // failing. The flat payment still stands and the crank still lands.
        assert_eq!(
            CrankPaymentsV0::liquidation_reimbursement(HUNDRED_DOLLARS, SOL, 30_000, 0).unwrap(),
            0
        );
        assert_eq!(
            CrankPaymentsV0::liquidation_reimbursement(HUNDRED_DOLLARS, 0, 30_000, 2_000).unwrap(),
            0
        );
        assert_eq!(
            CrankPaymentsV0::liquidation_reimbursement(HUNDRED_DOLLARS, SOL, 0, 2_000).unwrap(),
            0
        );

        // A cent of recovery buys a cent of crank. 20% of $0.01 at $200 per
        // SOL is 10,000 lamports, short of the 30,000 fee. The keeper declines
        // and the liquidation waits for a cheaper block or a worse account.
        assert_eq!(
            CrankPaymentsV0::liquidation_reimbursement(
                QUOTE_PRECISION_U64 / 100,
                SOL,
                30_000,
                2_000
            )
            .unwrap(),
            10_000
        );
    }

    /// One measurement per crank, one rate for the protocol.
    ///
    /// The cross costs six times the removal because it requests six times as
    /// much. A single figure for the market would have to be one or the other,
    /// which is why the prices are derived rather than set.
    #[test]
    fn each_crank_is_priced_from_what_it_requests() {
        let units = CrankCostUnitsV0 {
            removal: 30_000,
            cross: 180_000,
            taker_origin_cross: 190_000,
            trigger: 40_000,
            liquidation: 120_000,
            force_cancel: 60_000,
            refill: 30_000,
        };

        // A flat charge per signature prices every crank the same, whatever it
        // asks for.
        let flat = CrankPaymentsV0::derive(
            &crate::state::state::TransactionFeeRails::FLAT_PER_SIGNATURE,
            &units,
        )
        .unwrap();
        assert_eq!(flat.removal, 5_000);
        assert_eq!(flat.cross, 5_000);
        assert_eq!(flat.max(), 5_000);

        // Charge for what a transaction requests and they separate.
        let rails = crate::state::state::TransactionFeeRails {
            inclusion_lamports: 2_500,
            signature_lamports: 0,
            resource_fee_numerator: 1,
            resource_fee_denominator: 2,
            max_priority_micro_lamports_per_cu: 0,
        };
        let priced = CrankPaymentsV0::derive(&rails, &units).unwrap();
        assert_eq!(priced.removal, 2_500 + 15_000);
        assert_eq!(priced.cross, 2_500 + 90_000);
        assert_eq!(priced.taker_origin_cross, 2_500 + 95_000);
        assert_eq!(priced.max(), priced.taker_origin_cross);
        assert!(priced.all_priced());
        // A removal paid the cross price costs four times too much. A cross
        // paid the removal price is a crank nobody runs.
        assert!(priced.cross > priced.removal * 4);
    }

    /// The rate rounds up. A zero denominator turns the resource fee off
    /// instead of dividing.
    #[test]
    fn the_resource_rate_rounds_up_and_switches_off_cleanly() {
        use crate::state::state::TransactionFeeRails;

        let tenth = TransactionFeeRails {
            inclusion_lamports: 2_500,
            signature_lamports: 0,
            resource_fee_numerator: 1,
            resource_fee_denominator: 10,
            max_priority_micro_lamports_per_cu: 0,
        };

        // 3 units at a tenth of a lamport each is a third of a lamport. A
        // payment short by a lamport buys nothing.
        assert_eq!(tenth.transaction_cost(3, 1).unwrap(), 2_501);
        assert_eq!(tenth.transaction_cost(30_000, 1).unwrap(), 2_500 + 3_000);

        let off = TransactionFeeRails {
            inclusion_lamports: 2_500,
            signature_lamports: 5_000,
            resource_fee_numerator: 1,
            resource_fee_denominator: 0,
            max_priority_micro_lamports_per_cu: 0,
        };

        assert_eq!(off.transaction_cost(u64::MAX, 2).unwrap(), 2_500 + 10_000);
    }

    #[test]
    fn size_matches_the_layout_and_the_spec() {
        // The account must fit anchor init's 10,240-byte CPI allocation
        // ceiling, or an attach of a CLOB to a market fails.
        assert!(ClobCrankConditionsV0::SIZE <= 10_240);
        // The watch registers at the relay block, which is the first field.
        assert_eq!(CLOB_CRANK_BLOCK_OFFSET, 8);
        // The u64 reservoir fields must land 8-aligned, past the block.
        assert_eq!(std::mem::align_of::<ClobCrankConditionsV0>(), 8);
        assert_eq!(
            std::mem::size_of::<ClobCrankConditionsV0>(),
            ClobCrankConditionsV0::SIZE - 8
        );
    }

    #[test]
    fn header_then_conditions_round_trip_through_the_spec() {
        let mut acct = ClobCrankConditionsV0::default();
        acct.init_block().unwrap();
        let resolvers = acct
            .write_resolvers(&[relay_spec::AccountRefV0::writable([9; 32])])
            .unwrap();

        // Built the way a host builds one. The constructor marks a condition
        // active. `set_wake` only rewrites the wake.
        let condition = relay_spec::ConditionV0::at_timestamp(
            1_234,
            relay_spec::CrankSpecV0 {
                resolver_program: crate::ID.to_bytes(),
                resolver_disc: [1; 8],
                min_payment: 5_000,
            },
            resolvers,
        );

        acct.set_condition(CLOB_CRANK_CROSS_FALLBACK, &condition)
            .unwrap();

        // Relay's own reader must accept the written block, at offset 0 of the
        // region and offset 8 of the account.
        let (header, conditions) = relay_spec::read_block(acct.block(), 0).unwrap();
        assert_eq!(header.num_conditions, CLOB_CRANK_CONDITIONS as u8);
        assert_eq!(conditions.len(), CLOB_CRANK_CONDITIONS);
        assert_eq!(
            conditions[CLOB_CRANK_CROSS_FALLBACK].wake(),
            Ok(relay_spec::WakeView::AtTimestamp { unix_ts: 1_234 })
        );

        assert!(conditions[CLOB_CRANK_CROSS_FALLBACK].is_active());

        assert_eq!(
            acct.get_condition(CLOB_CRANK_CROSS_FALLBACK)
                .unwrap()
                .wake(),
            Ok(relay_spec::WakeView::AtTimestamp { unix_ts: 1_234 })
        );
    }

    #[test]
    fn staged_payload_round_trips_through_the_pointer() {
        // The shared scratch account holds the staged response, so the pointer
        // names scratch at index 0.
        let mut scratch = crate::state::relay_scratch::RelayScratchV0::default();
        let resolved = ResolvedCrankV0::new(
            crate::ID.to_bytes(),
            [2; 8],
            (0..11u8)
                .map(|i| relay_spec::AccountRefV0::writable([i; 32]))
                .collect(),
            vec![1, 2, 3],
        );
        let pointer_bytes = scratch.stage(&resolved).unwrap();
        let pointer = ResponsePointerV0::read(&pointer_bytes).unwrap();
        assert!(pointer.has_work());
        assert_eq!(
            pointer.account_index,
            crate::state::relay_scratch::RELAY_SCRATCH_ACCOUNT_INDEX
        );
        assert_eq!(
            pointer.offset(),
            crate::state::relay_scratch::RELAY_SCRATCH_OFFSET
        );

        let staged = &scratch.scratch[..pointer.len() as usize];
        assert_eq!(ResolvedCrankV0::read(staged).unwrap(), resolved);
    }

    #[test]
    fn out_of_range_condition_index_is_rejected() {
        let mut acct = ClobCrankConditionsV0::default();
        acct.init_block().unwrap();
        assert!(acct
            .set_condition(CLOB_CRANK_CONDITIONS, &relay_spec::ConditionV0::zeroed())
            .is_err());
        assert!(acct.get_condition(CLOB_CRANK_CONDITIONS).is_err());
    }

    /// The reservoir is held against the most expensive crank it pays, so a
    /// market above its watermark can afford any one of them. The refill is
    /// excluded even when it is the largest figure on the account. The
    /// treasury pays the refill, and counting it would raise the balance every
    /// market has to hold.
    #[test]
    fn the_watermark_follows_the_dearest_crank() {
        let payments = CrankPaymentsV0 {
            removal: 5_100,
            cross: 21_000,
            taker_origin_cross: 18_000,
            trigger: 6_000,
            liquidation: 12_000,
            force_cancel: 7_000,
            // Higher than every reservoir-paid crank, and not counted. The
            // treasury pays the refill, so it must not raise the balance a
            // reservoir is held at.
            refill: 90_000,
            padding: 0,
        };

        assert_eq!(payments.max_payment(), 21_000);
    }

    /// The mirror is what the refill condition reads, so a payment that did
    /// not restate it would leave a drained reservoir looking full.
    #[test]
    fn a_payment_restates_the_mirror() {
        let key = Pubkey::new_unique();
        let mut lamports = 10_000_000u64;
        let mut data = vec![0u8; ClobCrankConditionsV0::SIZE];
        let owner = crate::ID;
        let conditions =
            AccountInfo::new(&key, false, true, &mut lamports, &mut data, &owner, false);
        let rent_minimum = 5_000_000;

        ClobCrankConditionsV0::write_spendable_mirror(&conditions, rent_minimum).unwrap();
        assert_eq!(read_mirror(&conditions), 5_000_000);

        let mut keeper_lamports = 0u64;
        let mut keeper_data = Vec::new();
        let keeper_key = Pubkey::new_unique();
        let keeper = AccountInfo::new(
            &keeper_key,
            false,
            true,
            &mut keeper_lamports,
            &mut keeper_data,
            &owner,
            false,
        );

        ClobCrankConditionsV0::pay_keeper_lamports(&conditions, &keeper, 1_500_000, rent_minimum)
            .unwrap();
        assert_eq!(read_mirror(&conditions), 3_500_000);
    }

    fn read_mirror(conditions: &AccountInfo) -> u64 {
        let data = conditions.try_borrow_data().unwrap();
        let mut bytes = [0u8; 8];
        bytes.copy_from_slice(
            &data[CLOB_CRANK_SPENDABLE_MIRROR_OFFSET..CLOB_CRANK_SPENDABLE_MIRROR_OFFSET + 8],
        );

        u64::from_le_bytes(bytes)
    }

    /// A crank that requests less than the measured figure is reimbursed for
    /// what it requested. Reimbursement of the measured figure would hand it
    /// the difference as profit on every liquidation.
    #[test]
    fn priority_lamports_price_the_smaller_request() {
        let under = LIQUIDATION_CRANK_REIMBURSED_UNITS / 2;
        assert_eq!(
            CrankPaymentsV0::crank_priority_lamports(1_000_000, under, u64::MAX).unwrap(),
            u64::from(under)
        );
    }

    /// A crank that inflates its limit is reimbursed only for the measured
    /// figure, so it cannot bill the difference.
    #[test]
    fn priority_lamports_cap_an_inflated_request() {
        let capped = CrankPaymentsV0::crank_priority_lamports(
            1_000_000,
            LIQUIDATION_CRANK_REIMBURSED_UNITS,
            u64::MAX,
        )
        .unwrap();
        assert_eq!(
            CrankPaymentsV0::crank_priority_lamports(1_000_000, u32::MAX, u64::MAX).unwrap(),
            capped
        );
        assert_eq!(capped, u64::from(LIQUIDATION_CRANK_REIMBURSED_UNITS));
    }

    /// An honest crank is made whole. The fee it paid is the fee it gets back.
    #[test]
    fn priority_lamports_match_what_the_runtime_charges() {
        let price = 37_500;
        let units = 250_000;
        let charged = u64::from(price) * u64::from(units) / 1_000_000;
        assert_eq!(
            CrankPaymentsV0::crank_priority_lamports(price, units, u64::MAX).unwrap(),
            charged
        );
    }

    /// The per-unit price is clamped to the ceiling, so a caller cannot bill
    /// an arbitrary compute-unit price back to the reservoir.
    #[test]
    fn priority_lamports_clamp_the_price_per_unit() {
        let units = 200_000;
        let ceiling = 10_000;
        // A price above the ceiling is reimbursed at the ceiling.
        assert_eq!(
            CrankPaymentsV0::crank_priority_lamports(1_000_000, units, ceiling).unwrap(),
            u64::from(units) * ceiling / 1_000_000
        );

        // A zero ceiling disables the priority reimbursement.
        assert_eq!(
            CrankPaymentsV0::crank_priority_lamports(1_000_000, units, 0).unwrap(),
            0
        );

        // A price below the ceiling is reimbursed at what it asked for.
        assert_eq!(
            CrankPaymentsV0::crank_priority_lamports(5_000, units, ceiling).unwrap(),
            u64::from(units) * 5_000 / 1_000_000
        );
    }
}
