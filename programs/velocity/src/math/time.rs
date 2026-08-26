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
//! - [`StoredSlotDuration`] is a compact onchain duration encoded in quanta of
//!   a slot length fixed when the field was introduced. For example,
//!   `StoredSlotDuration<u8, 400>` still occupies one byte, but its type records
//!   that a stored `10` means 4,000ms. Program logic immediately normalizes it
//!   to [`Millis`]; it is never interpreted using the live slot length.
//! - Plain `u64` remains the type of actual slot counts: same-slot
//!   idempotence, blockhash windows, per-order auction snapshots. Genuine
//!   chain-slot logic never touches `Millis`.
//!
//! Legacy admin-set fields keep their compact onchain encoding in units of
//! [`STORED_UNIT_MS`] = 400ms, the historical slot length. Ordinary duration
//! fields express that fact in their [`StoredSlotDuration`] type. Signed fields
//! whose raw values carry sentinel meanings keep their raw integer type and use
//! a purpose-specific decoder such as [`DelayOverride`].
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

use {
    crate::math::safe_math::SafeMath,
    anchor_lang::prelude::*,
    bytemuck::{Pod, Zeroable},
    std::convert::{TryFrom, TryInto},
};

/// Storage encoding quantum for pre-gate duration fields: the historical
/// 400ms slot length. Exists only in the encode/decode of those fields (and in
/// the calibration periods of a few legacy per-slot rates). New compact fields
/// should put their chosen quantum in [`StoredSlotDuration`]'s const parameter
/// rather than referring to this legacy constant implicitly.
pub const STORED_UNIT_MS: u64 = 400;

/// A compact wall-clock duration stored as `T` fixed-slot quanta.
///
/// `SLOT_MS` records the slot length assumed when the field was introduced;
/// it is a storage codec, not the chain's live slot length. The transparent
/// representation preserves the wrapped integer's exact size, alignment, and
/// bytes, so existing accounts remain layout-compatible.
///
/// New fields and APIs should create values with [`Self::try_from_millis`],
/// which rejects durations that are not an exact multiple of `SLOT_MS` or do
/// not fit in `T`. Existing admin instructions intentionally retain their
/// legacy raw-unit wire arguments; [`Self::from_raw_units`] exists for those
/// boundaries and for decoding already-encoded account data.
#[repr(transparent)]
#[derive(
    Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug, Default, AnchorSerialize, AnchorDeserialize,
)]
pub struct StoredSlotDuration<T, const SLOT_MS: u64>(T);

// SAFETY: `StoredSlotDuration` is `repr(transparent)` over its only stored
// field, `T`; the const generic occupies no memory. Therefore every all-zero
// bit pattern valid for a `Zeroable` `T` is also valid for this wrapper.
unsafe impl<T: Zeroable, const SLOT_MS: u64> Zeroable for StoredSlotDuration<T, SLOT_MS> {}

// SAFETY: `repr(transparent)` gives this wrapper exactly `T`'s layout, with no
// additional fields or padding. If `T` is `Pod`, the wrapper has the same valid
// bit patterns and can be read from zero-copy account bytes safely.
unsafe impl<T: Pod, const SLOT_MS: u64> Pod for StoredSlotDuration<T, SLOT_MS> {}

impl<T, const SLOT_MS: u64> StoredSlotDuration<T, SLOT_MS>
where
    T: Copy + TryFrom<u64> + TryInto<u64>,
{
    /// Wrap already-encoded fixed-slot units. Keep this at account/legacy-API
    /// boundaries; duration arithmetic should use [`Self::to_millis`].
    pub const fn from_raw_units(units: T) -> Self {
        assert!(SLOT_MS > 0, "stored slot-duration quantum must be nonzero");
        Self(units)
    }

    /// Encode an exact millisecond duration without changing storage width.
    pub fn try_from_millis(duration: Millis) -> Option<Self> {
        if SLOT_MS == 0 || duration.as_ms() % SLOT_MS != 0 {
            return None;
        }
        T::try_from(duration.as_ms() / SLOT_MS).ok().map(Self)
    }

    /// Decode into the common wall-clock arithmetic type.
    pub fn to_millis(self) -> Millis {
        assert!(SLOT_MS > 0, "stored slot-duration quantum must be nonzero");
        // Signed legacy fields may contain historical negative values. Their
        // failed conversion deliberately normalizes to zero, matching the
        // pre-newtype `value.max(0) as u64` decode.
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

// Runtime and IDL builds deliberately see different Rust type names with the
// same wire bytes:
//
// - Normal program builds use `StoredSlotDuration`, giving Rust the strong
//   fixed-quantum type.
// - Anchor's `idl-build` sees the original integer type. Anchor currently
//   describes the transparent generic wrapper as a defined tuple struct, and
//   its JavaScript Borsh coder would decode it as `{ 0: value }` rather than the
//   primitive number/BN clients already consume.
//
// The complementary `cfg` attributes are required because each alias name may
// have exactly one definition in any build. Remove this split once Anchor's IDL
// and JavaScript coder flatten transparent wrappers to their inner primitive.
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

pub const fn legacy_slot_duration_u8_raw(value: LegacySlotDurationU8) -> u8 {
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

/// The four post baseline regimes, in activation order, matching the IBRL
/// feature gate schedule (400 is the pre upgrade baseline; there is no path
/// back to slower slots, feature gates cannot deactivate). `State` stores the
/// exact first slot of each regime at the matching array index.
pub const SLOT_DURATION_TRANSITION_MS: [u16; 4] = [350, 300, 250, 200];

/// Archive index for a post baseline slot duration.
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

/// The raw `slot_duration_ms` in effect at `now_slot` given the `State` staging
/// fields: the staged `pending_ms` once `now_slot` reaches `effective_slot`,
/// otherwise the current `base_ms`. The single source of truth for the staged
/// switch, shared by `State::active_slot_duration_ms`, the native fast-path
/// reader, and the off-chain mirrors, so they cannot diverge.
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

/// Cluster slot clock reconstructed from the four IBRL transition slots.
///
/// The legacy staging fields remain as a fallback for accounts written by the
/// first slot duration implementation. Once any archive entry exists, the
/// archive is authoritative and elapsed intervals are integrated piecewise.
/// `Default` is the 400ms baseline: all zero fields are exactly
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

    /// Exact elapsed wall clock milliseconds from the start of `start_slot`
    /// to the start of `end_slot`. Every crossed slot duration regime is
    /// integrated separately, matching Agave's transition archive semantics.
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

    pub const fn transition_slots(self) -> [u64; 4] {
        self.transition_slots
    }
}

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
        self.0.safe_div(d.0.max(1)).unwrap_or(0)
    }

    /// This duration expressed in actual slots, rounding up. For
    /// user-protection windows (liquidation ramps, grace periods): the user
    /// never gets less than the intended time.
    pub fn to_slots_ceil(self, d: SlotDuration) -> u64 {
        self.0.safe_div_ceil(d.0.max(1)).unwrap_or(0)
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

/// The storage codec both override fields share: an `i8` in
/// [`STORED_UNIT_MS`] quanta, the same encoding
/// `StoredSlotDuration<i8, STORED_UNIT_MS>` would express. The overrides cannot
/// use that type because their raw values carry sentinels, so the quantum is
/// named here instead. The same migration rule applies: changing it reinterprets
/// every stored byte and needs an admin rewrite, never a type edit alone.
type DelayOverrideStored = StoredSlotDuration<i8, STORED_UNIT_MS>;

impl DelayOverride {
    /// Decode a positive override's raw units. Callers branch on the sentinels
    /// first, so only strictly positive values reach this.
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
        // Anchor's IDL build substitutes primitive aliases for the transparent
        // wrappers. Pin those cfg-only branches so they cannot silently drift
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
    fn transition_durations_are_the_gate_values_only() {
        // pins the synchronizable set: exactly the four IBRL gate values, and
        // neither 0 (unset sentinel) nor 400 (baseline) has a gate.
        assert_eq!(SLOT_DURATION_TRANSITION_MS, [350, 300, 250, 200]);
        assert!(!SLOT_DURATION_TRANSITION_MS.contains(&0));
        assert!(!SLOT_DURATION_TRANSITION_MS.contains(&400));
        // every value is strictly below the 400ms baseline: there is no path
        // back to slower slots.
        for v in SLOT_DURATION_TRANSITION_MS {
            assert!((v as u64) < SlotDuration::BASELINE.as_ms());
        }
    }

    #[test]
    fn slot_clock_without_history_falls_back_to_legacy_staging() {
        // legacy staging fields: base 400, pending 350 effective at 1_000
        let clock = SlotClock::from_state_fields([0; 4], 400, 350, 1_000);
        assert!(!clock.has_transition_history());
        assert_eq!(clock.slot_duration_at(999).as_ms(), 400);
        assert_eq!(clock.slot_duration_at(1_000).as_ms(), 350);
        // no history: the whole delta is priced at the end slot duration
        assert_eq!(clock.elapsed(0, 10).as_ms(), 10 * 400);
        assert_eq!(clock.elapsed(1_000, 1_010).as_ms(), 10 * 350);
    }

    #[test]
    fn slot_clock_archive_is_authoritative_over_legacy_fields() {
        let clock = SlotClock::from_state_fields([1_000, 0, 0, 0], 200, 250, 5);
        assert!(clock.has_transition_history());
        // pre transition is the 400ms baseline regardless of stale legacy fields
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
        // fully inside one regime
        assert_eq!(clock.elapsed(0, 10).as_ms(), 10 * 400);
        assert_eq!(clock.elapsed(4_000, 4_010).as_ms(), 10 * 200);
        // spanning one transition: 10 slots at 400ms + 10 at 350ms
        assert_eq!(clock.elapsed(990, 1_010).as_ms(), 10 * 400 + 10 * 350);
        // spanning every transition
        assert_eq!(
            clock.elapsed(0, 5_000).as_ms(),
            1_000 * 400 + 1_000 * 350 + 1_000 * 300 + 1_000 * 250 + 1_000 * 200
        );
        // degenerate intervals are zero
        assert_eq!(clock.elapsed(10, 10), Millis::ZERO);
        assert_eq!(clock.elapsed(20, 10), Millis::ZERO);
        // the delta form anchors the interval at its end slot
        assert_eq!(
            clock.elapsed_slot_delta(20, 1_010).as_ms(),
            10 * 400 + 10 * 350
        );
        // a delta larger than the end slot saturates to slot zero
        assert_eq!(clock.elapsed_slot_delta(100, 50).as_ms(), 50 * 400);
    }

    #[test]
    fn slot_clock_with_partial_archive_stays_on_the_last_synced_regime() {
        // only the first two transitions synchronized so far
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
