//! Relay condition block for a perp market's CLOB cranks.
//!
//! Relay turners discover work by reading a *condition block* — a
//! `relay-spec` wire structure naming, per condition, when to wake, which
//! instruction to simulate to find work (the resolver), and which instruction
//! does it (the executor).
//!
//! A condition splits into two halves with different owners. The wake is a
//! fact about an account, so it belongs to the program that writes that
//! account. The resolver is what to do about it, and removing an order adjusts
//! the maker's `User` — open-order aggregates and the reward debit — which
//! only velocity can do. So the book hosts the wakes for its own state and
//! velocity registers the resolvers that answer them, and this account holds
//! the one wake that is velocity's own.
//!
//! One condition per market, [`CLOB_CRANK_CROSS_FALLBACK`]: the periodic poll
//! that catches a cross a PropAMM created by repricing, which changes nothing
//! on the book and so fires no watch.
//!
//! The book's own work — an expired order, a side at its eviction threshold,
//! a crossed book, an order reaching its activation slot — wakes off
//! conditions the CLOB hosts on the market account itself, registered here at
//! attach through `set_crank_conditions_v0`. They name velocity's resolvers
//! and pay out of this account's reservoir, so what runs is still velocity's;
//! what is watched is the book's. That split is why there is no expiry
//! fallback poll any more: a poll covers a hint whose maintenance is
//! best-effort, and the book maintains those in the same instruction that
//! changes what they describe.
//!
//! Resolvers stage their `ResolvedCrankV0` (executor account list + args) into
//! the program-wide [`crate::state::relay_scratch::RelayScratchV0`], not into
//! this account — a resolver only ever runs under simulation, so the staged
//! bytes never land on chain and two turners cannot collide. See that module
//! for why the region is shared rather than per conditions account.
//!
//! The block is held as an opaque byte region accessed through
//! `relay_spec::read_block` / `read_block_mut` rather than as typed fields.
//! That keeps `relay-spec`'s pod types out of velocity's zero-copy layout —
//! the region's size is the only thing this account commits to — and means a
//! spec revision that adds a field is a version bump here, not a layout
//! migration.
//!
//! `relay` is the FIRST field so it begins at offset 8 (past anchor's
//! discriminator), which is the 8-aligned offset `read_block` requires.

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

/// Index of the cross fallback: `WakeKind::EverySlots`.
///
/// The only condition velocity hosts for a market. The book hosts the four
/// that describe its own state — an expiry, an activation, a side at its cap,
/// a crossed book — because it is the account those facts live on and the
/// only program that can keep their wakes current without being handed a
/// second account on every write.
///
/// This one is not about the book. A PropAMM that reprices into a cross with
/// the book changes nothing on the book's account, so no watch on it fires,
/// and the maker's own reprice watch belongs to the maker's conditions. The
/// poll is that case's liveness floor.
pub const CLOB_CRANK_CROSS_FALLBACK: usize = 0;
/// Index of the reservoir refill: `WakeKind::OnValueCross`.
///
/// The reservoir that pays this market's cranks mirrors its own spendable
/// lamports into [`ClobCrankConditionsV0::spendable_mirror`], and this
/// condition wakes when that value falls to the treasury's watermark. The
/// refill then moves lamports from the protocol treasury into the reservoir.
///
/// The watched value sits on this same account, so the watch that already
/// finds this block also covers it. No second watch is registered.
pub const CLOB_CRANK_REFILL: usize = 1;
/// Conditions hosted per market.
pub const CLOB_CRANK_CONDITIONS: usize = 2;

/// Every condition on this account resolves with the same five accounts;
/// the capacity is [`RelayBlockV0`]'s minimum granularity of 8.
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

/// Compute units a liquidation crank's priority fee is reimbursed against.
///
/// Deliberately a measured figure rather than the limit a transaction asks
/// for: reimbursing the request would let a caller inflate its own bill, and
/// a keeper paid for an honest crank has every reason to size its request
/// tightly.
pub const LIQUIDATION_CRANK_REIMBURSED_UNITS: u32 = 400_000;

/// Least filled quote value a liquidation crank must recover to earn its flat
/// reservoir payment, in `QUOTE_PRECISION` (ten dollars).
///
/// The flat payment is paid once per crank, whatever it filled. Without a
/// floor a keeper stages one liquidation as many tiny fills and collects the
/// flat payment on each, draining the reservoir for work that recovered almost
/// nothing. A crank that fills less than this still liquidates the position;
/// it just does not draw the flat payment, so dust is cranked without paying
/// to farm it.
pub const LIQUIDATION_FLAT_PAYMENT_MIN_FILLED_QUOTE: u64 = 10_000_000;

/// Cost units each of a market's cranks requests, one field per crank.
///
/// Measured, not guessed: a turner simulates the crank and requests a compute
/// limit from what it burned, and the rest of the sum — signatures, write
/// locks, instruction-data bytes, the loaded-accounts limit — falls out of the
/// transaction it assembles. An admin passes those totals here.
///
/// The unit is the block-packing cost unit, which is what the network prices a
/// transaction by. `State.transaction_fee_rails` turns it into lamports.
#[derive(AnchorSerialize, AnchorDeserialize, Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CrankCostUnitsV0 {
    /// `evict_worst` / `remove_expired`: one book write and a hint repair.
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

/// What each of a market's cranks pays its keeper, in lamports.
///
/// One figure per crank rather than one for the market. A book removal and a
/// two-legged cross differ by an order of magnitude in what they request, and
/// the network charges a transaction for what it requests — so a single figure
/// either underpays the cross, and nobody runs it, or overpays every removal.
///
/// Derived once at attach time from [`CrankCostUnitsV0`] and
/// `State.transaction_fee_rails`. Stored rather than recomputed at crank time
/// for two reasons: pricing itself would cost a crank compute and an extra
/// account, and a staged executor that could re-derive its own terms could
/// re-price its own work.
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
    /// What the *treasury* pays to have this market's reservoir refilled.
    ///
    /// Stored with the market's other crank prices even though the treasury is
    /// the purse, because this is where the refill condition lives and a
    /// condition has to advertise a floor a turner can filter on. Derived from
    /// the same rails as every other crank, so re-pricing the network
    /// re-prices this too on the market's next attach.
    pub refill: u32,
    pub padding: u32,
}

impl CrankPaymentsV0 {
    /// Price every crank off one measurement each and the network's current
    /// fee model.
    ///
    /// A crank transaction carries exactly one signature — the turner's fee
    /// payer. Executors name no signer at all (relay refuses to sign a
    /// transaction whose executor account list contains one), so there is
    /// never a second.
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

    /// Extra lamports an expiry crank pays for every second it went unclaimed,
    /// on top of the base removal payment.
    ///
    /// A crank's base payment covers what the transaction costs to land in a
    /// quiet market and nothing more, so a turner paying any priority fee is
    /// out of pocket and rationally declines — exactly when congestion is what
    /// stopped the crank in the first place. The offer therefore climbs with
    /// the delay, and the turner takes it at whatever point it beats the fee
    /// it has to pay. Nobody is reimbursed for a number they chose: the
    /// protocol sets the price and the turner decides whether to accept it.
    ///
    /// Linear, from nothing at the moment the order comes due to
    /// [`EXPIRY_ESCALATION_CEILING`] after [`EXPIRY_ESCALATION_SECONDS`].
    ///
    /// The period is long because an expired order is not urgent: quote and
    /// execute already skip it, so it causes no bad fills while it rests. The
    /// only cost of leaving it is its owner's margin staying reserved. A fast
    /// ramp would pay a premium for ordinary latency; this one still clears a
    /// stuck order inside a congestion episode.
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

    /// What a liquidation crank pays on top of its base figure: what the
    /// transaction actually cost, bounded by a share of what the liquidation
    /// recovered.
    ///
    /// `filled_quote` is the liquidation's own filled value, `sol_price` the
    /// oracle's SOL price — both in `PRICE_PRECISION`-scaled quote — and
    /// `priority_lamports` the fee the transaction paid for its position in
    /// the block. The base figure already covers the signature, so only the
    /// priority fee is added back.
    ///
    /// Returns zero when the share is unset, the price is unusable, or the
    /// cap is below what was spent. The last of those is not a failure: it
    /// says this liquidation was too small to be worth landing at this
    /// moment's fee, and a keeper that agrees will leave it. It becomes worth
    /// landing when fees fall or the account deteriorates further.
    /// The largest figure this market's *reservoir* pays for any one crank.
    ///
    /// The reservoir is held between multiples of this rather than of each
    /// crank's own price, so a market can always afford its most expensive
    /// crank while it is above the watermark. The refill is not among them:
    /// the treasury pays that one, and a reservoir never spends on being
    /// filled.
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

    /// The priority fee this transaction paid for the work a liquidation
    /// crank is reimbursed for.
    ///
    /// Priced on the lesser of what the transaction asked for and what the
    /// crank was measured to need. Both bounds are load-bearing, in opposite
    /// directions. The runtime charges the priority fee on the limit a
    /// transaction *requests*, so a fixed figure would pay a caller that
    /// requests less than that figure more than it spent — a profit, drawn
    /// from the reservoir, on every liquidation. The request alone would
    /// instead let a caller inflate the limit and bill the difference.
    /// The smaller of the two leaves nothing in either direction.
    ///
    /// The per-unit price is capped at `max_price_per_unit`. The caller sets
    /// the price, so an uncapped price lets a caller that builds the block pay
    /// the fee to itself and bill the reservoir any amount. A zero cap prices
    /// the priority fee at nothing, which disables the reimbursement.
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

    pub fn liquidation_reimbursement(
        filled_quote: u64,
        sol_price: i64,
        priority_lamports: u64,
        share_bps: u16,
    ) -> VelocityResult<u64> {
        if share_bps == 0 || sol_price <= 0 || priority_lamports == 0 {
            return Ok(0);
        }
        // Quote the protocol will spend at most, then the same figure in
        // lamports: `quote / sol_price` is SOL, and a SOL is `LAMPORTS_PER_SOL`.
        let capped_quote = (filled_quote as u128)
            .safe_mul(u128::from(share_bps))?
            .safe_div(10_000)?;
        let cap_lamports = capped_quote
            .safe_mul(u128::from(crate::math::constants::LAMPORTS_PER_SOL_U64))?
            .safe_div(sol_price as u128)?;
        Ok(u64::try_from(cap_lamports.min(u128::from(priority_lamports))).unwrap_or(0))
    }

    /// The quote value of a lamport figure at the SOL oracle price.
    ///
    /// The cross cranks pay their keeper in lamports but net the protocol its
    /// surplus in quote. A floor set in one unit cannot bound a cost in the
    /// other, so a cross must clear at least the keeper payment converted to
    /// quote or the protocol loses on it net of what it pays to land it.
    /// Rounded up, so the floor never sits below the true cost. Returns `None`
    /// when the price is unusable, leaving the admin's own floor to stand.
    pub fn lamports_to_quote(lamports: u64, sol_price: i64) -> Option<u64> {
        if sol_price <= 0 || lamports == 0 {
            return None;
        }
        let quote = (lamports as u128)
            .checked_mul(sol_price as u128)?
            .div_ceil(u128::from(crate::math::constants::LAMPORTS_PER_SOL_U64));
        u64::try_from(quote).ok()
    }

    /// The largest of them. What the reservoir has to be able to cover for
    /// every crank on the market to run.
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
    /// Everything relay needs hosted, in one field: the `relay-spec` header,
    /// the condition slots, and the resolver account list every condition
    /// here points at. First field, so its watch offset is 8.
    pub relay: RelayBlock<CLOB_CRANK_CONDITIONS, CLOB_CRANK_RESOLVER_CAPACITY>,
    /// The market's oracle, captured at attach time. Resolvers hold only
    /// four fixed accounts, so the staged executor's map section is derived
    /// from here rather than from the perp market account; an admin oracle
    /// rotation goes live for the cranks on re-attach.
    pub oracle: Pubkey,
    /// Lamports each executor pays its keeper, mirrored into that crank's
    /// `min_payment`. This account doubles as the reservoir those lamports
    /// come from: relay's `assert_paid_v0` measures the keeper's lamport
    /// balance, so a crank that moves no lamports cannot express a fee, and
    /// turners would have no signal to prioritize (or decline) the work. Held
    /// here rather than in a global PDA because the executor already has to
    /// touch this account to repair the expiry hint — so the reservoir costs
    /// no extra account in a crank transaction.
    ///
    /// Refilled by the maker, not the protocol: the flat removal reward the
    /// maker pays accrues to a protocol-owned `User`, and a hot role withdraws
    /// that quote and converts it to SOL to top these reservoirs off. An empty
    /// reservoir stops cranks rather than silently paying nothing, which is the
    /// failure mode ops can actually see.
    pub crank_payments: CrankPaymentsV0,
    /// Floor on the protocol's quote surplus from a cross-match crank, in
    /// QUOTE_PRECISION. A cross costs the protocol real SOL — the reservoir
    /// pays `crank_payments.cross` to whoever cranked it — so a cross that
    /// clears by a cent is a cross worth declining. Zero keeps the bare
    /// "strictly profitable" rule.
    ///
    /// Denominated in quote rather than derived from the lamport cost because
    /// the conversion needs a SOL price, and the cross crank carries no SOL
    /// oracle (it holds the perp's oracle and its map section, nothing more).
    /// Admins set it to cover the cross payout with margin and re-price it
    /// alongside the payments, which is the same cadence.
    pub min_cross_surplus: u64,
    /// Where the book's own condition block sits in the market account, as it
    /// reported at attach.
    ///
    /// A market has two blocks and each needs its own relay watch: this
    /// account's, whose block is its first field at offset 8, and the book's,
    /// which holds the four conditions describing the book itself. A
    /// registrar that watches only this account leaves the book's cranks
    /// unwoken, so the offset is captured here for it to find.
    pub clob_block_offset: u32,
    /// The region of the book that changes whenever either side's best moves,
    /// as the book reported it at attach.
    ///
    /// A crossing order is by definition a new best, so a relay watch here
    /// catches every cross the moment it appears. Captured rather than
    /// derived: the book answers where its own heads sit, so velocity
    /// registers a watch on it without knowing its layout. Read by the
    /// per-quoter cross conditions, which watch this same book for a cross
    /// against a PropAMM.
    pub top_of_book_offset: u32,
    pub top_of_book_len: u32,
    /// The perp market these conditions crank. Also the PDA seed.
    pub market_index: u16,
    /// The market's quote spot market, captured at attach time (the staged
    /// executor's map section needs its PDA).
    pub quote_spot_market_index: u16,
    /// The spendable balance this reservoir wakes its refill at, in lamports.
    ///
    /// Resolved at attach from the treasury's watermark setting and this
    /// market's dearest crank, and stored because it is the threshold the
    /// wake condition carries: relay compares the mirror against this number,
    /// so the executor has to read the same one rather than recompute it. A
    /// figure recomputed from a program constant would drift from the
    /// conditions written before an upgrade, and a market would wake at one
    /// level while its executor refused at another.
    pub refill_watermark_lamports: u64,
    /// This account's spendable lamports — its balance less its rent
    /// exemption — as of the last payment or refill.
    ///
    /// A relay watch reads account data, and a lamport balance is account
    /// metadata rather than data. Mirroring it here is what lets the refill
    /// condition wake on a draining reservoir. The write costs nothing: every
    /// payment already writes this account.
    ///
    /// Advisory, not authoritative. The refill instruction reads the real
    /// balance, and the resolver refuses to stage one against a reservoir that
    /// is genuinely full.
    ///
    /// Written by the attach and by every payment, which is every way the
    /// balance falls. A plain lamport transfer into the reservoir is the one
    /// way it can rise without a write, and that leaves the mirror low: the
    /// condition then stays due and turners keep resolving it to "no work"
    /// until the next payment restates it. That costs simulations rather than
    /// lamports, and the treasury refill exists so that hand-funding a
    /// reservoir is not the normal path.
    pub spendable_mirror: u64,
    /// Tail reserve: 4 bytes of alignment slack plus room for a captured
    /// pubkey and change, so a resolver that needs another fixed account can
    /// take it from here instead of forcing an `extend_account` migration on
    /// every market's conditions.
    pub padding: [u8; 16],
}

// `padding` is longer than 32 bytes, which `#[derive(Default)]` does not
// cover (arrays only derive it up to 32).

impl ClobCrankConditionsV0 {
    /// 8 (discriminator) + the relay block + trailing fields. Kept as a
    /// const so the alignment invariant below is checked at compile time.
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

    /// Anchor-flavoured wrappers over [`relay_spec::ConditionBlock`]'s
    /// provided methods, so handlers keep using `?` with the program's own
    /// error type.
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

    /// Move `keeper_payment_lamports` from the conditions account to `keeper`,
    /// so relay's `assert_paid_v0` sees the keeper's balance grow.
    ///
    /// Velocity owns this PDA, so the debit is a direct lamport mutation — a
    /// system-program transfer would need the PDA to sign, and only the owning
    /// program may decrement an account's lamports anyway. The reservoir must
    /// stay rent-exempt: dropping below the minimum would make the account
    /// purgeable and take the market's conditions with it. When it can't cover
    /// the payment the crank fails here rather than underpaying, because an
    /// underpaid crank fails `assert_paid_v0` after doing the work — same
    /// revert, but the reason would be buried in relay instead of naming the
    /// empty reservoir.
    ///
    /// Returns the lamports paid.
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

    /// Restate the spendable balance in account data, so the refill condition
    /// sees what the account actually holds.
    ///
    /// Written as raw bytes rather than through the loader because every
    /// caller already holds this account as an `AccountInfo`, and a loader
    /// borrow here would collide with one the caller may still hold.
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
// cast; anchor's discriminator puts field 0 at offset 8.
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

    /// The account holds two conditions, and both describe this account
    /// rather than the book: the poll for a cross a PropAMM created by
    /// repricing, and the reservoir falling to its refill watermark. The
    /// book's own four moved onto the book. If this fails, a condition was
    /// added here that belongs on the account whose state it describes.
    #[test]
    fn the_crank_terms_cost_no_account_space() {
        assert_eq!(CLOB_CRANK_CONDITIONS, 2);
        assert_eq!(std::mem::size_of::<ClobCrankConditionsV0>(), 800);
        assert_eq!(ClobCrankConditionsV0::SIZE, 808);
        // The refill condition watches this offset. A field reordered above
        // the mirror moves it, and every market's condition would then wake on
        // whatever moved into its place.
        assert_eq!(CLOB_CRANK_SPENDABLE_MIRROR_OFFSET, 784);
        // Default is the bare "strictly profitable" rule: a market that never
        // sets a floor behaves as it did before the field existed.
        assert_eq!(ClobCrankConditionsV0::default().min_cross_surplus, 0);
        // And an unpriced market arms nothing: every payment is zero, which
        // `write_clob_crank_conditions` refuses.
        assert!(!ClobCrankConditionsV0::default().crank_payments.all_priced());
    }

    /// The offer climbs with the wait and then stops, so a stuck expiry
    /// eventually beats any fee a turner is paying without the protocol
    /// writing a blank cheque.
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
        // A fee under the cap is repaid in full — the keeper is made whole
        // and no more, so bidding higher wins it nothing.
        assert_eq!(
            CrankPaymentsV0::liquidation_reimbursement(HUNDRED_DOLLARS, SOL, 30_000, 2_000)
                .unwrap(),
            30_000
        );
        // A fee over the cap is truncated to it. The keeper is short, so it
        // declines — this liquidation is not worth landing at that fee.
        assert_eq!(
            CrankPaymentsV0::liquidation_reimbursement(HUNDRED_DOLLARS, SOL, cap * 5, 2_000)
                .unwrap(),
            cap
        );
        // Everything that means "do not reimburse" pays nothing rather than
        // failing: the flat payment still stands and the crank still lands.
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
        // A cent of recovery buys a cent's worth of crank: 20% of $0.01 at
        // $200/SOL is 10,000 lamports, short of the 30,000 fee. The keeper
        // declines and the liquidation waits for a cheaper block or a worse
        // account — which is the answer, not a failure.
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
    /// The point of deriving rather than setting: the cross costs six times
    /// the removal because it requests six times as much, and a single figure
    /// for the market would have to be one or the other.
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

        // A flat charge per signature prices every crank the same, however
        // much it asks for. This is the model the payments were sized under
        // when they were one number.
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
        // A removal paid the cross's price is four times what it costs; a
        // cross paid the removal's price is a crank nobody runs.
        assert!(priced.cross > priced.removal * 4);
    }

    /// The rate is rounded up and a zero denominator is the whole resource
    /// fee switched off, not a division.
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
        // 3 units at a tenth of a lamport each is a third of a lamport, and a
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
        // The whole account must clear anchor init's 10,240-byte CPI
        // allocation ceiling, or attaching a CLOB to a market breaks.
        assert!(ClobCrankConditionsV0::SIZE <= 10_240);
        // The watch registers at the relay block, which is the first field.
        assert_eq!(CLOB_CRANK_BLOCK_OFFSET, 8);
        // the u64 reservoir field must land 8-aligned, past the block
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

        // Built the way a host builds one — the constructor is what marks
        // a condition active; `set_wake` only rewrites the wake.
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

        // relay's own reader must accept what we wrote, at offset 0 of the
        // region (offset 8 of the account).
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
        // Staging is the shared scratch account's job now, not this
        // account's: the pointer names scratch at index 0.
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

    /// The reservoir is held against the dearest crank it pays, so a market
    /// above its watermark can always afford any one of them. The refill is
    /// excluded even when it is the dearest figure on the account: the
    /// treasury pays that one, and counting it would inflate the float every
    /// market parks.
    #[test]
    fn the_watermark_follows_the_dearest_crank() {
        let payments = CrankPaymentsV0 {
            removal: 5_100,
            cross: 21_000,
            taker_origin_cross: 18_000,
            trigger: 6_000,
            liquidation: 12_000,
            force_cancel: 7_000,
            // Higher than every reservoir-paid crank, and deliberately not
            // counted: the treasury pays the refill, so it must not inflate
            // the float a reservoir is held at.
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
    /// what it requested. Reimbursing the measured figure would hand it the
    /// difference as profit on every liquidation.
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

    /// An honest crank is made whole: the fee it paid is the fee it gets back.
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
