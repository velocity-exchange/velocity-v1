//! Midpoint quoter state: one account per instantiation. A maker doesn't
//! deploy a program — they create a [`MidpointQuoterV0`] PDA of this one and
//! register it as a Custom quoter in velocity's registry.
//!
//! The quoting model is a *spline around a midpoint*: the maker maintains a
//! shape — per-side ladders of `(offset from mid, size)` — that moves rarely,
//! and a mid price that moves constantly. The hot path is therefore
//! [`MidpointQuoterV0::set_mid`]: a single stamped u64 write, kept as close to
//! free as the runtime allows, so a maker can track fair value tick-by-tick at
//! negligible compute cost. Quote prices are computed from `mid ± offset` at
//! quote time; nothing reprices on a mid write.
//!
//! Fills deplete per-level `filled` counters (the ladder is standing intent,
//! not an order book — a consumed level stays consumed until the maker
//! rewrites the side). Safety is the mid-staleness gate: a maker whose feed
//! died stops quoting once `mid_slot` falls `max_mid_staleness_slots` behind.
//!
//! Two identities live on the instance and they are deliberately separate:
//! `authority` is the maker's *config* key (pause, rotate, reshape) and
//! `user_authority` is the wallet half of the quoted velocity `User`. Nothing
//! derives one from the other — a desk can run its config key cold while the
//! quoted sub-account belongs to a different wallet.
//!
//! Responses are written *in place*: `quote_v0`/`execute_v0` stream borsh
//! straight into the `response` tail of this account (a zero-copy view of
//! account data) and return a [`ResponsePointerV0`] to it. No intermediate
//! `Vec` is built and no bytes are copied twice.

use {
    crate::error::MidpointError,
    anchor_lang_v2::{prelude::*, BorshConfig, BORSH_CONFIG},
    static_assertions::const_assert_eq,
    wincode::io::Cursor,
};

/// Offsets are parts-per-million of mid (velocity's PERCENTAGE_PRECISION).
pub const PERCENTAGE_PRECISION_U64: u64 = 1_000_000;
pub const PERCENTAGE_PRECISION: u128 = PERCENTAGE_PRECISION_U64 as u128;

/// Ladder capacity per side.
pub const MAX_SPLINE_LEVELS: usize = 64;

/// Response region size. A full-ladder quote response is `4 + 64 × 16`
/// bytes and the execute response is one balance change, so this leaves
/// ample headroom.
pub const RESPONSE_BUFFER_BYTES: usize = 2048;

pub const ZERO_ADDRESS: Address = Address::new_from_array([0u8; 32]);

/// Taker direction, as passed through the quoter interface (same wire enum
/// as the CLOB's).
#[derive(Clone, Copy, PartialEq, Eq, Debug, wincode::SchemaRead, wincode::SchemaWrite)]
pub enum Direction {
    Long,
    Short,
}

/// Which sides a `cancel_all_v0` withdraws. The same wire enum (and the same
/// borsh tags) as the CLOB's, so a client speaks one shape to either quoter
/// type. Named sides rather than a pair of bools because the wire must not be
/// able to express "neither" — that is a maker believing their quotes are gone
/// when nothing happened.
#[derive(Clone, Copy, PartialEq, Eq, Debug, wincode::SchemaRead, wincode::SchemaWrite)]
pub enum CancelSidesV0 {
    Bids,
    Asks,
    Both,
}

impl CancelSidesV0 {
    /// The taker directions that consume the named sides. A bid is what a
    /// `Short` taker hits, an ask what a `Long` taker hits.
    pub fn directions(self) -> &'static [Direction] {
        match self {
            CancelSidesV0::Bids => &[Direction::Short],
            CancelSidesV0::Asks => &[Direction::Long],
            CancelSidesV0::Both => &[Direction::Short, Direction::Long],
        }
    }
}

/// What a `cancel_all_v0` withdrew.
///
/// Rung counts and nothing more, deliberately. Unlike the CLOB's, this sweep
/// reserves nothing on velocity's side — spline depth is standing intent,
/// margin-clamped at execute — so no caller has aggregates to unwind and the
/// response is informational. Totalling the withdrawn intent as well would mean
/// reading every rung back before zeroing it, which costs more per rung than
/// the withdrawal itself and reports a number the maker (who wrote the shape)
/// already knows.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default, wincode::SchemaRead, wincode::SchemaWrite)]
pub struct CancelAllOutcomeV0 {
    /// Rungs withdrawn per side.
    pub bid_rungs: u8,
    pub ask_rungs: u8,
    /// Whether the mid was zeroed too.
    pub mid_cleared: bool,
}

/// A velocity user in its derivable form — see the CLOB's `UserRefV0` for
/// why identity is stored as `(authority, sub_account_id)` rather than the
/// `User` account key.
#[derive(Clone, Copy, PartialEq, Eq, Debug, wincode::SchemaRead, wincode::SchemaWrite)]
pub struct UserRefV0 {
    pub authority: Address,
    pub sub_account_id: u16,
}

impl UserRefV0 {
    pub const ZERO: Self = Self {
        authority: ZERO_ADDRESS,
        sub_account_id: 0,
    };
}

/// Capacity of [`UserSetV0`] — see the CLOB's `USER_SET_CAPACITY` for where
/// the number comes from. Every program on the quoter wire must agree on it.
pub const USER_SET_CAPACITY: usize = 48;

/// The caller's settleable-user set as velocity's `QuoterUserSetV0` puts it on
/// the wire: a live count then a fixed-width array, so decoding it costs no
/// allocation and the encoding is pinned rather than negotiated. Empty means
/// unrestricted.
#[derive(Clone, Copy, PartialEq, Eq, Debug, wincode::SchemaRead, wincode::SchemaWrite)]
pub struct UserSetV0 {
    pub len: u8,
    pub users: [UserRefV0; USER_SET_CAPACITY],
}

/// Encoded width of a [`UserSetV0`]: 32-byte authority + u16 sub-account each,
/// behind a one-byte count.
pub const USER_SET_BYTES: usize = 1 + USER_SET_CAPACITY * (32 + 2);

impl UserSetV0 {
    pub const EMPTY: Self = Self {
        len: 0,
        users: [UserRefV0::ZERO; USER_SET_CAPACITY],
    };

    /// The live prefix. `len` arrives from a foreign caller, so it is clamped
    /// rather than trusted.
    pub fn as_slice(&self) -> &[UserRefV0] {
        &self.users[..(self.len as usize).min(USER_SET_CAPACITY)]
    }

    /// Build from at most [`USER_SET_CAPACITY`] refs; `None` past capacity.
    pub fn from_refs(refs: &[UserRefV0]) -> Option<Self> {
        if refs.len() > USER_SET_CAPACITY {
            return None;
        }
        let mut set = Self::EMPTY;
        set.users[..refs.len()].copy_from_slice(refs);
        set.len = refs.len() as u8;
        Some(set)
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, wincode::SchemaRead, wincode::SchemaWrite)]
pub struct PriceLevel {
    pub price: u64,
    pub size: u64,
}

/// One user's share of an executed fill. Mirrors velocity's quoter-interface
/// `UserBalanceChange`; the midpoint always has exactly one (the quoted
/// user) and never completes orders (the ladder has none).
///
/// This is the wire *definition* — the program writes the same bytes field by
/// field (see [`MidpointQuoterV0::write_execute_response`]) rather than
/// building one of these, and a unit test pins the two encodings equal.
#[derive(Clone, PartialEq, Eq, Debug, wincode::SchemaRead, wincode::SchemaWrite)]
pub struct UserBalanceChange {
    pub user: UserRefV0,
    pub base_size: u64,
    pub quote_size: u64,
    pub completed_order_ids: Vec<u64>,
}

/// Sub-min cancelled remainder — wire compatibility with the quoter
/// interface; the midpoint never emits one (spline intent has no orders to
/// cancel, a dusty level is simply not quoted).
#[derive(Clone, Copy, PartialEq, Eq, Debug, wincode::SchemaRead, wincode::SchemaWrite)]
pub struct CancelledRemainderV0 {
    pub user: UserRefV0,
    pub order_id: u64,
    pub base_asset_amount: u64,
}

/// Where in the quoter account the borsh response was written. Returned via
/// return data by `quote_v0`/`execute_v0`.
#[derive(Clone, Copy, PartialEq, Eq, Debug, wincode::SchemaRead, wincode::SchemaWrite)]
pub struct ResponsePointerV0 {
    pub offset: u32,
    pub len: u32,
}

/// Wire definition of the quote response. See [`UserBalanceChange`] on why
/// the program does not construct one.
#[derive(Clone, wincode::SchemaRead, wincode::SchemaWrite)]
pub struct QuoteResponseV0 {
    /// Levels the quoter will fill at, best price first.
    pub levels: Vec<PriceLevel>,
}

#[derive(Clone, wincode::SchemaRead, wincode::SchemaWrite)]
pub struct ExecuteResponseV0 {
    pub balance_changes: Vec<UserBalanceChange>,
    pub cancelled: Vec<CancelledRemainderV0>,
}

/// One rung of the spline: standing intent `size` at `mid ± offset`, with
/// `filled` tracking what executes have consumed since the side was written.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, bytemuck::Pod, bytemuck::Zeroable)]
pub struct SplineLevelV0 {
    /// Distance from mid toward this side, parts-per-million of mid.
    /// Strictly ascending within a side; 0 quotes at mid itself.
    pub offset_ppm: u64,
    /// Intent size, in the market's base precision.
    pub size: u64,
    /// Consumed since the side was last written.
    pub filled: u64,
}

const_assert_eq!(core::mem::size_of::<SplineLevelV0>(), 24);

/// A maker's spline-level input on the wire: `filled` is program-owned, so
/// the setter takes only the shape.
#[derive(Clone, Copy, PartialEq, Eq, Debug, wincode::SchemaRead, wincode::SchemaWrite)]
pub struct SplineLevelInputV0 {
    pub offset_ppm: u64,
    pub size: u64,
}

#[account]
pub struct MidpointQuoterV0 {
    /// The maker's config key: pause, reconfigure, rotate the hot key. Signs
    /// creation. Never reassigned — the maker's kill switch stays theirs.
    /// Independent of `user_authority`: a desk can hold this cold.
    pub authority: Address,
    /// Hot key allowed to write mid/levels. Rotatable by `authority`, so the
    /// key that signs thousands of mid updates a day carries no other power.
    pub hot_authority: Address,
    /// Only signer allowed to execute (velocity's quoter CPI signer PDA —
    /// velocity clamps size to the quoted user's margin before CPI'ing here).
    /// Immutable after init.
    pub execute_authority: Address,
    /// Wallet half of the quoted velocity `User` — fills settle against
    /// `(user_authority, user_sub_account_id)`. It signs creation (consent)
    /// and rides the PDA seeds, so it is fixed for the life of the instance;
    /// quoting a different wallet means a new instance that wallet signs for.
    pub user_authority: Address,
    /// Mid price, in the market's price precision (velocity perps:
    /// PRICE_PRECISION = 1e6). 0 = not quoting.
    pub mid_price: u64,
    /// Slot of the last mid write — the staleness gate's input.
    pub mid_slot: u64,
    /// Monotonic guard for racing mid writers (see
    /// [`MidpointQuoterV0::set_mid`]).
    pub mid_sequence: u64,
    /// Quotes go empty once `mid_slot` falls this many slots behind.
    pub max_mid_staleness_slots: u64,
    /// Quote prices round to a multiple of this, away from mid (same price
    /// precision as `mid_price`).
    pub price_tick_size: u64,
    /// Quoted sizes floor to a multiple of this (base precision).
    pub size_step: u64,
    /// Level remainders below this are not quoted. Base precision — a floor
    /// on the *base* size of a rung, despite the name.
    pub min_quote_size: u64,
    /// Base units per whole unit (velocity perps: 1e9).
    pub base_precision: u64,
    /// Sub-account half of the quoted user's identity (`user_authority`
    /// above is the wallet half).
    pub user_sub_account_id: u16,
    /// Velocity perp market index this quoter serves.
    pub market_index: u16,
    /// Maker kill switch (config path; the registry's `is_active` is the
    /// velocity-side one).
    pub is_paused: u8,
    /// Quote only flow attested by velocity's current `hot_flow_authority`
    /// (see `crate::velocity`).
    pub require_attested_flow: u8,
    pub bid_count: u8,
    pub ask_count: u8,
    /// Room for two more pubkeys plus a scalar or two, so a future field
    /// lands without moving the ladders or the response tail.
    pub padding: [u8; 72],
    pub bids: [SplineLevelV0; MAX_SPLINE_LEVELS],
    pub asks: [SplineLevelV0; MAX_SPLINE_LEVELS],
    /// Scratch region `quote_v0`/`execute_v0` stream their borsh response
    /// into; return data carries a [`ResponsePointerV0`] locating it.
    pub response: [u8; RESPONSE_BUFFER_BYTES],
}

const_assert_eq!(core::mem::size_of::<MidpointQuoterV0>(), 5392);
// The header (everything before the ladders) stays 8-aligned and hole-free —
// `#[account]` is Pod, which rejects padding bytes.
const_assert_eq!(4 * 32 + 8 * 8 + 8 + 72, 272);

/// Account-data offset of the `response` region.
pub const RESPONSE_OFFSET: usize =
    8 + core::mem::size_of::<MidpointQuoterV0>() - RESPONSE_BUFFER_BYTES;

/// Immutable + rarely-changed config, set at init (the addresses ride the
/// accounts list — duplicated accounts cost one index byte in the tx).
#[derive(Clone, Copy, PartialEq, Eq, Debug, wincode::SchemaRead, wincode::SchemaWrite)]
pub struct QuoterConfigV0 {
    pub market_index: u16,
    pub user_sub_account_id: u16,
    pub base_precision: u64,
    pub max_mid_staleness_slots: u64,
    pub price_tick_size: u64,
    pub size_step: u64,
    pub min_quote_size: u64,
    pub require_attested_flow: bool,
}

/// The consumed prefix of one side of the spline for a taker of `size`:
/// what execute commits and quote previews.
pub struct SplineFill {
    pub base: u64,
    pub quote: u64,
    /// `filled` deltas per level index, applied only by execute.
    pub consumed: [u64; MAX_SPLINE_LEVELS],
}

/// The scalars a spline walk needs, copied out of the account so the walk can
/// borrow the ladders and the response tail at the same time.
#[derive(Clone, Copy, Debug)]
pub struct SplineParams {
    pub mid_price: u64,
    pub price_tick_size: u64,
    pub size_step: u64,
    pub min_quote_size: u64,
    pub base_precision: u64,
}

impl SplineParams {
    /// A level's price at the current mid: `mid ± mid × offset / 1e6`,
    /// rounded *away* from mid to the tick (maker-conservative). `None` if
    /// the price would leave u64 or the bid side would cross zero.
    ///
    /// This runs once per rung per quote, so it stays in u64: SBF has no
    /// 128-bit divide and each `u128 / u128` costs hundreds of compute units,
    /// which a 64-rung ladder would pay 128 times over. `mid × offset_ppm`
    /// fits u64 for any plausible market (1e8 price × 1e6 ppm = 1e14); the
    /// u128 path exists only for the inputs where it doesn't, and returns
    /// exactly what the all-u128 form would (pinned by a unit test).
    pub fn level_price(&self, direction: Direction, offset_ppm: u64) -> Option<u64> {
        let mid = self.mid_price;
        let delta = match mid.checked_mul(offset_ppm) {
            Some(product) => product / PERCENTAGE_PRECISION_U64,
            None => {
                u64::try_from((mid as u128).checked_mul(offset_ppm as u128)? / PERCENTAGE_PRECISION)
                    .ok()?
            }
        };
        let tick = self.price_tick_size.max(1);
        let price = match direction {
            // Taker buys: ask above mid, rounded up.
            Direction::Long => mid.checked_add(delta)?.div_ceil(tick).checked_mul(tick)?,
            // Taker sells: bid below mid, rounded down.
            Direction::Short => mid.checked_sub(delta)? / tick * tick,
        };
        (price != 0).then_some(price)
    }

    /// A level's quotable remainder: intent minus consumed, floored to the
    /// step, zero when below the min-quote floor.
    pub fn level_remaining(&self, level: &SplineLevelV0) -> u64 {
        let step = self.size_step.max(1);
        let remaining = level.size.saturating_sub(level.filled) / step * step;
        if remaining < self.min_quote_size {
            0
        } else {
            remaining
        }
    }

    /// Quote amount for `base` at `price`, mirroring the CLOB:
    /// `price × base / base_precision`, floored, with `carry` — the previous
    /// rung's sub-unit remainder — folded in and the new remainder handed
    /// back. Carrying rather than truncating per rung is what makes a
    /// multi-rung fill's total the floor of the whole walk's notional, which
    /// is the one rounding velocity admits when it holds the fill to the
    /// prices this quoter published. u64 while the product fits (see
    /// [`SplineParams::level_price`] on why the divide is worth avoiding),
    /// widening only when it must.
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

    /// The side a taker of `direction` consumes.
    pub fn side_levels(&self, direction: Direction) -> &[SplineLevelV0] {
        match direction {
            Direction::Long => &self.asks[..self.ask_count as usize],
            Direction::Short => &self.bids[..self.bid_count as usize],
        }
    }

    /// Whether this quoter quotes at all right now — pause, unset mid, and
    /// the staleness gate. Attestation and self-trade gating live with the
    /// callers (they need the sysvar/taker inputs).
    pub fn is_quoting(&self, slot: u64) -> bool {
        self.is_paused == 0
            && self.mid_price != 0
            && slot.saturating_sub(self.mid_slot) <= self.max_mid_staleness_slots
    }

    /// Walk the consumed prefix for a fill of `size`, without mutating —
    /// execute applies `consumed` after the walk.
    pub fn fill(&self, direction: Direction, size: u64, slot: u64) -> Result<SplineFill> {
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

    /// Apply a walked fill's consumption to the side's `filled` counters, then
    /// assert the post-state invariants of a consuming walk (see
    /// [`MidpointQuoterV0::validate_consumption`]).
    pub fn apply_fill(&mut self, direction: Direction, fill: &SplineFill) -> Result<()> {
        let count = self.side_count(direction) as usize;
        let levels = match direction {
            Direction::Long => &mut self.asks[..count],
            Direction::Short => &mut self.bids[..count],
        };
        for (level, consumed) in levels.iter_mut().zip(fill.consumed.iter()) {
            level.filled = level.filled.saturating_add(*consumed);
        }
        self.validate_consumption(direction, fill)
    }

    /// Replace one side's ladder: offsets strictly ascending (best rung
    /// closest to mid first), sizes nonzero, `filled` reset.
    pub fn write_side(
        &mut self,
        direction: Direction,
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
        let (levels, count) = match direction {
            Direction::Long => (&mut self.asks[..], &mut self.ask_count),
            Direction::Short => (&mut self.bids[..], &mut self.bid_count),
        };
        for (slot, input) in levels.iter_mut().zip(inputs.iter()) {
            *slot = SplineLevelV0 {
                offset_ppm: input.offset_ppm,
                size: input.size,
                filled: 0,
            };
        }
        for slot in levels.iter_mut().skip(inputs.len()) {
            *slot = SplineLevelV0 {
                offset_ppm: 0,
                size: 0,
                filled: 0,
            };
        }
        *count = inputs.len() as u8;
        Ok(())
    }

    /// Withdraw one side's standing intent: zero its live rungs and drop the
    /// count to nothing. Returns the rungs cleared.
    ///
    /// Only the live prefix is written. The tail past `count` is already zero
    /// by the ladder invariant, so a maker running eight rungs pays for eight
    /// rather than for the ladder's capacity — which is what makes this cheaper
    /// than `set_levels_v0` with an empty side, and it is a straight `fill` so
    /// the write lowers to a memset rather than a per-rung loop.
    pub fn clear_side(&mut self, direction: Direction) -> u8 {
        let count = self.side_count(direction) as usize;
        let (levels, stored) = match direction {
            Direction::Long => (&mut self.asks[..], &mut self.ask_count),
            Direction::Short => (&mut self.bids[..], &mut self.bid_count),
        };
        levels[..count].fill(SplineLevelV0 {
            offset_ppm: 0,
            size: 0,
            filled: 0,
        });
        *stored = 0;
        count as u8
    }

    /// Post-condition of [`Self::clear_side`]: the side's count is gone and
    /// every rung the withdrawal wrote is zeroed, so nothing on it can quote.
    ///
    /// Scoped to the `cleared` rungs rather than the whole ladder, and that
    /// scope is the point. The tail past the old count was already zero by the
    /// ladder invariant and a withdrawal never writes there, so scanning it
    /// would be compute spent proving something this operation cannot break —
    /// on a path a maker takes under time pressure. Every other mutating
    /// instruction still runs the full [`Self::validate`], so a tail corrupted
    /// by anything else is still caught there. Same trade [`Self::set_mid`]
    /// makes for the same reason.
    pub fn validate_cleared_side(&self, direction: Direction, cleared: u8) -> Result<()> {
        require!(
            self.side_count(direction) == 0,
            MidpointError::InvariantViolated
        );
        let levels = match direction {
            Direction::Long => &self.asks,
            Direction::Short => &self.bids,
        };
        require!(
            levels[..(cleared as usize).min(MAX_SPLINE_LEVELS)]
                .iter()
                .all(|level| level.offset_ppm == 0 && level.size == 0 && level.filled == 0),
            MidpointError::InvariantViolated
        );
        Ok(())
    }

    /// Stamp a new mid. `sequence` is an opt-in monotonic guard for racing
    /// writers: nonzero sequences must strictly increase (a delayed relay
    /// can't clobber a fresher mid); zero skips the check (single-writer
    /// setups don't pay for coordination they don't need).
    ///
    /// The post-write assertion is deliberately O(1): this is the CU-pinned
    /// hot path, so it checks only what it just wrote, never the ladders.
    pub fn set_mid(&mut self, mid: u64, sequence: u64, slot: u64) -> Result<()> {
        if sequence != 0 {
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

    fn side_count(&self, direction: Direction) -> u8 {
        match direction {
            Direction::Long => self.ask_count,
            Direction::Short => self.bid_count,
        }
    }

    /// Invariants every non-hot mutating instruction leaves true. Cheap
    /// enough for config/shape writes (a bounded scan of both ladders);
    /// deliberately *not* called from `set_mid_v0`, which is compute-pinned
    /// and cannot touch a rung.
    pub fn validate(&self) -> Result<()> {
        require!(self.base_precision != 0, MidpointError::InvalidConfig);
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
        Self::validate_side(&self.bids, self.bid_count)?;
        Self::validate_side(&self.asks, self.ask_count)
    }

    /// One side is consistent: `count` live rungs with strictly ascending
    /// offsets, nonzero sizes and `filled <= size`, and a fully zeroed tail
    /// (so a shrinking rewrite can never leave a stale rung quotable).
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
                level.offset_ppm == 0 && level.size == 0 && level.filled == 0,
                MidpointError::InvariantViolated
            );
        }
        Ok(())
    }

    /// Post-execute invariants for the side just consumed:
    ///
    /// - consumption is monotone best-first — a rung is only touched once
    ///   every better rung is exhausted, so at most the *last* touched rung
    ///   is left with a quotable remainder;
    /// - no rung outside the live count was touched;
    /// - `filled <= size` everywhere, and the per-rung deltas sum to the
    ///   `base` the response reports.
    fn validate_consumption(&self, direction: Direction, fill: &SplineFill) -> Result<()> {
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
            // A take that left the rung quotable means the taker ran out of
            // size here: nothing beyond this rung may be consumed.
            walk_ended = params.level_remaining(level) > 0;
            total = total.checked_add(*taken).ok_or(MidpointError::MathError)?;
        }
        require!(total == fill.base, MidpointError::InvariantViolated);
        Ok(())
    }

    /// Stream a `QuoteResponseV0` for a taker of `direction`/`size` straight
    /// into the response tail, best-first, truncated once the taker's size is
    /// covered. `open` is the caller gate (settleability / self-trade /
    /// attestation); a closed gate or a non-quoting spline writes an empty
    /// level vec, which is the wire's "nothing for you".
    pub fn write_quote_response(
        &mut self,
        direction: Direction,
        size: u64,
        slot: u64,
        open: bool,
    ) -> Result<ResponsePointerV0> {
        let params = self.params();
        let live = open && self.is_quoting(slot);
        let count = if live { self.side_count(direction) } else { 0 } as usize;
        // Disjoint field borrows: the walk reads a ladder while the writer
        // holds the response tail. Both are views into the same account.
        let Self {
            bids,
            asks,
            response,
            ..
        } = self;
        let side: &[SplineLevelV0] = match direction {
            Direction::Long => &asks[..count],
            Direction::Short => &bids[..count],
        };

        let mut cursor = Cursor::new(&mut response[..]);
        // Length prefix, backfilled once the walk knows the count.
        write_wire(&mut cursor, &0u32)?;
        let mut quoted_levels: u32 = 0;
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
            let quoted = remaining.min(wanted);
            write_wire(
                &mut cursor,
                &PriceLevel {
                    price,
                    size: quoted,
                },
            )?;
            quoted_levels += 1;
            wanted -= quoted;
        }
        let len = cursor.position();
        response[..4].copy_from_slice(&quoted_levels.to_le_bytes());
        Ok(response_pointer(len))
    }

    /// Stream an `ExecuteResponseV0` straight into the response tail: at most
    /// one balance change (the quoted user), never a completed order id,
    /// never a cancelled remainder.
    pub fn write_execute_response(
        &mut self,
        change: Option<(u64, u64)>,
    ) -> Result<ResponsePointerV0> {
        let user = self.user_ref();
        let response = &mut self.response;
        let mut cursor = Cursor::new(&mut response[..]);
        match change {
            None => write_wire(&mut cursor, &0u32)?,
            Some((base_size, quote_size)) => {
                write_wire(&mut cursor, &1u32)?;
                write_wire(&mut cursor, &user)?;
                write_wire(&mut cursor, &base_size)?;
                write_wire(&mut cursor, &quote_size)?;
                // completed_order_ids: the ladder has no orders to complete.
                write_wire(&mut cursor, &0u32)?;
            }
        }
        // cancelled: standing intent has no remainders to cancel.
        write_wire(&mut cursor, &0u32)?;
        Ok(response_pointer(cursor.position()))
    }
}

/// Borsh-encode one value at the cursor. The wire config is anchor's
/// borsh-compatible one, so streaming field by field is byte-identical to
/// serializing the whole response struct.
fn write_wire<T>(cursor: &mut Cursor<&mut [u8]>, value: &T) -> Result<()>
where
    T: wincode::SchemaWrite<BorshConfig, Src = T> + ?Sized,
{
    wincode::config::serialize_into(cursor, value, BORSH_CONFIG)
        .map_err(|_| MidpointError::ResponseTooLarge.into())
}

fn response_pointer(len: usize) -> ResponsePointerV0 {
    ResponsePointerV0 {
        offset: RESPONSE_OFFSET as u32,
        len: len as u32,
    }
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
            .write_side(Direction::Short, &inputs(bids))
            .expect("bids");
        quoter
            .write_side(Direction::Long, &inputs(asks))
            .expect("asks");
        quoter.validate().expect("armed quoter is valid");
        quoter
    }

    /// The Vec-building reference encoder the program used to run, kept as
    /// the oracle for the streaming writer.
    fn reference_quote(quoter: &MidpointQuoterV0, direction: Direction, size: u64) -> Vec<u8> {
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
            levels.push(PriceLevel {
                price,
                size: quoted,
            });
            wanted -= quoted;
        }
        let mut bytes = Vec::new();
        wincode::config::serialize_into(&mut bytes, &QuoteResponseV0 { levels }, BORSH_CONFIG)
            .unwrap();
        bytes
    }

    fn reference_execute(quoter: &MidpointQuoterV0, change: Option<(u64, u64)>) -> Vec<u8> {
        let balance_changes = change
            .map(|(base_size, quote_size)| UserBalanceChange {
                user: quoter.user_ref(),
                base_size,
                quote_size,
                completed_order_ids: Vec::new(),
            })
            .into_iter()
            .collect();
        let mut bytes = Vec::new();
        wincode::config::serialize_into(
            &mut bytes,
            &ExecuteResponseV0 {
                balance_changes,
                cancelled: Vec::new(),
            },
            BORSH_CONFIG,
        )
        .unwrap();
        bytes
    }

    /// The tests here pin the written bytes against *this program's* schema.
    /// This pins that schema against `quoter-spec`, the one declaration of the
    /// layout every program on the wire shares — velocity decodes to it and
    /// the CLOB writes to it. The midpoint always emits exactly one change and
    /// never completes an order, so the empty vecs it always sends are part of
    /// what has to match.
    #[test]
    fn the_wire_struct_is_the_quoter_spec_layout() {
        let quoter = quoter(&[], &[]);
        let local = reference_execute(&quoter, Some((1_000_000_000, 101_000_000)));

        let mut from_spec = Vec::new();
        quoter_spec::ExecuteResponseV0 {
            balance_changes: vec![quoter_spec::UserBalanceChange {
                user: quoter_spec::UserRefV0 {
                    authority: quoter.user_ref().authority.to_bytes(),
                    sub_account_id: quoter.user_ref().sub_account_id,
                },
                base_size: 1_000_000_000,
                quote_size: 101_000_000,
                completed_order_ids: Vec::new(),
            }],
            cancelled: Vec::new(),
        }
        .encode(&mut from_spec);

        assert_eq!(local, from_spec);
    }

    fn written(quoter: &MidpointQuoterV0, pointer: ResponsePointerV0) -> Vec<u8> {
        assert_eq!(pointer.offset as usize, RESPONSE_OFFSET);
        quoter.response[..pointer.len as usize].to_vec()
    }

    /// The in-place writer must be byte-identical to serializing the wire
    /// struct — the response is a public wire, not an internal detail.
    #[test]
    fn streamed_quote_response_matches_the_wire_struct() {
        for size in [0, UNIT / 4, UNIT, 3 * UNIT, u64::MAX] {
            for direction in [Direction::Long, Direction::Short] {
                let mut quoter = quoter(
                    &[(1_000, UNIT), (3_000, UNIT / 2)],
                    &[(500, UNIT / 2), (2_500, UNIT), (9_000, 5 * UNIT)],
                );
                let expected = reference_quote(&quoter, direction, size);
                let pointer = quoter
                    .write_quote_response(direction, size, 0, true)
                    .unwrap();
                assert_eq!(written(&quoter, pointer), expected);
            }
        }
    }

    #[test]
    fn a_closed_gate_streams_an_empty_level_vec() {
        let mut quoter = quoter(&[(1_000, UNIT)], &[(1_000, UNIT)]);
        let closed = quoter
            .write_quote_response(Direction::Long, UNIT, 0, false)
            .unwrap();
        assert_eq!(written(&quoter, closed), 0u32.to_le_bytes().to_vec());
        // A stale mid is the same silence, even with the gate open.
        let stale = quoter
            .write_quote_response(Direction::Long, UNIT, 10_000, true)
            .unwrap();
        assert_eq!(written(&quoter, stale), 0u32.to_le_bytes().to_vec());
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
            .write_quote_response(Direction::Long, UNIT + UNIT / 2, 0, true)
            .unwrap();
        let bytes = written(&quoter, pointer);
        assert_eq!(u32::from_le_bytes(bytes[..4].try_into().unwrap()), 2);
        assert_eq!(
            u64::from_le_bytes(bytes[4..12].try_into().unwrap()),
            100_100_000
        );
        assert_eq!(u64::from_le_bytes(bytes[12..20].try_into().unwrap()), UNIT);
        assert_eq!(
            u64::from_le_bytes(bytes[20..28].try_into().unwrap()),
            100_300_000
        );
        assert_eq!(
            u64::from_le_bytes(bytes[28..36].try_into().unwrap()),
            UNIT / 2
        );
    }

    #[test]
    fn a_full_ladder_response_fits_the_buffer() {
        let side: Vec<(u64, u64)> = (0..MAX_SPLINE_LEVELS as u64)
            .map(|i| (1_000 + i, UNIT))
            .collect();
        let mut quoter = quoter(&side, &side);
        let pointer = quoter
            .write_quote_response(Direction::Long, u64::MAX, 0, true)
            .unwrap();
        assert_eq!(pointer.len as usize, 4 + MAX_SPLINE_LEVELS * 16);
        assert!(pointer.len as usize <= RESPONSE_BUFFER_BYTES);
    }

    #[test]
    fn execute_consumes_a_monotone_prefix() {
        let mut quoter = quoter(&[], &[(1_000, UNIT), (3_000, UNIT)]);
        let fill = quoter.fill(Direction::Long, UNIT + UNIT / 2, 0).unwrap();
        quoter.apply_fill(Direction::Long, &fill).unwrap();
        assert_eq!(fill.base, UNIT + UNIT / 2);
        assert_eq!(quoter.asks[0].filled, UNIT);
        assert_eq!(quoter.asks[1].filled, UNIT / 2);
        quoter.validate().unwrap();

        // The partially consumed rung is still quotable, so a second fill
        // starts there and the invariant still holds.
        let fill = quoter.fill(Direction::Long, UNIT, 0).unwrap();
        quoter.apply_fill(Direction::Long, &fill).unwrap();
        assert_eq!(fill.base, UNIT / 2);
        assert_eq!(quoter.asks[1].filled, UNIT);
        quoter.validate().unwrap();
    }

    #[test]
    fn consumption_beyond_a_partially_taken_rung_is_rejected() {
        let mut quoter = quoter(&[], &[(1_000, UNIT), (3_000, UNIT)]);
        // Hand-built impossible walk: half of rung 0 (leaving it quotable)
        // and half of rung 1.
        let mut consumed = [0u64; MAX_SPLINE_LEVELS];
        consumed[0] = UNIT / 2;
        consumed[1] = UNIT / 2;
        let fill = SplineFill {
            base: UNIT,
            quote: 0,
            consumed,
        };
        assert!(quoter.apply_fill(Direction::Long, &fill).is_err());
    }

    #[test]
    fn consumption_of_a_dead_rung_is_rejected() {
        let mut quoter = quoter(&[], &[(1_000, UNIT)]);
        let mut consumed = [0u64; MAX_SPLINE_LEVELS];
        // Rung 1 is past `ask_count` — nothing may touch it.
        consumed[1] = UNIT;
        let fill = SplineFill {
            base: UNIT,
            quote: 0,
            consumed,
        };
        assert!(quoter.apply_fill(Direction::Long, &fill).is_err());
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
        assert!(quoter.apply_fill(Direction::Long, &fill).is_err());
    }

    #[test]
    fn clear_side_withdraws_one_side_and_leaves_the_other() {
        let mut quoter = quoter(&[(1_000, UNIT), (3_000, UNIT / 2)], &[(500, 2 * UNIT)]);
        // A partly-consumed rung is withdrawn like any other, `filled` and all.
        let fill = quoter.fill(Direction::Short, UNIT / 4, 0).unwrap();
        quoter.apply_fill(Direction::Short, &fill).unwrap();

        assert_eq!(quoter.clear_side(Direction::Short), 2);
        assert_eq!(quoter.bid_count, 0);
        assert!(quoter.bids.iter().all(|level| *level
            == SplineLevelV0 {
                offset_ppm: 0,
                size: 0,
                filled: 0
            }));
        // The ask side is untouched and still quotes.
        assert_eq!(quoter.ask_count, 1);
        assert_eq!(quoter.asks[0].size, 2 * UNIT);
        quoter.validate().unwrap();
        assert!(quoter
            .write_quote_response(Direction::Short, UNIT, 0, true)
            .is_ok());
        assert!(quoter.fill(Direction::Short, UNIT, 0).unwrap().base == 0);
        assert_eq!(quoter.fill(Direction::Long, UNIT, 0).unwrap().base, UNIT);
    }

    /// Clearing an empty side is a no-op that still leaves a valid ladder, so
    /// the instruction is idempotent — a maker can fire it twice without a
    /// failed transaction telling them nothing was wrong.
    #[test]
    fn clearing_an_empty_side_is_a_valid_no_op() {
        let mut quoter = quoter(&[], &[(500, UNIT)]);
        assert_eq!(quoter.clear_side(Direction::Short), 0);
        quoter.validate().unwrap();
        assert_eq!(quoter.clear_side(Direction::Long), 1);
        assert_eq!(quoter.clear_side(Direction::Long), 0);
        quoter.validate().unwrap();
    }

    /// The withdrawal post-check covers the rungs the withdrawal wrote and a
    /// count that failed to drop — and deliberately not the tail beyond them,
    /// which is the trade that keeps it proportional to the shape being pulled.
    /// The full [`MidpointQuoterV0::validate`] every other mutating instruction
    /// runs is what covers the tail.
    #[test]
    fn the_withdrawal_post_check_covers_what_the_withdrawal_wrote() {
        let mut quoter = quoter(&[(1_000, UNIT), (3_000, UNIT)], &[(500, UNIT)]);
        let cleared = quoter.clear_side(Direction::Short);
        quoter
            .validate_cleared_side(Direction::Short, cleared)
            .unwrap();

        // A rung the withdrawal should have zeroed but didn't.
        quoter.bids[1].size = UNIT;
        assert!(quoter
            .validate_cleared_side(Direction::Short, cleared)
            .is_err());
        quoter.bids[1].size = 0;

        // A count that failed to drop, which would leave the side quotable.
        quoter.bid_count = 1;
        assert!(quoter
            .validate_cleared_side(Direction::Short, cleared)
            .is_err());
        quoter.bid_count = 0;

        // Scoped to the side it was asked about: the ask side is still live and
        // that is not an error.
        quoter
            .validate_cleared_side(Direction::Short, cleared)
            .unwrap();
        assert_eq!(quoter.ask_count, 1);

        // Stale tail rungs are out of scope here, and caught by `validate`.
        quoter.bids[7].size = UNIT;
        quoter
            .validate_cleared_side(Direction::Short, cleared)
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

    /// The all-u128 form of `level_price`, kept as the oracle for the u64 fast
    /// path the program actually runs.
    fn reference_level_price(
        params: &SplineParams,
        direction: Direction,
        offset_ppm: u64,
    ) -> Option<u64> {
        let mid = params.mid_price as u128;
        let delta = mid
            .checked_mul(offset_ppm as u128)?
            .checked_div(PERCENTAGE_PRECISION)?;
        let tick = (params.price_tick_size as u128).max(1);
        let price = match direction {
            Direction::Long => mid.checked_add(delta)?.div_ceil(tick).checked_mul(tick)?,
            Direction::Short => {
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

    /// The u64 fast paths must be exactly the u128 math, including where they
    /// give up — a rounding difference here is a mispriced fill.
    #[test]
    fn the_u64_price_math_matches_the_u128_form() {
        let mids = [0, 1, 99, 100_000_000, u64::MAX / 2, u64::MAX - 1, u64::MAX];
        let offsets = [0, 1, 999, 1_000_000, u64::MAX / 3, u64::MAX];
        let ticks = [0, 1, 7, 100, 1_000_000, u64::MAX];
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
                    for direction in [Direction::Long, Direction::Short] {
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
        // Zero opts out of the guard entirely.
        quoter.set_mid(MID + 2, 0, 12).unwrap();
        assert_eq!(quoter.mid_price, MID + 2);
        assert_eq!(quoter.mid_sequence, 6);
        assert_eq!(quoter.mid_slot, 12);
    }
}
