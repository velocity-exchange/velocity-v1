//! Wall-clock durations and the live slot length.
//!
//! Solana's slot time is dropping from 400ms to 200ms through a series of
//! feature gates (400 -> 350 -> 300 -> 250 -> 200). Any slot count used as a
//! wall-clock duration drifts with each gate, so the program separates the two
//! concepts by type:
//!
//! - [`Millis`] is the only duration unit in the codebase. Every wall-clock
//!   threshold, window, ramp, and grace period is a `Millis`, whether it comes
//!   from a code constant or an admin-set field. It cannot be compared against
//!   a slot count without converting through the live slot length.
//! - [`SlotDuration`] is the current slot length, admin-updated on `State` as
//!   each gate activates. It is the sole bridge between durations and slots
//!   and is not constructible from an arbitrary number, so a raw slot value
//!   can never be passed where the slot length belongs.
//! - Plain `u64` remains the type of actual slot counts: same-slot
//!   idempotence, blockhash windows, per-order auction snapshots. Genuine
//!   chain-slot logic never touches `Millis`.
//!
//! Legacy admin-set fields (oracle guard rails, `liquidation_duration`, the
//! per-market delay overrides, `min_perp_auction_duration`) keep their compact
//! onchain encoding in units of [`STORED_UNIT_MS`] = 400ms, the historical
//! slot length. That factor is a storage codec detail confined to those
//! fields' getters; it is not a unit any logic thinks in. New stored durations
//! should store milliseconds natively.
//!
//! Rounding is deliberate and mirrored by the TypeScript SDK exactly:
//! [`Millis::to_slots`] floors (staleness windows come out marginally tighter,
//! the safe direction); [`Millis::to_slots_ceil`] is for user-protection
//! windows (the user never gets less than the intended time);
//! [`Millis::from_slots`] is exact; [`Millis::div_periods`] floors (elapsed
//! time is under-counted, so rate ramps engage marginally later, favoring the
//! affected user).
//!
//! Full design rationale, the gate-activation runbook, and worked examples
//! live in [`docs/SLOT-DURATION.md`](../../../../docs/SLOT-DURATION.md).

use crate::math::safe_math::SafeMath;

/// Storage encoding quantum for pre-gate duration fields: the historical
/// 400ms slot length. Exists only in the encode/decode of those fields (and in
/// the calibration periods of a few legacy per-slot rates). Never used by new
/// code; new stored durations store milliseconds natively.
pub const STORED_UNIT_MS: u64 = 400;

/// The set of slot durations the admin may configure, matching the IBRL
/// feature-gate schedule. 400 is the pre-upgrade default and is not settable
/// (there is no path back to slower slots: feature gates cannot deactivate).
pub const VALID_SLOT_DURATIONS_MS: [u16; 4] = [350, 300, 250, 200];

/// A wall-clock duration in milliseconds. The only duration unit in the
/// codebase; see the module doc for the type discipline.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug, Default)]
pub struct Millis(u64);

impl Millis {
    pub const ZERO: Millis = Millis(0);

    /// The historical 400ms calibration period. A few legacy rates and step
    /// functions were tuned as "per slot" in the 400ms era; their accrual
    /// period is this constant, made explicit at the site because changing it
    /// changes economics.
    pub const UNIT: Millis = Millis(STORED_UNIT_MS);

    pub const fn from_ms(ms: u64) -> Self {
        Millis(ms)
    }

    pub const fn from_secs(secs: u64) -> Self {
        Millis(secs.saturating_mul(1_000))
    }

    /// Decode a legacy stored value denominated in [`STORED_UNIT_MS`] units.
    /// Storage codec only; never use for new values.
    pub const fn from_stored_units(units: u64) -> Self {
        Millis(units.saturating_mul(STORED_UNIT_MS))
    }

    /// The exact wall-clock time a measured slot delta represents at the
    /// current slot duration.
    pub fn from_slots(slots: u64, d: SlotDuration) -> Self {
        Millis(slots.saturating_mul(d.0))
    }

    pub const fn as_ms(self) -> u64 {
        self.0
    }

    /// This duration expressed in actual slots at the current slot duration,
    /// rounding down. Default for staleness windows: marginally tighter than
    /// wall-clock is the safe direction.
    pub fn to_slots(self, d: SlotDuration) -> u64 {
        self.0.safe_div(d.0.max(1)).unwrap_or(self.0)
    }

    /// This duration expressed in actual slots, rounding up. For
    /// user-protection windows (liquidation ramps, grace periods): the user
    /// never gets less than the intended time.
    pub fn to_slots_ceil(self, d: SlotDuration) -> u64 {
        self.0.safe_div_ceil(d.0.max(1)).unwrap_or(self.0)
    }

    /// How many whole `period`s fit in this duration (floor). For legacy
    /// per-period rates: elapsed time is under-counted, so ramps engage
    /// marginally later, favoring the affected user.
    pub fn div_periods(self, period: Millis) -> u64 {
        self.0.safe_div(period.0.max(1)).unwrap_or(0)
    }

    pub fn saturating_mul(self, n: u64) -> Self {
        Millis(self.0.saturating_mul(n))
    }
}

/// The live slot length in milliseconds. Read it via [`SlotDuration::from_state_ms`]
/// on `State::slot_duration()`; there is deliberately no constructor from a
/// bare number in program logic, so a slot count can never be passed as the
/// slot length.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct SlotDuration(u64);

impl SlotDuration {
    /// The pre-gate 400ms baseline; what an unset (`0`) `State.slot_duration_ms`
    /// resolves to, and the value under which every conversion in this module
    /// is the identity on the legacy stored units.
    pub const BASELINE: SlotDuration = SlotDuration(STORED_UNIT_MS);

    /// Resolve the raw `State.slot_duration_ms` field: `0` is what pre-upgrade
    /// accounts read out of former padding and means "unset" (the 400ms
    /// baseline).
    pub const fn from_state_ms(raw: u16) -> Self {
        if raw == 0 {
            SlotDuration::BASELINE
        } else {
            SlotDuration(raw as u64)
        }
    }

    pub const fn as_ms(self) -> u64 {
        self.0
    }
}

/// A per-market oracle slot-delay override, decoded from its stored `i8`
/// (values are legacy [`STORED_UNIT_MS`] units).
///
/// The two override fields share this decode but use different sentinel
/// schemes, so each has its own constructor:
/// - immediate-fill override: `0` = never allow immediate AMM fills, `< 0` =
///   unset (source-aware fallback), `> 0` = explicit threshold;
/// - low-risk override: `0` = unset (use the guard rails), otherwise an
///   explicit threshold clamped at zero.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum DelayOverride {
    /// Immediate fills never allowed on this market.
    Never,
    /// No explicit threshold configured; the caller's fallback applies.
    Unset,
    /// Explicit staleness threshold.
    Fixed(Millis),
}

impl DelayOverride {
    /// Decode `PerpMarket.oracle_slot_delay_override`.
    pub const fn from_immediate(raw: i8) -> Self {
        if raw == 0 {
            DelayOverride::Never
        } else if raw < 0 {
            DelayOverride::Unset
        } else {
            DelayOverride::Fixed(Millis::from_stored_units(raw as u64))
        }
    }

    /// Decode `PerpMarket.oracle_low_risk_slot_delay_override`.
    pub const fn from_low_risk(raw: i8) -> Self {
        if raw == 0 {
            DelayOverride::Unset
        } else if raw < 0 {
            DelayOverride::Fixed(Millis::ZERO)
        } else {
            DelayOverride::Fixed(Millis::from_stored_units(raw as u64))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn baseline_is_identity_on_stored_units() {
        for v in [0u64, 1, 10, 120, 1_500, 18_144_000] {
            let m = Millis::from_stored_units(v);
            assert_eq!(m.to_slots(SlotDuration::BASELINE), v);
            assert_eq!(m.to_slots_ceil(SlotDuration::BASELINE), v);
            assert_eq!(
                Millis::from_slots(v, SlotDuration::BASELINE).div_periods(Millis::UNIT),
                v
            );
        }
    }

    #[test]
    fn zero_state_field_resolves_to_baseline() {
        assert_eq!(SlotDuration::from_state_ms(0), SlotDuration::BASELINE);
        assert_eq!(SlotDuration::from_state_ms(200).as_ms(), 200);
    }

    #[test]
    fn terminal_gate_doubles_slot_counts() {
        let d = SlotDuration::from_state_ms(200);
        assert_eq!(Millis::from_secs(4).to_slots(d), 20);
        assert_eq!(Millis::from_secs(60).to_slots_ceil(d), 300);
        // 20 actual slots at 200ms = 4s = 10 whole 400ms periods
        assert_eq!(Millis::from_slots(20, d).div_periods(Millis::UNIT), 10);
    }

    #[test]
    fn intermediate_gates_round_as_documented() {
        // 4s at 350ms: 4000 / 350 = 11.43
        let d350 = SlotDuration::from_state_ms(350);
        assert_eq!(Millis::from_secs(4).to_slots(d350), 11);
        assert_eq!(Millis::from_secs(4).to_slots_ceil(d350), 12);
        // 4s at 300ms: 13.33
        assert_eq!(
            Millis::from_secs(4).to_slots(SlotDuration::from_state_ms(300)),
            13
        );
        // 4s at 250ms: exact
        assert_eq!(
            Millis::from_secs(4).to_slots(SlotDuration::from_state_ms(250)),
            16
        );
        // period counting floors: 3 slots at 200ms = 600ms = 1 whole 400ms period
        assert_eq!(
            Millis::from_slots(3, SlotDuration::from_state_ms(200)).div_periods(Millis::UNIT),
            1
        );
    }

    #[test]
    fn valid_slot_durations_are_the_gate_values_only() {
        // pins the admin-settable set: exactly the four IBRL gate values, and
        // neither 0 (unset sentinel) nor 400 (baseline) is settable.
        assert_eq!(VALID_SLOT_DURATIONS_MS, [350, 300, 250, 200]);
        assert!(!VALID_SLOT_DURATIONS_MS.contains(&0));
        assert!(!VALID_SLOT_DURATIONS_MS.contains(&400));
        // every settable value is strictly below the 400ms baseline, so the
        // first flip from unset (which resolves to 400) always passes the
        // handler's monotonic-decrease guard.
        for v in VALID_SLOT_DURATIONS_MS {
            assert!((v as u64) < SlotDuration::BASELINE.as_ms());
        }
    }

    #[test]
    fn overrides_decode_sentinels() {
        assert_eq!(DelayOverride::from_immediate(0), DelayOverride::Never);
        assert_eq!(DelayOverride::from_immediate(-1), DelayOverride::Unset);
        assert_eq!(
            DelayOverride::from_immediate(100),
            DelayOverride::Fixed(Millis::from_ms(40_000))
        );
        assert_eq!(DelayOverride::from_low_risk(0), DelayOverride::Unset);
        assert_eq!(
            DelayOverride::from_low_risk(-5),
            DelayOverride::Fixed(Millis::ZERO)
        );
        assert_eq!(
            DelayOverride::from_low_risk(10),
            DelayOverride::Fixed(Millis::from_ms(4_000))
        );
    }
}
