//! Midpoint quoter state. One account holds one instantiation. A maker does
//! not deploy a program. The maker creates a [`MidpointQuoterV0`] PDA and
//! registers it as a Custom quoter in velocity's registry.
//!
//! The quoting model is a spline around a midpoint. The maker keeps a
//! per-side ladder of offsets from mid and sizes. The ladder moves rarely and
//! the mid moves constantly. The hot path is therefore
//! [`MidpointQuoterV0::set_mid`], which writes one stamped u64 at a small
//! compute cost. Quote prices come from `mid ± offset` at quote time, so a mid
//! write reprices nothing.
//!
//! Fills deplete per-level `filled` counters. The ladder is standing intent
//! and not an order book, so a consumed level stays consumed until the maker
//! rewrites the side. The mid-staleness gate is the safety. A maker whose feed
//! dies stops quoting once `mid_slot` falls `max_mid_staleness_slots` behind.
//!
//! The instance holds two separate identities. `authority` is the maker's
//! config key for pause, reconfigure and rotate. `user_authority` is the
//! wallet half of the quoted velocity `User`. Nothing derives one from the
//! other, so a desk can hold the config key cold while the quoted sub-account
//! belongs to a different wallet.
//!
//! `quote_v0` and `execute_v0` write their response in place. They stream
//! wincode into the `response` tail of this account and return a
//! [`ResponsePointerV0`] to it. No intermediate `Vec` is built and no bytes
//! are copied twice.

/// Base units in one whole base asset. `quoter-spec` declares it, and it is
/// fixed rather than per instance. The caller's exact-notional check on a
/// routed fill divides by this constant. An instance on another denominator
/// would price its quotes on one scale and settle on another.
pub use quoter_spec::BASE_PRECISION;
use {
    crate::error::MidpointError,
    anchor_lang::prelude::*,
    bytemuck::Zeroable,
    quoter_spec::{ExecuteWriter, QuoteWriter},
    static_assertions::const_assert_eq,
};

/// Offsets are parts per million of mid. Velocity calls this scale
/// PERCENTAGE_PRECISION.
pub const PERCENTAGE_PRECISION_U64: u64 = 1_000_000;
pub const PERCENTAGE_PRECISION: u128 = PERCENTAGE_PRECISION_U64 as u128;

/// Ladder capacity per side.
pub const MAX_SPLINE_LEVELS: usize = 64;

/// Ceiling on `max_mid_staleness_slots`, about one hour at Solana's roughly
/// 400ms slot time. A live feed re-stamps the mid far more often than that;
/// the ceiling only stops a miswritten config from leaving the staleness gate
/// unbounded.
pub const MAX_MID_STALENESS_SLOTS_CEILING: u64 = 9_000;

/// Response region size. A full-ladder quote response is `4 + 64 × 16` bytes.
/// The execute response is one balance change. Both fit with room to spare.
pub const RESPONSE_BUFFER_BYTES: usize = 2048;

/// Which sides a `cancel_all_v0` withdraws. It is the same wire enum and the
/// same wincode tags the CLOB uses. Named sides replace a pair of bools because
/// the wire must not express "neither".
pub use quoter_spec::CancelSidesV0;
/// Taker direction on the quoter interface. `quoter-spec` declares it once.
pub use quoter_spec::DirectionV0;
pub use quoter_spec::ZERO_ADDRESS;

/// What the wire's named sides mean to a spline. The spline holds no orders,
/// so it reads a side as the flow that consumes its rungs.
pub trait CancelSidesExt {
    fn directions(self) -> &'static [DirectionV0];
}

impl CancelSidesExt for CancelSidesV0 {
    /// The taker directions that consume the named sides. A `Short` taker
    /// hits a bid. A `Long` taker hits an ask.
    fn directions(self) -> &'static [DirectionV0] {
        match self {
            CancelSidesV0::Bids => &[DirectionV0::Short],
            CancelSidesV0::Asks => &[DirectionV0::Long],
            CancelSidesV0::Both => &[DirectionV0::Short, DirectionV0::Long],
        }
    }
}

/// What a `cancel_all_v0` withdrew. It carries rung counts and nothing more.
/// Unlike the CLOB sweep, this one reserves nothing on velocity's side, so no
/// caller has aggregates to unwind. A total of the withdrawn intent would mean
/// reading every rung back, which costs more per rung than the withdrawal.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default, wincode::SchemaRead, wincode::SchemaWrite)]
#[cfg_attr(feature = "idl-build", derive(anchor_lang::IdlType))]
pub struct CancelAllOutcomeV0 {
    /// Rungs withdrawn per side.
    pub bid_rungs: u8,
    pub ask_rungs: u8,
    /// Whether the mid was zeroed too.
    pub mid_cleared: bool,
}

/// Sub-minimum cancelled remainder, present for wire compatibility with the
/// quoter interface. The midpoint never emits one. Spline intent has no orders
/// to cancel, and a level below the minimum is not quoted.
pub use quoter_spec::CancelledRemainderV0;
/// One priced, sized rung on the wire. `quoter-spec` declares it.
pub use quoter_spec::PriceLevelV0;
/// Where in the quoter account the program wrote the wincode response.
/// `quoter-spec` declares it.
pub use quoter_spec::ResponsePointerV0;
/// One user's share of an executed fill. The midpoint always has exactly one,
/// the quoted user, and never completes an order because the ladder holds
/// none. [`MidpointQuoterV0::write_execute_response`] writes the same bytes
/// field by field. A unit test pins the two encodings equal.
pub use quoter_spec::UserBalanceChangeV0;
/// Per-user budgets, also declared by `quoter-spec`. The midpoint does not
/// spend them, because it settles against one standing-intent user and holds
/// no orders to skip. The args carry them because the args are one layout.
pub use quoter_spec::UserCapsV0;
/// A velocity user in its derivable form. Identity is
/// `(authority, sub_account_id)` rather than the `User` account key. The
/// CLOB's `UserRefV0` says why.
pub use quoter_spec::UserRefV0;
/// The caller's settleable-user set. `quoter-spec` declares its capacity, the
/// check a reader owes it, and its widest encoded form. Every program on this
/// wire reads that one declaration. A mirror that drifts by a field decodes
/// the args wrong and reports nothing at all.
pub use quoter_spec::{user_set_within_capacity, USER_SET_CAPACITY, USER_SET_MAX_BYTES};
pub use quoter_spec::{ExecuteResponseV0, QuoteResponseV0};

/// One rung of the spline. It holds standing intent `size` at `mid ± offset`.
/// `filled` counts what executes consumed since the maker wrote the side.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, bytemuck::Pod, bytemuck::Zeroable)]
#[cfg_attr(feature = "idl-build", derive(anchor_lang::IdlType))]
pub struct SplineLevelV0 {
    /// Distance from mid toward this side, in parts per million of mid.
    /// Offsets ascend strictly within a side. Zero quotes at mid itself.
    pub offset_ppm: u64,
    /// Intent size, in the market's base precision.
    pub size: u64,
    /// Base consumed since the maker last wrote the side.
    pub filled: u64,
}

const_assert_eq!(core::mem::size_of::<SplineLevelV0>(), 24);

/// One side's rung array and its live count, borrowed together.
struct SideMut<'a> {
    levels: &'a mut [SplineLevelV0; MAX_SPLINE_LEVELS],
    count: &'a mut u8,
}

/// A maker's spline-level input on the wire. The program owns `filled`, so
/// the setter takes only the shape.
#[derive(Clone, Copy, PartialEq, Eq, Debug, wincode::SchemaRead, wincode::SchemaWrite)]
#[cfg_attr(feature = "idl-build", derive(anchor_lang::IdlType))]
pub struct SplineLevelInputV0 {
    pub offset_ppm: u64,
    pub size: u64,
}

#[account]
pub struct MidpointQuoterV0 {
    /// The maker's config key. It pauses, reconfigures and rotates the hot
    /// key, and it signs creation. Nothing reassigns it, so the maker keeps
    /// the kill switch. It is independent of `user_authority`, so a desk can
    /// hold it cold.
    pub authority: Address,
    /// The key allowed to write the mid and the levels. `authority` rotates
    /// it. The key that signs thousands of mid updates a day carries no other
    /// power.
    pub hot_authority: Address,
    /// The only signer allowed to execute. It is velocity's quoter CPI signer
    /// PDA. Velocity clamps size to the quoted user's margin before it calls
    /// here. The field never changes after init.
    pub execute_authority: Address,
    /// Wallet half of the quoted velocity `User`. Fills settle against
    /// `(user_authority, user_sub_account_id)`. It signs creation and is part
    /// of the PDA seeds, so quoting a different wallet needs a new instance.
    pub user_authority: Address,
    /// Mid price, in the market's price precision. Velocity perps use a
    /// PRICE_PRECISION of 1e6. Zero means the instance does not quote.
    pub mid_price: u64,
    /// Slot of the last mid write. The staleness gate reads it.
    pub mid_slot: u64,
    /// Monotonic guard for racing mid writers. See
    /// [`MidpointQuoterV0::set_mid`].
    pub mid_sequence: u64,
    /// Quotes go empty once `mid_slot` falls this many slots behind.
    pub max_mid_staleness_slots: u64,
    /// Quote prices round to a multiple of this, away from mid. It uses the
    /// same price precision as `mid_price`.
    pub price_tick_size: u64,
    /// Quoted sizes floor to a multiple of this (base precision).
    pub size_step: u64,
    /// The quoter does not quote a level remainder below this. The value is
    /// in base precision. It floors the base size of a rung despite the name.
    pub min_quote_size: u64,
    /// Base units per whole unit. It is always [`BASE_PRECISION`]. See there
    /// for why an instance cannot choose another value.
    pub base_precision: u64,
    /// Sub-account half of the quoted user's identity. `user_authority` is
    /// the wallet half.
    pub user_sub_account_id: u16,
    /// Velocity perp market index this quoter serves.
    pub market_index: u16,
    /// The maker's kill switch on the config path. The registry's `is_active`
    /// is the velocity-side one.
    pub is_paused: u8,
    /// Quote only protected flow. The taker's flow must serve a window before
    /// the call, either the swift hold or the book's activation delay.
    /// Velocity asserts the fact on the wire as `taker_served_window`. This
    /// program trusts its caller for it, as it does for `users` and `caps`.
    pub require_attested_flow: u8,
    pub bid_count: u8,
    pub ask_count: u8,
    /// The largest `|mid − reference_price| / reference_price` the quoter
    /// fills at, in parts per million. `validate` rejects zero, so the bound
    /// can never be disabled. The config key owns the field, so a compromised
    /// hot key cannot move the mid past this band around velocity's oracle.
    pub max_mid_deviation_ppm: u64,
    /// Proposed next `authority`, or [`ZERO_ADDRESS`] when no rotation is
    /// pending. `authority` stays the signer until `accept_authority_v0`
    /// completes the swap, so a mistyped target cannot lock the maker out.
    pub pending_authority: Address,
    /// Room for one more pubkey and a scalar or two. A future field lands
    /// here without moving the ladders or the response tail.
    pub padding: [u8; 32],
    pub bids: [SplineLevelV0; MAX_SPLINE_LEVELS],
    pub asks: [SplineLevelV0; MAX_SPLINE_LEVELS],
    /// The region `quote_v0` and `execute_v0` stream their wincode response
    /// into. Return data carries a [`ResponsePointerV0`] to it.
    pub response: [u8; RESPONSE_BUFFER_BYTES],
}

const_assert_eq!(core::mem::size_of::<MidpointQuoterV0>(), 5392);
// The header, which is everything before the ladders, stays 8-aligned and
// free of holes.
const_assert_eq!(5 * 32 + 8 * 8 + 8 + 8 + 32, 272);

/// Account-data offset of the `response` region.
pub const RESPONSE_OFFSET: usize =
    8 + core::mem::size_of::<MidpointQuoterV0>() - RESPONSE_BUFFER_BYTES;

// Both programs cast the response records onto these bytes, so the region
// must start on the step they are read at. Solana gives account data an
// 8-byte start, and every record's alignment divides 8. This offset is
// therefore the whole condition.
const_assert_eq!(RESPONSE_OFFSET % quoter_spec::LEN_BYTES, 0);

/// Config that init sets and that changes rarely. The addresses ride the
/// accounts list, because a duplicated account costs one index byte in the
/// transaction.
#[derive(Clone, Copy, PartialEq, Eq, Debug, wincode::SchemaRead, wincode::SchemaWrite)]
#[cfg_attr(feature = "idl-build", derive(anchor_lang::IdlType))]
pub struct QuoterConfigV0 {
    pub market_index: u16,
    pub user_sub_account_id: u16,
    pub base_precision: u64,
    pub max_mid_staleness_slots: u64,
    pub price_tick_size: u64,
    pub size_step: u64,
    pub min_quote_size: u64,
    pub require_attested_flow: bool,
    /// The largest `|mid - reference_price| / reference_price` the instance
    /// fills at, in parts per million. It must be nonzero at creation, so a
    /// compromised hot key cannot fill the maker at an off-market mid. The
    /// config key can set it to zero later through `update_quoter_v0`.
    pub max_mid_deviation_ppm: u64,
}

/// The consumed prefix of one side of the spline for a taker of `size`.
/// Execute commits it and quote previews it.
pub struct SplineFill {
    pub base: u64,
    pub quote: u64,
    /// `filled` deltas per level index. Only execute applies them.
    pub consumed: [u64; MAX_SPLINE_LEVELS],
}

/// The scalars a spline walk needs. They are copied out of the account. The
/// walk can then borrow the ladders and the response tail at the same time.
#[derive(Clone, Copy, Debug)]
pub struct SplineParams {
    pub mid_price: u64,
    pub price_tick_size: u64,
    pub size_step: u64,
    pub min_quote_size: u64,
    pub base_precision: u64,
}

impl SplineParams {
    /// A level's price at the current mid, which is `mid ± mid × offset / 1e6`.
    /// It rounds away from mid to the tick, which favors the maker. It returns
    /// `None` if the price leaves u64 or the bid side crosses zero.
    ///
    /// This runs once per rung per quote, so it stays in u64. SBF has no
    /// 128-bit divide, and each `u128 / u128` costs hundreds of compute units.
    /// `mid × offset_ppm` fits u64 for any plausible market, because a 1e8
    /// price times 1e6 ppm is 1e14. A unit test pins the u128 fallback equal.
    pub fn level_price(&self, direction: DirectionV0, offset_ppm: u64) -> Option<u64> {
        let mid = self.mid_price;
        let delta = match mid.checked_mul(offset_ppm) {
            Some(product) => product / PERCENTAGE_PRECISION_U64,
            None => {
                u64::try_from((mid as u128).checked_mul(offset_ppm as u128)? / PERCENTAGE_PRECISION)
                    .ok()?
            }
        };
        // validate() rejects a zero tick, so this never divides by zero.
        let tick = self.price_tick_size;
        let price = match direction {
            // Taker buys: ask above mid, rounded up.
            DirectionV0::Long => mid.checked_add(delta)?.div_ceil(tick).checked_mul(tick)?,
            // Taker sells: bid below mid, rounded down.
            DirectionV0::Short => mid.checked_sub(delta)? / tick * tick,
        };

        (price != 0).then_some(price)
    }

    /// A level's quotable remainder. It is intent minus consumed, floored to
    /// the step. It is zero when it falls below the minimum quote size.
    pub fn level_remaining(&self, level: &SplineLevelV0) -> u64 {
        // validate() rejects a zero step, so this never divides by zero.
        let step = self.size_step;
        let remaining = level.size.saturating_sub(level.filled) / step * step;
        if remaining < self.min_quote_size {
            0
        } else {
            remaining
        }
    }

    /// Quote amount for `base` at `price`, which mirrors the CLOB. It is
    /// `price × base / base_precision`, floored. `carry` is the previous
    /// rung's sub-unit remainder. This function folds it in and returns the
    /// new remainder.
    ///
    /// The carry makes a multi-rung fill total the floor of the whole walk's
    /// notional. That is the one rounding velocity admits when it holds the
    /// fill to the prices this quoter published. See
    /// [`SplineParams::level_price`] for why the math stays in u64 while the
    /// product fits.
    pub fn quote_amount(&self, price: u64, base: u64, carry: u64) -> Result<(u64, u64)> {
        let base_precision = self.base_precision.max(1);
        if let Some(scaled) = price.checked_mul(base).and_then(|p| p.checked_add(carry)) {
            return Ok((scaled / base_precision, scaled % base_precision));
        }

        let scaled = (price as u128)
            .checked_mul(base as u128)
            .ok_or(MidpointError::MathError)?
            .checked_add(carry as u128)
            .ok_or(MidpointError::MathError)?;
        let precision = base_precision as u128;
        Ok((
            (scaled / precision)
                .try_into()
                .map_err(|_| MidpointError::MathError)?,
            (scaled % precision)
                .try_into()
                .map_err(|_| MidpointError::MathError)?,
        ))
    }
}

impl MidpointQuoterV0 {
    pub fn user_ref(&self) -> UserRefV0 {
        UserRefV0 {
            authority: self.user_authority,
            sub_account_id: self.user_sub_account_id,
        }
    }

    pub fn params(&self) -> SplineParams {
        SplineParams {
            mid_price: self.mid_price,
            price_tick_size: self.price_tick_size,
            size_step: self.size_step,
            min_quote_size: self.min_quote_size,
            base_precision: self.base_precision,
        }
    }

    /// Snapshot for [`crate::events::MidpointConfigRecordV0`]. Every
    /// config-mutating instruction emits it, so a reader never has to
    /// special-case which one fired.
    pub fn config_record(&self, ts: i64) -> crate::events::MidpointConfigRecordV0 {
        crate::events::MidpointConfigRecordV0 {
            authority: self.authority,
            hot_authority: self.hot_authority,
            pending_authority: self.pending_authority,
            ts,
            max_mid_staleness_slots: self.max_mid_staleness_slots,
            price_tick_size: self.price_tick_size,
            size_step: self.size_step,
            min_quote_size: self.min_quote_size,
            max_mid_deviation_ppm: self.max_mid_deviation_ppm,
            mid_sequence: self.mid_sequence,
            market_index: self.market_index,
            sub_account_id: self.user_sub_account_id,
            is_paused: self.is_paused,
            require_attested_flow: self.require_attested_flow,
            version: crate::events::MIDPOINT_EVENT_VERSION,
            _pad: [0; 1],
        }
    }

    /// The whole rung array of the side a taker of `direction` consumes,
    /// live prefix and zeroed tail together.
    fn side(&self, direction: DirectionV0) -> &[SplineLevelV0; MAX_SPLINE_LEVELS] {
        match direction {
            DirectionV0::Long => &self.asks,
            DirectionV0::Short => &self.bids,
        }
    }

    /// The same side and its live count, borrowed together so a write can
    /// resize the ladder.
    fn side_mut(&mut self, direction: DirectionV0) -> SideMut<'_> {
        match direction {
            DirectionV0::Long => SideMut {
                levels: &mut self.asks,
                count: &mut self.ask_count,
            },
            DirectionV0::Short => SideMut {
                levels: &mut self.bids,
                count: &mut self.bid_count,
            },
        }
    }

    /// The live rungs of the side a taker of `direction` consumes.
    pub fn side_levels(&self, direction: DirectionV0) -> &[SplineLevelV0] {
        &self.side(direction)[..self.side_count(direction) as usize]
    }

    /// Whether this quoter quotes right now. It checks the pause flag, an
    /// unset mid, and the staleness gate. The callers hold the attestation and
    /// self-trade gates, because they read `taker_served_window` and `taker`
    /// off the wire.
    pub fn is_quoting(&self, slot: u64) -> bool {
        self.is_paused == 0
            && self.mid_price != 0
            && slot.saturating_sub(self.mid_slot) <= self.max_mid_staleness_slots
    }

    /// Whether the current mid sits within the configured band of the caller's
    /// reference price, which is velocity's oracle. A zero bound or a zero mid
    /// disables the check. A zero reference fails it whenever a bound is set.
    pub fn mid_within_deviation(&self, reference_price: u64) -> bool {
        if self.max_mid_deviation_ppm == 0 || self.mid_price == 0 {
            return true;
        }

        let reference = reference_price as u128;
        let diff = (self.mid_price as u128).abs_diff(reference);
        // diff / reference <= ppm / 1e6  ⇔  diff * 1e6 <= ppm * reference.
        diff.saturating_mul(PERCENTAGE_PRECISION)
            <= (self.max_mid_deviation_ppm as u128).saturating_mul(reference)
    }

    /// Walk the consumed prefix for a fill of `size`. The walk mutates
    /// nothing. Execute applies `consumed` after the walk.
    pub fn fill(&self, direction: DirectionV0, size: u64, slot: u64) -> Result<SplineFill> {
        let mut fill = SplineFill {
            base: 0,
            quote: 0,
            consumed: [0; MAX_SPLINE_LEVELS],
        };

        if !self.is_quoting(slot) {
            return Ok(fill);
        }

        let params = self.params();
        let mut wanted = size;
        let mut carry = 0u64;
        for (index, level) in self.side_levels(direction).iter().enumerate() {
            if wanted == 0 {
                break;
            }

            let remaining = params.level_remaining(level);
            if remaining == 0 {
                continue;
            }

            let Some(price) = params.level_price(direction, level.offset_ppm) else {
                continue;
            };
            let take = remaining.min(wanted);
            fill.base = fill
                .base
                .checked_add(take)
                .ok_or(MidpointError::MathError)?;
            let (quote, remainder) = params.quote_amount(price, take, carry)?;
            carry = remainder;
            fill.quote = fill
                .quote
                .checked_add(quote)
                .ok_or(MidpointError::MathError)?;
            fill.consumed[index] = take;
            wanted -= take;
        }

        Ok(fill)
    }

    /// Apply a walked fill's consumption to the side's `filled` counters. It
    /// then asserts the post-state invariants in
    /// [`MidpointQuoterV0::validate_consumption`].
    pub fn apply_fill(&mut self, direction: DirectionV0, fill: &SplineFill) -> Result<()> {
        let count = self.side_count(direction) as usize;
        let side = self.side_mut(direction);

        for (level, consumed) in side.levels[..count].iter_mut().zip(fill.consumed.iter()) {
            level.filled = level.filled.saturating_add(*consumed);
        }

        self.validate_consumption(direction, fill)
    }

    /// Replace one side's ladder. Offsets must ascend strictly, so the rung
    /// closest to mid comes first. Sizes must be nonzero. The write resets
    /// `filled`.
    pub fn write_side(
        &mut self,
        direction: DirectionV0,
        inputs: &[SplineLevelInputV0],
    ) -> Result<()> {
        require!(
            inputs.len() <= MAX_SPLINE_LEVELS,
            MidpointError::TooManyLevels
        );

        let mut previous: Option<u64> = None;
        for input in inputs {
            require!(input.size > 0, MidpointError::InvalidLevel);
            require!(
                previous.is_none_or(|p| input.offset_ppm > p),
                MidpointError::LevelsNotAscending
            );

            previous = Some(input.offset_ppm);
        }

        let side = self.side_mut(direction);
        for (slot, input) in side.levels.iter_mut().zip(inputs.iter()) {
            *slot = SplineLevelV0 {
                offset_ppm: input.offset_ppm,
                size: input.size,
                filled: 0,
            };
        }

        for slot in side.levels.iter_mut().skip(inputs.len()) {
            *slot = SplineLevelV0::zeroed();
        }

        *side.count = inputs.len() as u8;
        Ok(())
    }

    /// Withdraw one side's standing intent. It zeroes the live rungs, drops
    /// the count to zero, and returns the number of rungs it cleared. The tail
    /// past `count` is already zero, so a maker who runs eight rungs pays for
    /// eight rather than for the ladder's capacity.
    pub fn clear_side(&mut self, direction: DirectionV0) -> u8 {
        let count = self.side_count(direction) as usize;
        let side = self.side_mut(direction);
        side.levels[..count].fill(SplineLevelV0::zeroed());
        *side.count = 0;
        count as u8
    }

    /// Post-condition of [`Self::clear_side`]. The side's count is zero and
    /// every rung the withdrawal wrote is zeroed. The check covers the
    /// `cleared` rungs alone, because the tail past the old count was already
    /// zero. Every other mutating instruction runs the full
    /// [`Self::validate`], which catches a tail that anything else corrupted.
    pub fn validate_cleared_side(&self, direction: DirectionV0, cleared: u8) -> Result<()> {
        require!(
            self.side_count(direction) == 0,
            MidpointError::InvariantViolated
        );

        require!(
            self.side(direction)[..(cleared as usize).min(MAX_SPLINE_LEVELS)]
                .iter()
                .all(|level| *level == SplineLevelV0::zeroed()),
            MidpointError::InvariantViolated
        );

        Ok(())
    }

    /// Stamp a new mid. `sequence` is an optional monotonic guard for racing
    /// writers. A nonzero sequence must increase strictly, so a delayed relay
    /// cannot overwrite a fresher mid. A zero sequence skips the check.
    ///
    /// A mid of zero is a withdrawal. It routes to [`Self::clear_mid`] and
    /// never fails on the guard.
    ///
    /// The post-write assertion is O(1). This is the compute-pinned hot path,
    /// so it checks only what it wrote and never the ladders.
    pub fn set_mid(&mut self, mid: u64, sequence: u64, slot: u64) -> Result<()> {
        if mid == 0 {
            return self.clear_mid();
        }

        // Once a writer uses sequences, every later write must carry a higher
        // one. A 0 after a real sequence would let a set_mid replayed through a
        // durable nonce re-stamp a stale mid as fresh. A writer that never
        // sequences keeps opting out with 0.
        if sequence != 0 || self.mid_sequence != 0 {
            require!(
                sequence > self.mid_sequence,
                MidpointError::StaleMidSequence
            );

            self.mid_sequence = sequence;
        }

        self.mid_price = mid;
        self.mid_slot = slot;
        require!(
            self.mid_slot == slot && (sequence == 0 || self.mid_sequence == sequence),
            MidpointError::InvariantViolated
        );

        Ok(())
    }

    /// Withdraw the mid. The instance stops quoting at once, because
    /// [`Self::is_quoting`] refuses a zero mid on every side. This write skips
    /// the monotonic guard and leaves `mid_sequence` and `mid_slot` alone. A
    /// zero mid publishes no price, and a withdrawal must never lose a race.
    pub fn clear_mid(&mut self) -> Result<()> {
        self.mid_price = 0;
        require!(self.mid_price == 0, MidpointError::InvariantViolated);
        Ok(())
    }

    fn side_count(&self, direction: DirectionV0) -> u8 {
        match direction {
            DirectionV0::Long => self.ask_count,
            DirectionV0::Short => self.bid_count,
        }
    }

    /// Invariants every non-hot mutating instruction leaves true. The check is
    /// a bounded scan of both ladders, which config and shape writes can
    /// afford. `set_mid_v0` does not call it, because that path is
    /// compute-pinned and cannot touch a rung.
    pub fn validate(&self) -> Result<()> {
        require!(
            self.base_precision == BASE_PRECISION,
            MidpointError::InvalidConfig
        );
        require!(
            self.is_paused <= 1 && self.require_attested_flow <= 1,
            MidpointError::InvariantViolated
        );
        require!(
            self.user_authority != ZERO_ADDRESS
                && self.authority != ZERO_ADDRESS
                && self.hot_authority != ZERO_ADDRESS
                && self.execute_authority != ZERO_ADDRESS,
            MidpointError::InvalidConfig
        );
        require!(
            self.price_tick_size != 0 && self.size_step != 0,
            MidpointError::InvalidConfig
        );
        require!(
            self.max_mid_staleness_slots != 0
                && self.max_mid_staleness_slots <= MAX_MID_STALENESS_SLOTS_CEILING,
            MidpointError::InvalidConfig
        );
        // Nonzero at creation and on every later update, so a compromised hot
        // key can never fill the maker at an off-market mid.
        require!(
            self.max_mid_deviation_ppm != 0,
            MidpointError::InvalidConfig
        );

        Self::validate_side(&self.bids, self.bid_count)?;
        Self::validate_side(&self.asks, self.ask_count)
    }

    /// One side is consistent. It holds `count` live rungs with strictly
    /// ascending offsets, nonzero sizes, and `filled <= size`. The tail past
    /// the count is zero, so a shrinking rewrite cannot leave a stale rung
    /// quotable.
    fn validate_side(levels: &[SplineLevelV0; MAX_SPLINE_LEVELS], count: u8) -> Result<()> {
        let count = count as usize;
        require!(count <= MAX_SPLINE_LEVELS, MidpointError::TooManyLevels);
        let mut previous: Option<u64> = None;
        for level in &levels[..count] {
            require!(level.size > 0, MidpointError::InvalidLevel);
            require!(level.filled <= level.size, MidpointError::InvariantViolated);
            require!(
                previous.is_none_or(|p| level.offset_ppm > p),
                MidpointError::LevelsNotAscending
            );

            previous = Some(level.offset_ppm);
        }
        for level in &levels[count..] {
            require!(
                *level == SplineLevelV0::zeroed(),
                MidpointError::InvariantViolated
            );
        }

        Ok(())
    }

    /// Post-execute invariants for the side the walk consumed.
    ///
    /// - Consumption runs best first. The walk touches a rung only once every
    ///   better rung is exhausted, so at most the last touched rung keeps a
    ///   quotable remainder.
    /// - The walk touches no rung outside the live count.
    /// - `filled <= size` on every rung, and the per-rung deltas sum to the
    ///   `base` the response reports.
    fn validate_consumption(&self, direction: DirectionV0, fill: &SplineFill) -> Result<()> {
        let params = self.params();
        let count = self.side_count(direction) as usize;
        require!(
            fill.consumed[count..].iter().all(|taken| *taken == 0),
            MidpointError::InvariantViolated
        );

        let mut total: u64 = 0;
        let mut walk_ended = false;
        for (level, taken) in self.side_levels(direction).iter().zip(fill.consumed.iter()) {
            require!(level.filled <= level.size, MidpointError::InvariantViolated);
            if *taken == 0 {
                continue;
            }

            require!(!walk_ended, MidpointError::InvariantViolated);
            // A take that leaves the rung quotable means the taker ran out of
            // size here. Nothing beyond this rung may be consumed.
            walk_ended = params.level_remaining(level) > 0;
            total = total.checked_add(*taken).ok_or(MidpointError::MathError)?;
        }

        require!(total == fill.base, MidpointError::InvariantViolated);
        Ok(())
    }

    /// Stream a `QuoteResponseV0` for a taker of `direction` and `size` into
    /// the response tail. Levels go out best first and stop once the taker's
    /// size is covered. `open` is the caller's gate for settleability,
    /// self-trade and attestation. A closed gate or a non-quoting spline
    /// writes an empty level vec, which the wire reads as no liquidity.
    pub fn write_quote_response(
        &mut self,
        direction: DirectionV0,
        size: u64,
        limit_price: u64,
        slot: u64,
        open: bool,
    ) -> Result<ResponsePointerV0> {
        let params = self.params();
        let live = open && self.is_quoting(slot);
        let count = if live { self.side_count(direction) } else { 0 } as usize;
        // Disjoint field borrows. The walk reads a ladder while the writer
        // holds the response tail. Both are views into the same account.
        let Self {
            bids,
            asks,
            response,
            ..
        } = self;
        let side: &[SplineLevelV0] = match direction {
            DirectionV0::Long => &asks[..count],
            DirectionV0::Short => &bids[..count],
        };

        let mut writer = QuoteWriter::new();
        let mut wanted = size;
        for level in side {
            if wanted == 0 {
                break;
            }

            let remaining = params.level_remaining(level);
            if remaining == 0 {
                continue;
            }

            let Some(price) = params.level_price(direction, level.offset_ppm) else {
                continue;
            };

            // Past the caller's worst acceptable price. Offsets ascend
            // strictly, so every later rung prices further from the mid and is
            // worse.
            if direction.worse_than_limit(price, limit_price) {
                break;
            }

            let quoted = remaining.min(wanted);
            writer
                .push_level(
                    &mut response[..],
                    PriceLevelV0 {
                        price,
                        size: quoted,
                    },
                )
                .map_err(MidpointError::from)?;
            wanted -= quoted;
        }

        // `finish` backfills the ladder's count and writes the withheld
        // report behind it. The report is always empty here. The midpoint
        // settles against one standing-intent user and holds no resting
        // orders, so it withholds no liquidity for want of an account.
        let len = writer
            .finish(&mut response[..], PriceLevelV0::default())
            .map_err(MidpointError::from)?;
        response_pointer(len)
    }

    /// Stream an `ExecuteResponseV0` into the response tail. It carries at
    /// most one balance change, for the quoted user. It never carries a
    /// completed order id or a cancelled remainder.
    pub fn write_execute_response(
        &mut self,
        change: Option<(u64, u64)>,
    ) -> Result<ResponsePointerV0> {
        let user = self.user_ref();
        let mut writer = ExecuteWriter::new();
        if let Some((base_size, quote_size)) = change {
            writer
                .push_change(
                    &mut self.response[..],
                    UserBalanceChangeV0 {
                        base_size,
                        quote_size,
                        user,
                        _pad: [0; 6],
                    },
                )
                .map_err(MidpointError::from)?;
        }

        // Standing intent has no resting orders, so there is never a
        // cancelled remainder, a completed order, or a partial fill.
        let len = writer
            .finish(&mut self.response[..], &[], &[], &[])
            .map_err(MidpointError::from)?;
        response_pointer(len)
    }
}

/// Point at the `len` bytes the writer just streamed. The response region
/// always starts at [`RESPONSE_OFFSET`].
fn response_pointer(len: usize) -> Result<ResponsePointerV0> {
    Ok(ResponsePointerV0::at(RESPONSE_OFFSET, len).map_err(MidpointError::from)?)
}

#[cfg(test)]
mod tests {
    use {super::*, bytemuck::Zeroable};

    const MID: u64 = 100_000_000;
    const UNIT: u64 = 1_000_000_000;

    fn quoter(bids: &[(u64, u64)], asks: &[(u64, u64)]) -> MidpointQuoterV0 {
        let mut quoter = MidpointQuoterV0::zeroed();
        quoter.authority = Address::new_from_array([1u8; 32]);
        quoter.hot_authority = Address::new_from_array([2u8; 32]);
        quoter.execute_authority = Address::new_from_array([3u8; 32]);
        quoter.user_authority = Address::new_from_array([4u8; 32]);
        quoter.base_precision = UNIT;
        quoter.price_tick_size = 100;
        quoter.size_step = 1_000;
        quoter.min_quote_size = 10_000;
        quoter.max_mid_staleness_slots = 25;
        // Widest legal band. validate() rejects zero, and most tests here
        // are not exercising the deviation gate.
        quoter.max_mid_deviation_ppm = u64::MAX;
        quoter.mid_price = MID;
        let inputs = |side: &[(u64, u64)]| {
            side.iter()
                .map(|(offset_ppm, size)| SplineLevelInputV0 {
                    offset_ppm: *offset_ppm,
                    size: *size,
                })
                .collect::<Vec<_>>()
        };

        quoter
            .write_side(DirectionV0::Short, &inputs(bids))
            .expect("bids");
        quoter
            .write_side(DirectionV0::Long, &inputs(asks))
            .expect("asks");
        quoter.validate().expect("armed quoter is valid");
        quoter
    }

    #[test]
    fn mid_deviation_bound_gates_an_off_market_mid() {
        let mut q = quoter(&[(1_000, UNIT)], &[(1_000, UNIT)]);
        // The helper's band is as wide as it can be, so any real mid passes.
        assert!(q.mid_within_deviation(MID));
        assert!(q.mid_within_deviation(MID / 1_000));

        // A one percent band around the oracle reference.
        q.max_mid_deviation_ppm = 10_000;
        assert!(q.mid_within_deviation(MID)); // exact
        assert!(q.mid_within_deviation(MID + MID / 200)); // ~0.5% off, inside
        assert!(!q.mid_within_deviation(MID + MID / 50)); // ~2% off, outside
        assert!(!q.mid_within_deviation(MID / 2)); // far below, outside

        // A zero reference is a price, not an absent one, so the band fails.
        assert!(!q.mid_within_deviation(0));
    }

    /// A `Vec`-building encoder, kept as the reference for the streaming
    /// writer.
    fn reference_quote(quoter: &MidpointQuoterV0, direction: DirectionV0, size: u64) -> Vec<u8> {
        let params = quoter.params();
        let mut levels = Vec::new();
        let mut wanted = size;
        for level in quoter.side_levels(direction) {
            if wanted == 0 {
                break;
            }

            let remaining = params.level_remaining(level);
            if remaining == 0 {
                continue;
            }

            let Some(price) = params.level_price(direction, level.offset_ppm) else {
                continue;
            };
            let quoted = remaining.min(wanted);
            levels.push(PriceLevelV0 {
                price,
                size: quoted,
            });

            wanted -= quoted;
        }

        wincode::serialize(&QuoteResponseV0 {
            levels: &levels,
            withheld: PriceLevelV0::default(),
        })
        .unwrap()
    }

    fn reference_execute(quoter: &MidpointQuoterV0, change: Option<(u64, u64)>) -> Vec<u8> {
        let change = change.map(|(base_size, quote_size)| UserBalanceChangeV0 {
            base_size,
            quote_size,
            user: quoter.user_ref(),
            _pad: [0; 6],
        });

        wincode::serialize(&ExecuteResponseV0 {
            changes: change.as_slice(),
            cancelled: &[],
            completed: &[],
            partial: &[],
        })
        .unwrap()
    }

    /// What the writer put in the account must read back as the response it
    /// meant to send. Velocity runs this same parse, so a framing mistake here
    /// is a fill that cannot be decoded rather than one that settles wrong.
    #[test]
    fn the_written_response_parses_back() {
        let mut quoter = quoter(&[], &[]);
        let pointer = quoter
            .write_execute_response(Some((1_000_000_000, 101_000_000)))
            .unwrap();
        let bytes = written(&quoter, pointer);
        let response = ExecuteResponseV0::parse(&bytes).unwrap();

        assert_eq!(response.changes.len(), 1);
        assert_eq!(response.changes[0].base_size, 1_000_000_000);
        assert_eq!(response.changes[0].quote_size, 101_000_000);
        assert_eq!(response.changes[0].user, quoter.user_ref());
        // Standing intent has no resting orders to remove or consume.
        assert!(response.cancelled.is_empty());
        assert!(response.completed.is_empty());
    }

    fn written(quoter: &MidpointQuoterV0, pointer: ResponsePointerV0) -> Vec<u8> {
        assert_eq!(pointer.offset as usize, RESPONSE_OFFSET);
        quoter.response[..pointer.len as usize].to_vec()
    }

    /// The in-place writer must be byte-identical to serializing the wire
    /// struct. The response is a public wire and not an internal detail.
    #[test]
    fn streamed_quote_response_matches_the_wire_struct() {
        for size in [0, UNIT / 4, UNIT, 3 * UNIT, u64::MAX] {
            for direction in [DirectionV0::Long, DirectionV0::Short] {
                let mut quoter = quoter(
                    &[(1_000, UNIT), (3_000, UNIT / 2)],
                    &[(500, UNIT / 2), (2_500, UNIT), (9_000, 5 * UNIT)],
                );
                let expected = reference_quote(&quoter, direction, size);
                let pointer = quoter
                    .write_quote_response(direction, size, 0, 0, true)
                    .unwrap();
                assert_eq!(written(&quoter, pointer), expected);
            }
        }
    }

    /// An empty ladder is its length prefix and the withheld report behind
    /// it. The report is always empty here, because the midpoint holds no
    /// resting orders and so withholds no liquidity.
    const EMPTY_QUOTE: usize = quoter_spec::LEN_BYTES + 2 * 8;

    #[test]
    fn a_closed_gate_streams_an_empty_level_vec() {
        let mut quoter = quoter(&[(1_000, UNIT)], &[(1_000, UNIT)]);
        let closed = quoter
            .write_quote_response(DirectionV0::Long, UNIT, 0, 0, false)
            .unwrap();
        assert_eq!(written(&quoter, closed), vec![0u8; EMPTY_QUOTE]);
        // A stale mid quotes nothing, even with the gate open.
        let stale = quoter
            .write_quote_response(DirectionV0::Long, UNIT, 0, 10_000, true)
            .unwrap();
        assert_eq!(written(&quoter, stale), vec![0u8; EMPTY_QUOTE]);
    }

    #[test]
    fn streamed_execute_response_matches_the_wire_struct() {
        for change in [None, Some((0, 0)), Some((UNIT, 100_100_000))] {
            let mut quoter = quoter(&[(1_000, UNIT)], &[(1_000, UNIT)]);
            let expected = reference_execute(&quoter, change);
            let pointer = quoter.write_execute_response(change).unwrap();
            assert_eq!(written(&quoter, pointer), expected);
        }
    }

    #[test]
    fn quote_truncates_at_the_takers_size_and_rounds_away_from_mid() {
        let mut quoter = quoter(&[(1_000, UNIT)], &[(1_000, UNIT), (3_000, UNIT)]);
        let pointer = quoter
            .write_quote_response(DirectionV0::Long, UNIT + UNIT / 2, 0, 0, true)
            .unwrap();
        let bytes = written(&quoter, pointer);
        let response = QuoteResponseV0::parse(&bytes).unwrap();

        // The taker's size runs out inside the second rung. That rung is
        // quoted for the remainder rather than for its full standing size.
        assert_eq!(response.levels.len(), 2);
        assert_eq!(response.levels[0].price, 100_100_000);
        assert_eq!(response.levels[0].size, UNIT);
        assert_eq!(response.levels[1].price, 100_300_000);
        assert_eq!(response.levels[1].size, UNIT / 2);
    }

    #[test]
    fn the_price_bound_stops_the_ladder_at_the_limit() {
        // Rungs at +0.1% and +0.3% of a 100.0 mid are 100.1 and 100.3.
        let mut quoter = quoter(&[(1_000, UNIT)], &[(1_000, UNIT), (3_000, UNIT)]);

        // A long taker paying no more than 100.2 gets the first rung only.
        let pointer = quoter
            .write_quote_response(DirectionV0::Long, u64::MAX, 100_200_000, 0, true)
            .unwrap();
        let bytes = written(&quoter, pointer);
        let response = QuoteResponseV0::parse(&bytes).unwrap();
        assert_eq!(response.levels.len(), 1);
        assert_eq!(response.levels[0].price, 100_100_000);

        // The rung exactly at the limit is acceptable.
        let pointer = quoter
            .write_quote_response(DirectionV0::Long, u64::MAX, 100_300_000, 0, true)
            .unwrap();
        let bytes = written(&quoter, pointer);
        assert_eq!(QuoteResponseV0::parse(&bytes).unwrap().levels.len(), 2);

        // Zero is no bound.
        let pointer = quoter
            .write_quote_response(DirectionV0::Long, u64::MAX, 0, 0, true)
            .unwrap();
        let bytes = written(&quoter, pointer);
        assert_eq!(QuoteResponseV0::parse(&bytes).unwrap().levels.len(), 2);

        // A short taker sells the bid ladder, so the bound cuts the low
        // side.
        let mut bid_side = super::tests::quoter(&[(1_000, UNIT), (3_000, UNIT)], &[(1_000, UNIT)]);
        let pointer = bid_side
            .write_quote_response(DirectionV0::Short, u64::MAX, 99_800_000, 0, true)
            .unwrap();
        let bytes = written(&bid_side, pointer);
        let response = QuoteResponseV0::parse(&bytes).unwrap();
        assert_eq!(response.levels.len(), 1);
        assert_eq!(response.levels[0].price, 99_900_000);
    }

    #[test]
    fn a_full_ladder_response_fits_the_buffer() {
        let side: Vec<(u64, u64)> = (0..MAX_SPLINE_LEVELS as u64)
            .map(|i| (1_000 + i, UNIT))
            .collect();
        let mut quoter = quoter(&side, &side);
        let pointer = quoter
            .write_quote_response(DirectionV0::Long, u64::MAX, 0, 0, true)
            .unwrap();
        assert_eq!(
            pointer.len as usize,
            quoter_spec::LEN_BYTES + MAX_SPLINE_LEVELS * quoter_spec::PRICE_LEVEL_BYTES + 2 * 8
        );

        assert!(pointer.len as usize <= RESPONSE_BUFFER_BYTES);
    }

    #[test]
    fn execute_consumes_a_monotone_prefix() {
        let mut quoter = quoter(&[], &[(1_000, UNIT), (3_000, UNIT)]);
        let fill = quoter.fill(DirectionV0::Long, UNIT + UNIT / 2, 0).unwrap();
        quoter.apply_fill(DirectionV0::Long, &fill).unwrap();
        assert_eq!(fill.base, UNIT + UNIT / 2);
        assert_eq!(quoter.asks[0].filled, UNIT);
        assert_eq!(quoter.asks[1].filled, UNIT / 2);
        quoter.validate().unwrap();

        // The partially consumed rung is still quotable, so a second fill
        // starts there and the invariant still holds.
        let fill = quoter.fill(DirectionV0::Long, UNIT, 0).unwrap();
        quoter.apply_fill(DirectionV0::Long, &fill).unwrap();
        assert_eq!(fill.base, UNIT / 2);
        assert_eq!(quoter.asks[1].filled, UNIT);
        quoter.validate().unwrap();
    }

    #[test]
    fn consumption_beyond_a_partially_taken_rung_is_rejected() {
        let mut quoter = quoter(&[], &[(1_000, UNIT), (3_000, UNIT)]);
        // A walk the fill path cannot produce. It takes half of rung 0, which
        // leaves rung 0 quotable, and half of rung 1.
        let mut consumed = [0u64; MAX_SPLINE_LEVELS];
        consumed[0] = UNIT / 2;
        consumed[1] = UNIT / 2;
        let fill = SplineFill {
            base: UNIT,
            quote: 0,
            consumed,
        };

        assert!(quoter.apply_fill(DirectionV0::Long, &fill).is_err());
    }

    #[test]
    fn consumption_of_a_dead_rung_is_rejected() {
        let mut quoter = quoter(&[], &[(1_000, UNIT)]);
        let mut consumed = [0u64; MAX_SPLINE_LEVELS];
        // Rung 1 is past `ask_count`, so nothing may touch it.
        consumed[1] = UNIT;
        let fill = SplineFill {
            base: UNIT,
            quote: 0,
            consumed,
        };

        assert!(quoter.apply_fill(DirectionV0::Long, &fill).is_err());
    }

    #[test]
    fn a_mismatched_base_total_is_rejected() {
        let mut quoter = quoter(&[], &[(1_000, UNIT)]);
        let mut consumed = [0u64; MAX_SPLINE_LEVELS];
        consumed[0] = UNIT;
        let fill = SplineFill {
            base: UNIT / 2,
            quote: 0,
            consumed,
        };

        assert!(quoter.apply_fill(DirectionV0::Long, &fill).is_err());
    }

    #[test]
    fn clear_side_withdraws_one_side_and_leaves_the_other() {
        let mut quoter = quoter(&[(1_000, UNIT), (3_000, UNIT / 2)], &[(500, 2 * UNIT)]);
        // A partly consumed rung is withdrawn like any other, `filled`
        // included.
        let fill = quoter.fill(DirectionV0::Short, UNIT / 4, 0).unwrap();
        quoter.apply_fill(DirectionV0::Short, &fill).unwrap();

        assert_eq!(quoter.clear_side(DirectionV0::Short), 2);
        assert_eq!(quoter.bid_count, 0);
        assert!(quoter
            .bids
            .iter()
            .all(|level| *level == SplineLevelV0::zeroed()));
        // The ask side is untouched and still quotes.
        assert_eq!(quoter.ask_count, 1);
        assert_eq!(quoter.asks[0].size, 2 * UNIT);
        quoter.validate().unwrap();
        assert!(quoter
            .write_quote_response(DirectionV0::Short, UNIT, 0, 0, true)
            .is_ok());
        assert!(quoter.fill(DirectionV0::Short, UNIT, 0).unwrap().base == 0);
        assert_eq!(quoter.fill(DirectionV0::Long, UNIT, 0).unwrap().base, UNIT);
    }

    /// The mid withdrawal is the maker's kill switch, so it must work on an
    /// instance that already runs sequences. It also consumes no sequence, so
    /// a later real mid must still beat the last real one.
    #[test]
    fn a_mid_withdrawal_ignores_the_sequence_guard() {
        let mut quoter = quoter(&[(1_000, UNIT)], &[(1_000, UNIT)]);
        quoter.set_mid(MID, 5, 10).unwrap();
        assert_eq!((quoter.mid_sequence, quoter.mid_slot), (5, 10));

        // The withdrawal succeeds although 0 does not beat 5.
        quoter.clear_mid().unwrap();
        assert_eq!(quoter.mid_price, 0);
        assert!(!quoter.is_quoting(10));
        // The sequence and the slot are the last real write's.
        assert_eq!((quoter.mid_sequence, quoter.mid_slot), (5, 10));
        // Routed through set_mid it is the same withdrawal.
        quoter.set_mid(0, 0, 99).unwrap();
        assert_eq!((quoter.mid_sequence, quoter.mid_slot), (5, 10));

        // A later real mid still has to beat 5.
        assert!(quoter.set_mid(MID, 5, 11).is_err());
        assert!(quoter.set_mid(MID, 4, 11).is_err());
        assert!(quoter.set_mid(MID, 0, 11).is_err());
        quoter.set_mid(MID, 6, 11).unwrap();
        assert_eq!((quoter.mid_price, quoter.mid_sequence), (MID, 6));
        assert!(quoter.is_quoting(11));
    }

    /// Clearing an empty side changes nothing and still leaves a valid
    /// ladder. The instruction is therefore idempotent. A maker can send it
    /// twice without a failed transaction that reports nothing was wrong.
    #[test]
    fn clearing_an_empty_side_is_a_valid_no_op() {
        let mut quoter = quoter(&[], &[(500, UNIT)]);
        assert_eq!(quoter.clear_side(DirectionV0::Short), 0);
        quoter.validate().unwrap();
        assert_eq!(quoter.clear_side(DirectionV0::Long), 1);
        assert_eq!(quoter.clear_side(DirectionV0::Long), 0);
        quoter.validate().unwrap();
    }

    /// The withdrawal post-check covers the rungs the withdrawal wrote and a
    /// count that failed to drop. It does not cover the tail beyond them,
    /// which keeps its cost proportional to the shape being withdrawn. The
    /// full [`MidpointQuoterV0::validate`] that every other mutating
    /// instruction runs covers the tail.
    #[test]
    fn the_withdrawal_post_check_covers_what_the_withdrawal_wrote() {
        let mut quoter = quoter(&[(1_000, UNIT), (3_000, UNIT)], &[(500, UNIT)]);
        let cleared = quoter.clear_side(DirectionV0::Short);
        quoter
            .validate_cleared_side(DirectionV0::Short, cleared)
            .unwrap();

        // A rung the withdrawal should have zeroed and did not.
        quoter.bids[1].size = UNIT;
        assert!(quoter
            .validate_cleared_side(DirectionV0::Short, cleared)
            .is_err());
        quoter.bids[1].size = 0;

        // A count that failed to drop, which would leave the side quotable.
        quoter.bid_count = 1;
        assert!(quoter
            .validate_cleared_side(DirectionV0::Short, cleared)
            .is_err());
        quoter.bid_count = 0;

        // The check covers only the side it was asked about. The ask side is
        // still live and that is not an error.
        quoter
            .validate_cleared_side(DirectionV0::Short, cleared)
            .unwrap();
        assert_eq!(quoter.ask_count, 1);

        // Stale tail rungs are out of scope here. `validate` catches them.
        quoter.bids[7].size = UNIT;
        quoter
            .validate_cleared_side(DirectionV0::Short, cleared)
            .unwrap();
        assert!(quoter.validate().is_err());
    }

    #[test]
    fn validate_catches_a_hand_corrupted_ladder() {
        let mut over_filled = quoter(&[], &[(1_000, UNIT)]);
        over_filled.asks[0].filled = UNIT + 1;
        assert!(over_filled.validate().is_err());

        let mut stale_tail = quoter(&[], &[(1_000, UNIT)]);
        stale_tail.asks[1].size = UNIT;
        assert!(stale_tail.validate().is_err());

        let mut descending = quoter(&[], &[(1_000, UNIT), (3_000, UNIT)]);
        descending.asks[0].offset_ppm = 4_000;
        assert!(descending.validate().is_err());

        let mut lying_count = quoter(&[], &[(1_000, UNIT)]);
        lying_count.ask_count = 2;
        assert!(lying_count.validate().is_err());

        let mut no_precision = quoter(&[], &[(1_000, UNIT)]);
        no_precision.base_precision = 0;
        assert!(no_precision.validate().is_err());
    }

    #[test]
    fn validate_bounds_the_config_scalars() {
        let mut zero_staleness = quoter(&[], &[(1_000, UNIT)]);
        zero_staleness.max_mid_staleness_slots = 0;
        assert!(zero_staleness.validate().is_err());

        let mut unbounded_staleness = quoter(&[], &[(1_000, UNIT)]);
        unbounded_staleness.max_mid_staleness_slots = u64::MAX;
        assert!(unbounded_staleness.validate().is_err());
        unbounded_staleness.max_mid_staleness_slots = MAX_MID_STALENESS_SLOTS_CEILING;
        assert!(unbounded_staleness.validate().is_ok());

        let mut zero_tick = quoter(&[], &[(1_000, UNIT)]);
        zero_tick.price_tick_size = 0;
        assert!(zero_tick.validate().is_err());

        let mut zero_step = quoter(&[], &[(1_000, UNIT)]);
        zero_step.size_step = 0;
        assert!(zero_step.validate().is_err());

        let mut zero_deviation = quoter(&[], &[(1_000, UNIT)]);
        zero_deviation.max_mid_deviation_ppm = 0;
        assert!(zero_deviation.validate().is_err());
    }

    /// The all-u128 form of `level_price`, kept as the reference for the u64
    /// fast path the program runs.
    fn reference_level_price(
        params: &SplineParams,
        direction: DirectionV0,
        offset_ppm: u64,
    ) -> Option<u64> {
        let mid = params.mid_price as u128;
        let delta = mid
            .checked_mul(offset_ppm as u128)?
            .checked_div(PERCENTAGE_PRECISION)?;
        let tick = (params.price_tick_size as u128).max(1);
        let price = match direction {
            DirectionV0::Long => mid.checked_add(delta)?.div_ceil(tick).checked_mul(tick)?,
            DirectionV0::Short => {
                let raw = mid.checked_sub(delta)?;
                raw / tick * tick
            }
        };

        if price == 0 {
            return None;
        }

        u64::try_from(price).ok()
    }

    fn reference_quote_amount(
        params: &SplineParams,
        price: u64,
        base: u64,
        carry: u64,
    ) -> Option<(u64, u64)> {
        let precision = params.base_precision.max(1) as u128;
        let scaled = (price as u128)
            .checked_mul(base as u128)?
            .checked_add(carry as u128)?;
        Some((
            (scaled / precision).try_into().ok()?,
            (scaled % precision).try_into().ok()?,
        ))
    }

    /// The u64 fast paths must match the u128 math exactly, including where
    /// they give up. A rounding difference here is a mispriced fill.
    #[test]
    fn the_u64_price_math_matches_the_u128_form() {
        let mids = [0, 1, 99, 100_000_000, u64::MAX / 2, u64::MAX - 1, u64::MAX];
        let offsets = [0, 1, 999, 1_000_000, u64::MAX / 3, u64::MAX];
        // A live account can never hold a zero tick; validate() rejects it.
        let ticks = [1, 7, 100, 1_000_000, u64::MAX];
        for mid in mids {
            for tick in ticks {
                let params = SplineParams {
                    mid_price: mid,
                    price_tick_size: tick,
                    size_step: 1,
                    min_quote_size: 0,
                    base_precision: UNIT,
                };

                for offset_ppm in offsets {
                    for direction in [DirectionV0::Long, DirectionV0::Short] {
                        assert_eq!(
                            params.level_price(direction, offset_ppm),
                            reference_level_price(&params, direction, offset_ppm),
                            "mid {mid} tick {tick} offset {offset_ppm} {direction:?}"
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn the_u64_quote_math_matches_the_u128_form() {
        let params = SplineParams {
            mid_price: MID,
            price_tick_size: 100,
            size_step: 1,
            min_quote_size: 0,
            base_precision: UNIT,
        };

        for price in [0, 1, MID, u64::MAX / 3, u64::MAX] {
            for base in [0, 1, UNIT, 12_345_678_901, u64::MAX / 7, u64::MAX] {
                for carry in [0, 1, UNIT - 1] {
                    assert_eq!(
                        params.quote_amount(price, base, carry).ok(),
                        reference_quote_amount(&params, price, base, carry),
                        "price {price} base {base} carry {carry}"
                    );
                }
            }
        }
    }

    #[test]
    fn a_nonzero_mid_sequence_must_strictly_increase() {
        let mut quoter = quoter(&[], &[]);
        quoter.set_mid(MID, 5, 10).unwrap();
        assert!(quoter.set_mid(MID + 1, 5, 11).is_err());
        assert!(quoter.set_mid(MID + 1, 4, 11).is_err());
        quoter.set_mid(MID + 1, 6, 11).unwrap();
        // A writer that used a sequence cannot fall back to zero. That would
        // let a replay re-stamp a stale mid as fresh.
        assert!(quoter.set_mid(MID + 2, 0, 12).is_err());
        assert_eq!(quoter.mid_price, MID + 1);
        assert_eq!(quoter.mid_sequence, 6);
        assert_eq!(quoter.mid_slot, 11);
    }
}
