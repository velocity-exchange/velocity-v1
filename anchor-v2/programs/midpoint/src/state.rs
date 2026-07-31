//! Midpoint quoter state: one account per instantiation. A maker doesn't
//! deploy a program — they create a [`MidpointQuoterV0`] PDA of this one and
//! register it as a Custom quoter in velocity's registry.
//!
//! The quoting model is a *spline around a midpoint*: the maker maintains a
//! shape — per-side ladders of `(offset from mid, size)` — that moves rarely,
//! and a mid price that moves constantly. The hot path is therefore
//! [`set_mid`]: a single stamped u64 write, kept as close to free as the
//! runtime allows, so a maker can track fair value tick-by-tick at negligible
//! compute cost. Quote prices are computed from `mid ± offset` at quote time;
//! nothing reprices on a mid write.
//!
//! Fills deplete per-level `filled` counters (the ladder is standing intent,
//! not an order book — a consumed level stays consumed until the maker
//! rewrites the side). Safety is the mid-staleness gate: a maker whose feed
//! died stops quoting once `mid_slot` falls `max_mid_staleness_slots` behind.

use {crate::error::MidpointError, anchor_lang_v2::prelude::*, static_assertions::const_assert_eq};

/// Offsets are parts-per-million of mid (velocity's PERCENTAGE_PRECISION).
pub const PERCENTAGE_PRECISION: u128 = 1_000_000;

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

/// A velocity user in its derivable form — see the CLOB's `UserRefV0` for
/// why identity is stored as `(authority, sub_account_id)` rather than the
/// `User` account key.
#[derive(Clone, Copy, PartialEq, Eq, Debug, wincode::SchemaRead, wincode::SchemaWrite)]
pub struct UserRefV0 {
    pub authority: Address,
    pub sub_account_id: u16,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, wincode::SchemaRead, wincode::SchemaWrite)]
pub struct PriceLevel {
    pub price: u64,
    pub size: u64,
}

/// One user's share of an executed fill. Mirrors velocity's quoter-interface
/// `UserBalanceChange`; the midpoint always has exactly one (the quoted
/// user) and never completes orders (the ladder has none).
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
    /// Intent size, base precision.
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
    /// The quoted wallet — signs creation (consent, mirroring the registry's
    /// Custom-entry rule) and config updates. Never reassigned.
    pub authority: Address,
    /// Hot key allowed to write mid/levels. Rotatable by `authority`, so the
    /// key that signs thousands of mid updates a day carries no other power.
    pub hot_authority: Address,
    /// Only signer allowed to execute (the velocity signer PDA — velocity
    /// clamps size to the quoted user's margin before CPI'ing here).
    /// Immutable after init.
    pub execute_authority: Address,
    /// When `require_attested_flow` is set, quotes exist only for
    /// transactions this address co-signed (checked via instructions-sysvar
    /// introspection — a quoter CPI never sees real signer bits).
    pub flow_authority: Address,
    /// Mid price, PRICE_PRECISION. 0 = not quoting.
    pub mid_price: u64,
    /// Slot of the last mid write — the staleness gate's input.
    pub mid_slot: u64,
    /// Monotonic guard for racing mid writers (see [`set_mid`]).
    pub mid_sequence: u64,
    /// Quotes go empty once `mid_slot` falls this many slots behind.
    pub max_mid_staleness_slots: u64,
    /// Quote prices round to a multiple of this, away from mid (PRICE_PRECISION).
    pub price_tick_size: u64,
    /// Quoted sizes floor to a multiple of this (base precision).
    pub size_step: u64,
    /// Level remainders below this are not quoted (base precision).
    pub min_quote_size: u64,
    /// Base units per whole unit (velocity perps: 1e9).
    pub base_precision: u64,
    /// Sub-account half of the quoted user's identity (`authority` above is
    /// the wallet half).
    pub user_sub_account_id: u16,
    /// Velocity perp market index this quoter serves.
    pub market_index: u16,
    /// Maker kill switch (config path; the registry's `is_active` is the
    /// velocity-side one).
    pub is_paused: u8,
    /// Quote only attested flow (see `flow_authority`).
    pub require_attested_flow: u8,
    pub bid_count: u8,
    pub ask_count: u8,
    pub bids: [SplineLevelV0; MAX_SPLINE_LEVELS],
    pub asks: [SplineLevelV0; MAX_SPLINE_LEVELS],
    /// Scratch region `quote_v0`/`execute_v0` write their borsh response
    /// into; return data carries a [`ResponsePointerV0`] locating it.
    pub response: [u8; RESPONSE_BUFFER_BYTES],
}

const_assert_eq!(core::mem::size_of::<MidpointQuoterV0>(), 5320);

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

impl MidpointQuoterV0 {
    pub fn user_ref(&self) -> UserRefV0 {
        UserRefV0 {
            authority: self.authority,
            sub_account_id: self.user_sub_account_id,
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

    /// A level's price at the current mid: `mid ± mid × offset / 1e6`,
    /// rounded *away* from mid to the tick (maker-conservative). `None` if
    /// the bid side would cross zero.
    pub fn level_price(&self, direction: Direction, offset_ppm: u64) -> Option<u64> {
        let mid = self.mid_price as u128;
        let delta = mid
            .checked_mul(offset_ppm as u128)?
            .checked_div(PERCENTAGE_PRECISION)?;
        let tick = (self.price_tick_size as u128).max(1);
        let price = match direction {
            // Taker buys: ask above mid, rounded up.
            Direction::Long => mid.checked_add(delta)?.div_ceil(tick).checked_mul(tick)?,
            // Taker sells: bid below mid, rounded down.
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

    /// A level's quotable remainder: intent minus consumed, floored to the
    /// step, zero when below the min-quote floor.
    fn level_remaining(&self, level: &SplineLevelV0) -> u64 {
        let step = self.size_step.max(1);
        let remaining = level.size.saturating_sub(level.filled) / step * step;
        if remaining < self.min_quote_size {
            0
        } else {
            remaining
        }
    }

    /// Price levels for a taker of `direction`/`size`, best-first, truncated
    /// once the taker's size is covered. Empty when not quoting.
    pub fn quote(&self, direction: Direction, size: u64, slot: u64) -> Vec<PriceLevel> {
        if !self.is_quoting(slot) {
            return Vec::new();
        }
        let mut levels = Vec::new();
        let mut wanted = size;
        for level in self.side_levels(direction) {
            if wanted == 0 {
                break;
            }
            let remaining = self.level_remaining(level);
            if remaining == 0 {
                continue;
            }
            let Some(price) = self.level_price(direction, level.offset_ppm) else {
                continue;
            };
            let quoted = remaining.min(wanted);
            levels.push(PriceLevel {
                price,
                size: quoted,
            });
            wanted -= quoted;
        }
        levels
    }

    /// Walk the consumed prefix for a fill of `size`, without mutating —
    /// execute applies `consumed` after the walk. Quote math mirrors the
    /// CLOB: `price × base / base_precision`, floored.
    pub fn fill(&self, direction: Direction, size: u64, slot: u64) -> Result<SplineFill> {
        let mut fill = SplineFill {
            base: 0,
            quote: 0,
            consumed: [0; MAX_SPLINE_LEVELS],
        };
        if !self.is_quoting(slot) {
            return Ok(fill);
        }
        let mut wanted = size;
        for (index, level) in self.side_levels(direction).iter().enumerate() {
            if wanted == 0 {
                break;
            }
            let remaining = self.level_remaining(level);
            if remaining == 0 {
                continue;
            }
            let Some(price) = self.level_price(direction, level.offset_ppm) else {
                continue;
            };
            let take = remaining.min(wanted);
            let quote_size: u64 = (price as u128)
                .checked_mul(take as u128)
                .ok_or(MidpointError::MathError)?
                .checked_div(self.base_precision.max(1) as u128)
                .ok_or(MidpointError::MathError)?
                .try_into()
                .map_err(|_| MidpointError::MathError)?;
            fill.base = fill
                .base
                .checked_add(take)
                .ok_or(MidpointError::MathError)?;
            fill.quote = fill
                .quote
                .checked_add(quote_size)
                .ok_or(MidpointError::MathError)?;
            fill.consumed[index] = take;
            wanted -= take;
        }
        Ok(fill)
    }

    /// Apply a walked fill's consumption to the side's `filled` counters.
    pub fn apply_fill(&mut self, direction: Direction, fill: &SplineFill) {
        let levels = match direction {
            Direction::Long => &mut self.asks[..],
            Direction::Short => &mut self.bids[..],
        };
        for (level, consumed) in levels.iter_mut().zip(fill.consumed.iter()) {
            level.filled = level.filled.saturating_add(*consumed);
        }
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

    /// Stamp a new mid. `sequence` is an opt-in monotonic guard for racing
    /// writers: nonzero sequences must strictly increase (a delayed relay
    /// can't clobber a fresher mid); zero skips the check (single-writer
    /// setups don't pay for coordination they don't need).
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
        Ok(())
    }

    pub fn write_response(&mut self, data: &[u8]) -> Result<ResponsePointerV0> {
        require!(
            data.len() <= RESPONSE_BUFFER_BYTES,
            MidpointError::ResponseTooLarge
        );
        self.response[..data.len()].copy_from_slice(data);
        Ok(ResponsePointerV0 {
            offset: RESPONSE_OFFSET as u32,
            len: data.len() as u32,
        })
    }
}
