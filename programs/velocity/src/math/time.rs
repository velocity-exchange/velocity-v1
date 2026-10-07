//! Wall-clock durations and the live slot length.
//!
//! Solana's slot time drops from 400ms to 200ms through a series of feature
//! gates: 400, then 350, then 300, then 250, then 200. Any slot count used as a
//! wall-clock duration drifts with each gate, so the program separates the two
//! concepts by type.
//!
//! - [`Millis`] is the only duration unit in the codebase. Every wall-clock
//!   threshold, window, ramp, and grace period is a `Millis`, whether it comes
//!   from a code constant or an admin-set field. A comparison against a slot
//!   count must convert through the live slot length first.
//! - [`SlotDuration`] is the slot length resolved from the permissionlessly
//!   synchronized transition archive on `State`. It is the only bridge between
//!   durations and slots. No constructor takes an arbitrary number, so a raw
//!   slot value can never be passed where the slot length belongs.
//! - [`StoredSlotDuration`] is a compact onchain duration encoded in quanta of
//!   a slot length fixed when the field was introduced. For example,
//!   `StoredSlotDuration<u8, 400>` still occupies one byte, but its type records
//!   that a stored `10` means 4,000ms. Program logic normalizes it to [`Millis`]
//!   at once. It is never read against the live slot length.
//! - Plain `u64` remains the type of actual slot counts, such as same-slot
//!   idempotence, blockhash windows, and per-order auction snapshots. Genuine
//!   chain-slot logic never touches `Millis`.
//!
//! Legacy admin-set fields keep their compact onchain encoding in units of
//! [`STORED_UNIT_MS`], the historical 400ms slot length. Ordinary duration
//! fields state that in their [`StoredSlotDuration`] type. Signed fields whose
//! raw values carry sentinel meanings keep their raw integer type and use a
//! purpose-specific decoder such as [`DelayOverride`].
//!
//! Rounding is deliberate, and the TypeScript SDK mirrors it exactly.
//! [`Millis::to_slots`] floors, so a staleness window comes out marginally
//! tighter, which is the safe direction. [`Millis::to_slots_ceil`] serves
//! user-protection windows, where the user never gets less than the intended
//! time. [`Millis::from_slots`] is exact. [`Millis::div_periods`] floors, so
//! elapsed time is under-counted and a rate ramp engages marginally later,
//! which favors the affected user.
//!
//! [`docs/SLOT-DURATION.md`](../../../../docs/SLOT-DURATION.md) holds the
//! design rationale, the gate-activation runbook, and worked examples.

use {
    crate::math::safe_math::SafeMath,
    anchor_lang::prelude::*,
    bytemuck::{Pod, Zeroable},
    std::convert::{TryFrom, TryInto},
};

/// Storage encoding quantum for pre-gate duration fields. It is the historical 400ms slot length.
/// It appears only in the encode and decode of those fields, and in the calibration period of a
/// few legacy per-slot rates. A new compact field puts its own quantum in [`StoredSlotDuration`]'s
/// const parameter instead.
pub const STORED_UNIT_MS: u64 = 400;

/// A compact wall-clock duration stored as `T` fixed-slot quanta.
///
/// `SLOT_MS` records the slot length assumed when the field was introduced. It
/// is a storage codec and not the chain's live slot length. The transparent
/// representation keeps the wrapped integer's exact size, alignment, and bytes,
/// so existing accounts stay layout-compatible.
///
/// A new field or API creates values with [`Self::try_from_millis`], which
/// rejects a duration that is not an exact multiple of `SLOT_MS` or does not
/// fit in `T`. Existing admin instructions keep their legacy raw-unit wire
/// arguments. [`Self::from_raw_units`] serves those boundaries and decodes
/// account data that is already encoded.
#[repr(transparent)]
#[derive(
    Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug, Default, AnchorSerialize, AnchorDeserialize,
)]
pub struct StoredSlotDuration<T, const SLOT_MS: u64>(T);

// SAFETY: `StoredSlotDuration` is `repr(transparent)` over its only stored
// field, `T`. The const generic occupies no memory. Every all-zero bit pattern
// valid for a `Zeroable` `T` is therefore valid for this wrapper.
unsafe impl<T: Zeroable, const SLOT_MS: u64> Zeroable for StoredSlotDuration<T, SLOT_MS> {}

// SAFETY: `repr(transparent)` gives this wrapper exactly `T`'s layout, with no
// extra field and no padding. When `T` is `Pod`, the wrapper has the same valid
// bit patterns, so a zero-copy account read is safe.
unsafe impl<T: Pod, const SLOT_MS: u64> Pod for StoredSlotDuration<T, SLOT_MS> {}

impl<T, const SLOT_MS: u64> StoredSlotDuration<T, SLOT_MS>
where
    T: Copy + TryFrom<u64> + TryInto<u64>,
{
    /// Wrap fixed-slot units that are already encoded. Use it only at an
    /// account boundary or a legacy API boundary. Duration arithmetic goes
    /// through [`Self::to_millis`].
    pub const fn from_raw_units(units: T) -> Self {
        assert!(SLOT_MS > 0, "stored slot-duration quantum must be nonzero");
        Self(units)
    }

    /// Encode an exact millisecond duration without changing storage width.
    pub fn try_from_millis(duration: Millis) -> Option<Self> {
        if SLOT_MS == 0 || !duration.as_ms().is_multiple_of(SLOT_MS) {
            return None;
        }
        T::try_from(duration.as_ms() / SLOT_MS).ok().map(Self)
    }

    /// Decode into the common wall-clock arithmetic type.
    pub fn to_millis(self) -> Millis {
        assert!(SLOT_MS > 0, "stored slot-duration quantum must be nonzero");
        // A signed legacy field may hold a historical negative value. The
        // failed conversion normalizes to zero on purpose. That matches the
        // `value.max(0) as u64` decode this type replaced.
        let units = self.0.try_into().unwrap_or(0);
        Millis::from_ms(units.saturating_mul(SLOT_MS))
    }

    /// Return the underlying fixed-slot units for wire compatibility, logging,
    /// or a legacy instruction boundary.
    pub const fn raw_units(self) -> T {
        self.0
    }
}

impl<T: std::fmt::Display, const SLOT_MS: u64> std::fmt::Display
    for StoredSlotDuration<T, SLOT_MS>
{
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(formatter)
    }
}

// A runtime build and an IDL build see different Rust type names over the same
// wire bytes.
//
// - A normal program build uses `StoredSlotDuration`, which gives Rust the
//   strong fixed-quantum type.
// - Anchor's `idl-build` sees the original integer type. Anchor describes the
//   transparent generic wrapper as a defined tuple struct, so its JavaScript
//   Borsh coder would decode it as `{ 0: value }` instead of the primitive
//   number or BN that clients already consume.
//
// Each alias name may have exactly one definition in a build, so the two `cfg`
// attributes must complement each other. Remove this split once Anchor's IDL
// and JavaScript coder flatten a transparent wrapper to its inner primitive.
#[cfg(feature = "idl-build")]
pub type LegacySlotDurationU8 = u8;
#[cfg(not(feature = "idl-build"))]
pub type LegacySlotDurationU8 = StoredSlotDuration<u8, STORED_UNIT_MS>;

#[cfg(feature = "idl-build")]
pub type LegacySlotDurationI64 = i64;
#[cfg(not(feature = "idl-build"))]
pub type LegacySlotDurationI64 = StoredSlotDuration<i64, STORED_UNIT_MS>;

#[cfg(feature = "idl-build")]
pub type LegacySlotDurationU64 = u64;
#[cfg(not(feature = "idl-build"))]
pub type LegacySlotDurationU64 = StoredSlotDuration<u64, STORED_UNIT_MS>;

pub const fn legacy_slot_duration_u8(units: u8) -> LegacySlotDurationU8 {
    #[cfg(not(feature = "idl-build"))]
    {
        StoredSlotDuration::from_raw_units(units)
    }
    #[cfg(feature = "idl-build")]
    {
        units
    }
}

pub const fn legacy_slot_duration_i64(units: i64) -> LegacySlotDurationI64 {
    #[cfg(not(feature = "idl-build"))]
    {
        StoredSlotDuration::from_raw_units(units)
    }
    #[cfg(feature = "idl-build")]
    {
        units
    }
}

pub const fn legacy_slot_duration_u64(units: u64) -> LegacySlotDurationU64 {
    #[cfg(not(feature = "idl-build"))]
    {
        StoredSlotDuration::from_raw_units(units)
    }
    #[cfg(feature = "idl-build")]
    {
        units
    }
}

pub fn legacy_slot_duration_u8_to_millis(value: LegacySlotDurationU8) -> Millis {
    #[cfg(not(feature = "idl-build"))]
    {
        value.to_millis()
    }
    #[cfg(feature = "idl-build")]
    {
        Millis::from_stored_units(value as u64)
    }
}

pub fn legacy_slot_duration_i64_to_millis(value: LegacySlotDurationI64) -> Millis {
    #[cfg(not(feature = "idl-build"))]
    {
        value.to_millis()
    }
    #[cfg(feature = "idl-build")]
    {
        Millis::from_stored_units(value.max(0) as u64)
    }
}

pub const fn legacy_slot_duration_i64_raw(value: LegacySlotDurationI64) -> i64 {
    #[cfg(not(feature = "idl-build"))]
    {
        value.raw_units()
    }
    #[cfg(feature = "idl-build")]
    {
        value
    }
}

pub fn legacy_slot_duration_u64_to_millis(value: LegacySlotDurationU64) -> Millis {
    #[cfg(not(feature = "idl-build"))]
    {
        value.to_millis()
    }
    #[cfg(feature = "idl-build")]
    {
        Millis::from_stored_units(value)
    }
}

/// The four regimes after the baseline, in activation order. They match the
/// IBRL feature gate schedule. 400ms is the baseline before the upgrade. A
/// feature gate cannot deactivate, so there is no path back to slower slots.
/// `State` stores the first slot of each regime at the matching array index.
pub const SLOT_DURATION_TRANSITION_MS: [u16; 4] = [350, 300, 250, 200];

/// Archive index of a slot duration after the baseline.
pub const fn slot_duration_transition_index(slot_duration_ms: u16) -> Option<usize> {
    let mut i = 0;
    while i < SLOT_DURATION_TRANSITION_MS.len() {
        if SLOT_DURATION_TRANSITION_MS[i] == slot_duration_ms {
            return Some(i);
        }
        i += 1;
    }
    None
}

/// The raw `slot_duration_ms` in force at `now_slot`, from the `State` staging
/// fields. It is the staged `pending_ms` once `now_slot` reaches
/// `effective_slot`, and the current `base_ms` before that. This is the one
/// statement of the staged switch. `State::active_slot_duration_ms`, the native
/// fast-path reader and the off-chain mirrors all share it, so they cannot
/// diverge.
pub const fn active_slot_duration_ms(
    base_ms: u16,
    pending_ms: u16,
    effective_slot: u64,
    now_slot: u64,
) -> u16 {
    if pending_ms != 0 && now_slot >= effective_slot {
        pending_ms
    } else {
        base_ms
    }
}

/// Cluster slot clock rebuilt from the four IBRL transition slots.
///
/// The legacy staging fields stay as a fallback for an account written by the
/// first slot duration implementation. Once any archive entry exists, the
/// archive wins, and an elapsed interval is integrated one regime at a time.
/// `Default` is the 400ms baseline, because all-zero fields are exactly
/// [`SlotClock::baseline`].
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct SlotClock {
    transition_slots: [u64; 4],
    legacy_base_ms: u16,
    legacy_pending_ms: u16,
    legacy_effective_slot: u64,
}

impl SlotClock {
    pub const fn from_state_fields(
        transition_slots: [u64; 4],
        legacy_base_ms: u16,
        legacy_pending_ms: u16,
        legacy_effective_slot: u64,
    ) -> Self {
        Self {
            transition_slots,
            legacy_base_ms,
            legacy_pending_ms,
            legacy_effective_slot,
        }
    }

    pub const fn baseline() -> Self {
        Self::from_state_fields([0; 4], 0, 0, 0)
    }

    pub fn has_transition_history(self) -> bool {
        self.transition_slots.iter().any(|slot| *slot != 0)
    }

    /// Slot duration in force at the start of `slot`.
    pub fn slot_duration_at(self, slot: u64) -> SlotDuration {
        if !self.has_transition_history() {
            return SlotDuration::from_state_ms(active_slot_duration_ms(
                self.legacy_base_ms,
                self.legacy_pending_ms,
                self.legacy_effective_slot,
                slot,
            ));
        }

        let mut duration_ms = STORED_UNIT_MS as u16;
        let mut i = 0;
        while i < self.transition_slots.len() {
            let transition_slot = self.transition_slots[i];
            if transition_slot != 0 && slot >= transition_slot {
                duration_ms = SLOT_DURATION_TRANSITION_MS[i];
            }
            i += 1;
        }
        SlotDuration::from_state_ms(duration_ms)
    }

    /// Exact elapsed wall-clock milliseconds from the start of `start_slot` to
    /// the start of `end_slot`. The walk integrates each crossed slot duration
    /// regime on its own, which matches Agave's transition archive.
    pub fn elapsed(self, start_slot: u64, end_slot: u64) -> Millis {
        if end_slot <= start_slot {
            return Millis::ZERO;
        }
        if !self.has_transition_history() {
            return Millis::from_slots(
                end_slot.saturating_sub(start_slot),
                self.slot_duration_at(end_slot),
            );
        }

        let mut cursor = start_slot;
        let mut elapsed_ms = 0u64;
        let mut duration = self.slot_duration_at(start_slot);
        for transition_slot in self.transition_slots {
            if transition_slot == 0 || transition_slot <= cursor || transition_slot > end_slot {
                continue;
            }
            elapsed_ms = elapsed_ms.saturating_add(
                transition_slot
                    .saturating_sub(cursor)
                    .saturating_mul(duration.as_ms()),
            );
            cursor = transition_slot;
            duration = self.slot_duration_at(cursor);
        }
        elapsed_ms = elapsed_ms.saturating_add(
            end_slot
                .saturating_sub(cursor)
                .saturating_mul(duration.as_ms()),
        );
        Millis::from_ms(elapsed_ms)
    }

    /// Elapsed time represented by `slot_delta`, ending at `end_slot`.
    pub fn elapsed_slot_delta(self, slot_delta: u64, end_slot: u64) -> Millis {
        self.elapsed(end_slot.saturating_sub(slot_delta), end_slot)
    }

    /// First slot whose start is at least `duration` after `start_slot`. The
    /// walk integrates the known future transition boundaries. It does not
    /// convert the whole window at the duration of one endpoint.
    pub fn slot_at_or_after_duration(self, start_slot: u64, duration: Millis) -> u64 {
        if duration == Millis::ZERO {
            return start_slot;
        }

        let mut cursor = start_slot;
        let mut remaining_ms = duration.as_ms();
        let mut current_duration = self.slot_duration_at(cursor);

        for transition_slot in self.transition_slots {
            if transition_slot == 0 || transition_slot <= cursor {
                continue;
            }

            let slots_in_regime = transition_slot.saturating_sub(cursor);
            let regime_ms = slots_in_regime.saturating_mul(current_duration.as_ms());
            if remaining_ms <= regime_ms {
                return cursor
                    .saturating_add(Millis::from_ms(remaining_ms).to_slots_ceil(current_duration));
            }

            remaining_ms = remaining_ms.saturating_sub(regime_ms);
            cursor = transition_slot;
            current_duration = self.slot_duration_at(cursor);
        }

        cursor.saturating_add(Millis::from_ms(remaining_ms).to_slots_ceil(current_duration))
    }

    pub const fn transition_slots(self) -> [u64; 4] {
        self.transition_slots
    }
}

/// A wall-clock duration in milliseconds. It is the only duration unit in the
/// codebase. The module doc states the type discipline.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug, Default)]
pub struct Millis(u64);

impl Millis {
    pub const ZERO: Millis = Millis(0);

    /// The historical 400ms calibration period. A few legacy rates and step
    /// functions were tuned per slot in the 400ms era. Their accrual period is
    /// this constant, named at the site because a change to it changes
    /// economics.
    pub const UNIT: Millis = Millis(STORED_UNIT_MS);

    pub const fn from_ms(ms: u64) -> Self {
        Millis(ms)
    }

    pub const fn from_secs(secs: u64) -> Self {
        Millis(secs.saturating_mul(1_000))
    }

    /// Decode a legacy stored value held in [`STORED_UNIT_MS`] units. It is a
    /// storage codec. Never use it for a new value.
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

    /// This duration in actual slots at the current slot duration, rounded
    /// down. It is the default for a staleness window, because marginally
    /// tighter than wall-clock is the safe direction.
    pub fn to_slots(self, d: SlotDuration) -> u64 {
        self.0.safe_div(d.0.max(1)).unwrap_or(0)
    }

    /// This duration in actual slots, rounded up. It serves a user-protection
    /// window such as a liquidation ramp or a grace period, where the user
    /// never gets less than the intended time.
    pub fn to_slots_ceil(self, d: SlotDuration) -> u64 {
        self.0.safe_div_ceil(d.0.max(1)).unwrap_or(0)
    }

    /// How many whole `period` spans fit in this duration, rounded down. It
    /// serves the legacy per-period rates. Elapsed time is under-counted, so a
    /// ramp engages marginally later, which favors the affected user.
    pub fn div_periods(self, period: Millis) -> u64 {
        self.0.safe_div(period.0.max(1)).unwrap_or(0)
    }

    pub fn saturating_mul(self, n: u64) -> Self {
        Millis(self.0.saturating_mul(n))
    }
}

/// The live slot length in milliseconds. Read it with
/// [`SlotDuration::from_state_ms`] on `State::slot_duration()`. Program logic
/// has no constructor from a bare number, so a slot count can never be passed
/// as the slot length.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct SlotDuration(u64);

impl SlotDuration {
    /// The 400ms baseline before the gates. An unset `State.slot_duration_ms`
    /// of `0` resolves to it. Under it every conversion in this module is the
    /// identity on the legacy stored units.
    pub const BASELINE: SlotDuration = SlotDuration(STORED_UNIT_MS);

    /// Resolve the raw `State.slot_duration_ms` field. A pre-upgrade account
    /// reads `0` out of former padding. `0` means unset, which is the 400ms
    /// baseline.
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

/// A per-market oracle slot-delay override, decoded from its stored `i8`. The
/// stored values are in legacy [`STORED_UNIT_MS`] units.
///
/// The two override fields share this decode but use different sentinels, so
/// each has its own constructor.
///
/// - Immediate-fill override: `0` never allows an immediate AMM fill, a
///   negative value is unset and takes the source-aware fallback, and a
///   positive value is an explicit threshold.
/// - Low-risk override: `0` is unset and takes the guard rails, and any other
///   value is an explicit threshold clamped at zero.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum DelayOverride {
    /// Immediate fills never allowed on this market.
    Never,
    /// No explicit threshold configured. The caller's fallback applies.
    Unset,
    /// Explicit staleness threshold.
    Fixed(Millis),
}

/// The storage codec both override fields share. It is an `i8` in
/// [`STORED_UNIT_MS`] quanta. The override fields cannot hold that type
/// directly, because their raw values carry sentinels, so the quantum is named
/// here. The migration rule is the same as for any stored quantum. A change to
/// it reinterprets every stored byte and needs an admin rewrite, never a type
/// edit alone.
type DelayOverrideStored = StoredSlotDuration<i8, STORED_UNIT_MS>;

impl DelayOverride {
    /// Decode the raw units of a positive override. Every caller branches on
    /// the sentinels first, so only a strictly positive value reaches this.
    fn decode_positive(raw: i8) -> Millis {
        DelayOverrideStored::from_raw_units(raw).to_millis()
    }

    /// Decode `PerpMarket.oracle_slot_delay_override`.
    pub fn from_immediate(raw: i8) -> Self {
        if raw == 0 {
            DelayOverride::Never
        } else if raw < 0 {
            DelayOverride::Unset
        } else {
            DelayOverride::Fixed(Self::decode_positive(raw))
        }
    }

    /// Decode `PerpMarket.oracle_low_risk_slot_delay_override`.
    pub fn from_low_risk(raw: i8) -> Self {
        if raw == 0 {
            DelayOverride::Unset
        } else if raw < 0 {
            DelayOverride::Fixed(Millis::ZERO)
        } else {
            DelayOverride::Fixed(Self::decode_positive(raw))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stored_slot_duration_preserves_storage_layout() {
        assert_eq!(std::mem::size_of::<StoredSlotDuration<u8, 400>>(), 1);
        assert_eq!(std::mem::align_of::<StoredSlotDuration<u8, 400>>(), 1);
        assert_eq!(std::mem::size_of::<StoredSlotDuration<i64, 400>>(), 8);
        assert_eq!(std::mem::align_of::<StoredSlotDuration<i64, 400>>(), 8);

        let value = StoredSlotDuration::<u8, 400>::from_raw_units(10);
        assert_eq!(bytemuck::bytes_of(&value), &[10]);
        let mut serialized = Vec::new();
        value.serialize(&mut serialized).unwrap();
        assert_eq!(serialized, vec![10]);
    }

    #[test]
    fn stored_slot_duration_encodes_only_exact_representable_millis() {
        type LegacyU8 = StoredSlotDuration<u8, 400>;

        let encoded = LegacyU8::try_from_millis(Millis::from_ms(4_000)).unwrap();
        assert_eq!(encoded.raw_units(), 10);
        assert_eq!(encoded.to_millis(), Millis::from_ms(4_000));
        assert!(LegacyU8::try_from_millis(Millis::from_ms(4_001)).is_none());
        assert!(LegacyU8::try_from_millis(Millis::from_ms(102_400)).is_none());

        let legacy_negative = StoredSlotDuration::<i64, 400>::from_raw_units(-1);
        assert_eq!(legacy_negative.to_millis(), Millis::ZERO);
    }

    #[test]
    fn stored_slot_duration_unit_is_part_of_the_type() {
        let legacy = StoredSlotDuration::<u8, 400>::from_raw_units(10);
        let newer = StoredSlotDuration::<u8, 200>::from_raw_units(10);
        assert_eq!(legacy.to_millis(), Millis::from_ms(4_000));
        assert_eq!(newer.to_millis(), Millis::from_ms(2_000));
    }

    #[cfg(feature = "idl-build")]
    #[test]
    fn idl_build_alias_helpers_preserve_primitive_semantics() {
        // Anchor's IDL build puts a primitive alias in place of each
        // transparent wrapper. Pin those cfg-only branches so they cannot drift
        // from the runtime codec.
        let u8_value = legacy_slot_duration_u8(10);
        let i64_value = legacy_slot_duration_i64(-1);
        let u64_value = legacy_slot_duration_u64(120);
        assert_eq!(u8_value, 10u8);
        assert_eq!(
            legacy_slot_duration_u8_to_millis(u8_value),
            Millis::from_ms(4_000)
        );
        assert_eq!(legacy_slot_duration_i64_raw(i64_value), -1);
        assert_eq!(legacy_slot_duration_i64_to_millis(i64_value), Millis::ZERO);
        assert_eq!(u64_value, 120u64);
        assert_eq!(
            legacy_slot_duration_u64_to_millis(u64_value),
            Millis::from_ms(48_000)
        );
    }

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
        // 20 actual slots at 200ms is 4s, which is 10 whole 400ms periods.
        assert_eq!(Millis::from_slots(20, d).div_periods(Millis::UNIT), 10);
    }

    #[test]
    fn intermediate_gates_round_as_documented() {
        // 4s at 350ms: 4000 / 350 = 11.43.
        let d350 = SlotDuration::from_state_ms(350);
        assert_eq!(Millis::from_secs(4).to_slots(d350), 11);
        assert_eq!(Millis::from_secs(4).to_slots_ceil(d350), 12);
        // 4s at 300ms: 13.33.
        assert_eq!(
            Millis::from_secs(4).to_slots(SlotDuration::from_state_ms(300)),
            13
        );

        // 4s at 250ms: exact.
        assert_eq!(
            Millis::from_secs(4).to_slots(SlotDuration::from_state_ms(250)),
            16
        );

        // Period counting floors: 3 slots at 200ms is 600ms, which is 1 whole
        // 400ms period.
        assert_eq!(
            Millis::from_slots(3, SlotDuration::from_state_ms(200)).div_periods(Millis::UNIT),
            1
        );
    }

    #[test]
    fn transition_durations_are_the_gate_values_only() {
        // The synchronizable set is exactly the four IBRL gate values. Neither
        // the unset sentinel 0 nor the 400ms baseline has a gate.
        assert_eq!(SLOT_DURATION_TRANSITION_MS, [350, 300, 250, 200]);
        assert!(!SLOT_DURATION_TRANSITION_MS.contains(&0));
        assert!(!SLOT_DURATION_TRANSITION_MS.contains(&400));
        // Every value is below the 400ms baseline, because there is no path
        // back to slower slots.
        for v in SLOT_DURATION_TRANSITION_MS {
            assert!((v as u64) < SlotDuration::BASELINE.as_ms());
        }
    }

    #[test]
    fn slot_clock_without_history_falls_back_to_legacy_staging() {
        // Legacy staging fields: base 400, pending 350 effective at 1_000.
        let clock = SlotClock::from_state_fields([0; 4], 400, 350, 1_000);
        assert!(!clock.has_transition_history());
        assert_eq!(clock.slot_duration_at(999).as_ms(), 400);
        assert_eq!(clock.slot_duration_at(1_000).as_ms(), 350);
        // With no history the whole delta is priced at the end slot duration.
        assert_eq!(clock.elapsed(0, 10).as_ms(), 10 * 400);
        assert_eq!(clock.elapsed(1_000, 1_010).as_ms(), 10 * 350);
    }

    #[test]
    fn slot_clock_archive_is_authoritative_over_legacy_fields() {
        let clock = SlotClock::from_state_fields([1_000, 0, 0, 0], 200, 250, 5);
        assert!(clock.has_transition_history());
        // Before the transition the clock reads the 400ms baseline, whatever
        // the stale legacy fields say.
        assert_eq!(clock.slot_duration_at(999).as_ms(), 400);
        assert_eq!(clock.slot_duration_at(1_000).as_ms(), 350);
    }

    #[test]
    fn slot_clock_switches_at_each_transition_slot() {
        let clock = SlotClock::from_state_fields([1_000, 2_000, 3_000, 4_000], 0, 0, 0);
        for (slot, expected_ms) in [
            (0u64, 400u64),
            (999, 400),
            (1_000, 350),
            (1_999, 350),
            (2_000, 300),
            (2_999, 300),
            (3_000, 250),
            (3_999, 250),
            (4_000, 200),
            (1_000_000, 200),
        ] {
            assert_eq!(clock.slot_duration_at(slot).as_ms(), expected_ms);
        }
    }

    #[test]
    fn slot_clock_integrates_elapsed_time_piecewise() {
        let clock = SlotClock::from_state_fields([1_000, 2_000, 3_000, 4_000], 0, 0, 0);
        // Fully inside one regime.
        assert_eq!(clock.elapsed(0, 10).as_ms(), 10 * 400);
        assert_eq!(clock.elapsed(4_000, 4_010).as_ms(), 10 * 200);
        // Spanning one transition: 10 slots at 400ms plus 10 at 350ms.
        assert_eq!(clock.elapsed(990, 1_010).as_ms(), 10 * 400 + 10 * 350);
        // Spanning every transition.
        assert_eq!(
            clock.elapsed(0, 5_000).as_ms(),
            1_000 * 400 + 1_000 * 350 + 1_000 * 300 + 1_000 * 250 + 1_000 * 200
        );

        // A degenerate interval is zero.
        assert_eq!(clock.elapsed(10, 10), Millis::ZERO);
        assert_eq!(clock.elapsed(20, 10), Millis::ZERO);
        // The delta form anchors the interval at its end slot.
        assert_eq!(
            clock.elapsed_slot_delta(20, 1_010).as_ms(),
            10 * 400 + 10 * 350
        );

        // A delta larger than the end slot saturates to slot zero.
        assert_eq!(clock.elapsed_slot_delta(100, 50).as_ms(), 50 * 400);
    }

    #[test]
    fn slot_clock_projects_duration_across_future_transitions() {
        let clock = SlotClock::from_state_fields([1_000, 2_000, 0, 0], 0, 0, 0);
        assert_eq!(
            clock.slot_at_or_after_duration(995, Millis::from_secs(4)),
            1_006
        );
        assert_eq!(clock.elapsed(995, 1_005).as_ms(), 3_750);
        assert_eq!(clock.elapsed(995, 1_006).as_ms(), 4_100);

        assert_eq!(
            SlotClock::baseline().slot_at_or_after_duration(10, Millis::from_ms(801)),
            13
        );
        assert_eq!(
            SlotClock::baseline().slot_at_or_after_duration(10, Millis::ZERO),
            10
        );
    }

    #[test]
    fn slot_clock_with_partial_archive_stays_on_the_last_synced_regime() {
        // Only the first two transitions are synchronized so far.
        let clock = SlotClock::from_state_fields([1_000, 2_000, 0, 0], 0, 0, 0);
        assert_eq!(clock.slot_duration_at(1_000_000).as_ms(), 300);
        assert_eq!(clock.elapsed(1_990, 2_010).as_ms(), 10 * 350 + 10 * 300);
    }

    #[test]
    fn transition_index_maps_the_four_gates() {
        assert_eq!(slot_duration_transition_index(350), Some(0));
        assert_eq!(slot_duration_transition_index(300), Some(1));
        assert_eq!(slot_duration_transition_index(250), Some(2));
        assert_eq!(slot_duration_transition_index(200), Some(3));
        assert_eq!(slot_duration_transition_index(400), None);
        assert_eq!(slot_duration_transition_index(0), None);
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
