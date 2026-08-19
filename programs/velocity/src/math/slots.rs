//! Slot-count scaling for variable slot duration.
//!
//! Solana's slot time is dropping from 400ms to 200ms through a series of
//! feature gates (400 -> 350 -> 300 -> 250 -> 200). Every slot-denominated
//! constant and admin-set field in this program keeps its historical value and
//! is interpreted as a count of 400ms baseline units; `State.slot_duration_ms`
//! records what a slot is currently worth, and the helpers here convert
//! between baseline units and actual slots at read time. This keeps every
//! window, ramp, and rate constant in wall-clock terms across all gate
//! activations with a single admin flip per gate and no layout changes.
//!
//! Two directions, with deliberate rounding:
//! - [`effective_slots`] / [`effective_slots_ceil`] inflate a
//!   baseline-denominated threshold into actual slots. Floor is the default
//!   (staleness windows come out marginally tighter than wall-clock, the safe
//!   direction); the ceil variant is for user-protection windows (liquidation
//!   ramps, grace periods) where the user should never get less than the
//!   intended time.
//! - [`base_units_from_slots`] deflates a measured slot delta into baseline
//!   units for comparison against unscaled constants. Floor: elapsed time is
//!   under-counted, so ramps and expiries engage marginally later, which
//!   favors the affected user.

use crate::math::safe_math::SafeMath;

/// The slot duration every slot-denominated value in this program was
/// calibrated against, in milliseconds.
pub const BASE_SLOT_DURATION_MS: u64 = 400;

/// The set of slot durations the admin may configure, matching the IBRL
/// feature-gate schedule. 400 is the pre-upgrade default and is not settable
/// (there is no path back to slower slots: feature gates cannot deactivate).
pub const VALID_SLOT_DURATIONS_MS: [u16; 4] = [350, 300, 250, 200];

/// Sanitize the raw `State.slot_duration_ms` field. `0` is what pre-upgrade
/// accounts read out of former padding and means "unset": the 400ms baseline.
pub fn sanitize_slot_duration_ms(raw: u16) -> u64 {
    if raw == 0 {
        BASE_SLOT_DURATION_MS
    } else {
        raw as u64
    }
}

/// Inflate a baseline(400ms)-denominated slot count into actual slots at the
/// current slot duration, rounding down.
pub fn effective_slots(base_slots: u64, slot_duration_ms: u64) -> u64 {
    base_slots
        .saturating_mul(BASE_SLOT_DURATION_MS)
        .safe_div(slot_duration_ms.max(1))
        .unwrap_or(base_slots)
}

/// Inflate a baseline(400ms)-denominated slot count into actual slots at the
/// current slot duration, rounding up.
pub fn effective_slots_ceil(base_slots: u64, slot_duration_ms: u64) -> u64 {
    base_slots
        .saturating_mul(BASE_SLOT_DURATION_MS)
        .safe_div_ceil(slot_duration_ms.max(1))
        .unwrap_or(base_slots)
}

/// [`effective_slots`] for i64 thresholds (oracle guard rails). Non-positive
/// values are sentinels and pass through unscaled.
pub fn effective_slots_i64(base_slots: i64, slot_duration_ms: u64) -> i64 {
    if base_slots <= 0 {
        return base_slots;
    }
    effective_slots(base_slots as u64, slot_duration_ms).min(i64::MAX as u64) as i64
}

/// Deflate a measured slot delta into baseline(400ms) units, rounding down.
pub fn base_units_from_slots(slots: u64, slot_duration_ms: u64) -> u64 {
    slots
        .saturating_mul(slot_duration_ms)
        .safe_div(BASE_SLOT_DURATION_MS)
        .unwrap_or(slots)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn baseline_is_identity() {
        for v in [0u64, 1, 10, 120, 1_500, 18_144_000] {
            assert_eq!(effective_slots(v, 400), v);
            assert_eq!(effective_slots_ceil(v, 400), v);
            assert_eq!(base_units_from_slots(v, 400), v);
        }
    }

    #[test]
    fn zero_duration_treated_as_baseline() {
        assert_eq!(sanitize_slot_duration_ms(0), 400);
        assert_eq!(sanitize_slot_duration_ms(200), 200);
    }

    #[test]
    fn terminal_gate_doubles() {
        assert_eq!(effective_slots(10, 200), 20);
        assert_eq!(effective_slots_ceil(150, 200), 300);
        assert_eq!(base_units_from_slots(20, 200), 10);
    }

    #[test]
    fn intermediate_gates_round_as_documented() {
        // 350ms: 10 * 400 / 350 = 11.43
        assert_eq!(effective_slots(10, 350), 11);
        assert_eq!(effective_slots_ceil(10, 350), 12);
        // 300ms: 10 * 4 / 3 = 13.33
        assert_eq!(effective_slots(10, 300), 13);
        // 250ms: exact
        assert_eq!(effective_slots(10, 250), 16);
        // deflation floors: 3 slots @200ms = 600ms = 1.5 base units
        assert_eq!(base_units_from_slots(3, 200), 1);
    }

    #[test]
    fn i64_sentinels_pass_through() {
        assert_eq!(effective_slots_i64(-1, 200), -1);
        assert_eq!(effective_slots_i64(0, 200), 0);
        assert_eq!(effective_slots_i64(100, 200), 200);
    }
}
