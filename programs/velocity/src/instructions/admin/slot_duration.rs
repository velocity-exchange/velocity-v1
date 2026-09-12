//! The cluster slot duration the program measures time against.
//!
//! Solana changes its slot time through a feature gate.
//! [`handle_sync_state_slot_duration`] copies one such transition into `State`.
//! It is permissionless: the feature account, its owner, and the cluster epoch
//! schedule fix every value it accepts.
//!
//! The module also holds the raw byte offsets of the slot-duration fields. The
//! native MM oracle handlers read the clock before Anchor deserializes the
//! account, so they read it by offset.

use super::*;

/// Solana's feature-gate program; every feature account is owned by it.
const FEATURE_GATE_PROGRAM: Pubkey = pubkey!("Feature111111111111111111111111111111111111");
/// The IBRL feature gate whose activation drops the slot to `slot_duration_ms`.
/// `None` for the 400ms baseline (no gate) or any non-schedule value.
fn ibrl_feature_gate(slot_duration_ms: u16) -> Option<Pubkey> {
    Some(match slot_duration_ms {
        350 => pubkey!("iBRL5RuWhw4yqaAZu96RUULHckHTZAoe2b77qaV38JZ"),
        300 => pubkey!("iBRLL3k18HST852F1Mf3Lv83waTNQmmqvKDxvYGwQFL"),
        250 => pubkey!("iBRLMc81UjRa8fn8A6eE8bJTnRbgQoPTynM51akENCV"),
        200 => pubkey!("iBRLjhJnkmDZgNoZRDMW11d8ZV7HvsL3vAyRjZB5npW"),
        _ => return None,
    })
}

/// Target slot duration selected by an IBRL feature account.
fn ibrl_slot_duration_ms(feature_gate: &Pubkey) -> Option<u16> {
    SLOT_DURATION_TRANSITION_MS
        .iter()
        .find(|slot_duration_ms| {
            ibrl_feature_gate(**slot_duration_ms).as_ref() == Some(feature_gate)
        })
        .copied()
}

/// Verify `expected` is the activated IBRL feature gate and return the slot at which
/// its slot-time reduction becomes effective (activation slot + one-epoch warmup).
/// Mirrors the feature-gate account layout: owned by Feature111…, 9 bytes,
/// `data[0] == 1` with the activation slot in little-endian `data[1..9]`. Errors
/// if the account is not activated or malformed. Key and owner constraints live
/// on [`SyncStateSlotDuration`]. It does not require the following epoch to have
/// begun: recording the boundary in advance is the point.
fn feature_gate_effective_slot(
    account: &AccountInfo,
    epoch_schedule: &EpochSchedule,
) -> Result<u64> {
    let data = account.try_borrow_data()?;
    validate!(
        data.len() == 9,
        ErrorCode::DefaultError,
        "feature-gate account has the wrong data length"
    )?;
    // 0 = inactive (Anza has not activated it); anything else = malformed.
    validate!(
        data[0] == 1,
        ErrorCode::DefaultError,
        "IBRL feature gate {} is not activated yet (data[0] = {})",
        account.key,
        data[0]
    )?;
    let activated_at = u64::from_le_bytes(data[1..9].try_into().unwrap());
    let activation_epoch = epoch_schedule.get_epoch(activated_at);
    Ok(epoch_schedule.get_first_slot_in_epoch(activation_epoch.saturating_add(1)))
}

fn validated_slot_duration_archive_update(
    state: &State,
    transition_index: usize,
    effective_slot: u64,
    now_slot: u64,
) -> Result<[u64; 4]> {
    let previous_slot = state.slot_duration_transition_slots[..transition_index]
        .iter()
        .rev()
        .find(|slot| **slot != 0)
        .copied();
    let next_slot = state.slot_duration_transition_slots[transition_index + 1..]
        .iter()
        .find(|slot| **slot != 0)
        .copied();

    validate!(
        previous_slot.is_none_or(|slot| effective_slot >= slot)
            && next_slot.is_none_or(|slot| effective_slot <= slot),
        ErrorCode::DefaultError,
        "IBRL transition slots are not monotonic"
    )?;

    if let Some(highest) = state
        .slot_duration_transition_slots
        .iter()
        .rposition(|slot| *slot != 0)
    {
        if transition_index > highest {
            validate!(
                now_slot >= state.slot_duration_transition_slots[highest],
                ErrorCode::DefaultError,
                "previous synchronized IBRL transition is not effective"
            )?;
        }
    }

    let current_duration_ms = state.slot_clock().slot_duration_at(now_slot).as_ms();
    let mut proposed_slots = state.slot_duration_transition_slots;
    proposed_slots[transition_index] = effective_slot;
    let proposed_clock = SlotClock::from_state_fields(
        proposed_slots,
        state.slot_duration_ms,
        state.pending_slot_duration_ms,
        state.slot_duration_effective_slot,
    );
    validate!(
        proposed_clock.slot_duration_at(now_slot).as_ms() <= current_duration_ms,
        ErrorCode::DefaultError,
        "IBRL synchronization cannot regress the active slot duration"
    )?;
    Ok(proposed_slots)
}

/// Synchronize one IBRL transition from its feature account. Permissionless:
/// all accepted data is fixed by the feature key, feature program ownership,
/// serialized activation slot and the cluster EpochSchedule sysvar.
pub fn handle_sync_state_slot_duration(ctx: Context<SyncStateSlotDuration>) -> Result<()> {
    let now_slot = Clock::get()?.slot;
    let epoch_schedule = EpochSchedule::get()?;
    let feature_account = ctx.accounts.feature_gate.to_account_info();
    let slot_duration_ms =
        ibrl_slot_duration_ms(feature_account.key).ok_or(ErrorCode::DefaultError)?;
    let transition_index =
        slot_duration_transition_index(slot_duration_ms).ok_or(ErrorCode::DefaultError)?;
    let effective_slot = feature_gate_effective_slot(&feature_account, &epoch_schedule)?;
    let mut state = ctx.accounts.state.load_mut()?;

    let recorded = state.slot_duration_transition_slots[transition_index];
    if recorded != 0 {
        validate!(
            recorded == effective_slot,
            ErrorCode::DefaultError,
            "IBRL transition already recorded at {}, feature account resolves to {}",
            recorded,
            effective_slot
        )?;
        return Ok(());
    }

    // A permissionless caller may backfill a missing historical transition,
    // but adding it must never make the clock at the current slot slower. This
    // also closes migration from a legacy state already at 300/250/200ms: sync
    // the currently active (fastest) gate first, then backfill older gates.
    let proposed_slots =
        validated_slot_duration_archive_update(&state, transition_index, effective_slot, now_slot)?;
    let proposed_clock = SlotClock::from_state_fields(
        proposed_slots,
        state.slot_duration_ms,
        state.pending_slot_duration_ms,
        state.slot_duration_effective_slot,
    );
    msg!(
        "slot_duration_ms: synchronized {}ms effective at slot {}",
        slot_duration_ms,
        effective_slot
    );
    state.slot_duration_transition_slots = proposed_slots;

    restage_legacy_slot_duration(&mut state, &proposed_clock, now_slot);

    Ok(())
}

/// Keeps the legacy staging trio coherent for old readers.
///
/// An archive reader applies every boundary itself. An old reader sees only the
/// active duration and the earliest known future boundary, so both are written
/// from the archive. Stale legacy staging never blocks a repair of the archive.
fn restage_legacy_slot_duration(state: &mut State, proposed_clock: &SlotClock, now_slot: u64) {
    state.slot_duration_ms = proposed_clock.slot_duration_at(now_slot).as_ms() as u16;

    let next_future = state
        .slot_duration_transition_slots
        .iter()
        .enumerate()
        .filter(|(_, slot)| **slot > now_slot)
        .min_by_key(|(_, slot)| **slot);

    if let Some((index, next_slot)) = next_future {
        state.pending_slot_duration_ms = SLOT_DURATION_TRANSITION_MS[index];
        state.slot_duration_effective_slot = *next_slot;
    } else {
        state.pending_slot_duration_ms = 0;
        state.slot_duration_effective_slot = 0;
    }
}

/// Byte offset of `State::slot_duration_ms` (u16 LE) from the start of the
/// account data. Same guard as above. The native MM-oracle handlers read it to
/// scale the write-gap and source-age gates.
pub(super) const STATE_SLOT_DURATION_MS_OFFSET: usize = 1506;
/// Byte offset of `State::pending_slot_duration_ms` (u16 LE): the staged next
/// value (`slot_duration_ms` offset + 2).
pub(super) const STATE_PENDING_SLOT_DURATION_MS_OFFSET: usize = 1508;
/// Byte offset of `State::slot_duration_effective_slot` (u64 LE): the slot the
/// staged switch takes effect at (8-aligned, 4 bytes after the pending u16).
pub(super) const STATE_SLOT_DURATION_EFFECTIVE_SLOT_OFFSET: usize = 1512;
/// Byte offset of `State::slot_duration_transition_slots` (`[u64; 4]` LE).
pub(super) const STATE_SLOT_DURATION_TRANSITION_SLOTS_OFFSET: usize = 1520;

/// Read the full clock from a raw, already discriminator-checked State account.
pub(super) fn read_native_state_slot_clock(state_account: &AccountInfo) -> Result<SlotClock> {
    let state = state_account.try_borrow_data()?;
    let read_u16 = |off: usize| -> Result<u16> {
        let bytes: [u8; 2] = state
            .get(off..off + 2)
            .ok_or(ErrorCode::InvalidNativeStateAccount)?
            .try_into()
            .map_err(|_| ErrorCode::InvalidNativeStateAccount)?;
        Ok(u16::from_le_bytes(bytes))
    };
    let base = read_u16(STATE_SLOT_DURATION_MS_OFFSET)?;
    let pending = read_u16(STATE_PENDING_SLOT_DURATION_MS_OFFSET)?;
    let effective_bytes: [u8; 8] = state
        .get(
            STATE_SLOT_DURATION_EFFECTIVE_SLOT_OFFSET
                ..STATE_SLOT_DURATION_EFFECTIVE_SLOT_OFFSET + 8,
        )
        .ok_or(ErrorCode::InvalidNativeStateAccount)?
        .try_into()
        .map_err(|_| ErrorCode::InvalidNativeStateAccount)?;
    let effective_slot = u64::from_le_bytes(effective_bytes);
    let mut transition_slots = [0u64; 4];
    for (i, transition_slot) in transition_slots.iter_mut().enumerate() {
        let off = STATE_SLOT_DURATION_TRANSITION_SLOTS_OFFSET + i * 8;
        let bytes: [u8; 8] = state
            .get(off..off + 8)
            .ok_or(ErrorCode::InvalidNativeStateAccount)?
            .try_into()
            .map_err(|_| ErrorCode::InvalidNativeStateAccount)?;
        *transition_slot = u64::from_le_bytes(bytes);
    }
    Ok(SlotClock::from_state_fields(
        transition_slots,
        base,
        pending,
        effective_slot,
    ))
}

#[derive(Accounts)]
pub struct SyncStateSlotDuration<'info> {
    #[account(mut, seeds = [b"velocity_state".as_ref()], bump)]
    pub state: AccountLoader<'info, State>,
    /// CHECK: constrained to the feature program and the four scheduled IBRL keys;
    /// serialized activation state is validated by the handler.
    #[account(
        owner = FEATURE_GATE_PROGRAM @ ErrorCode::DefaultError,
        constraint = ibrl_slot_duration_ms(&feature_gate.key()).is_some() @ ErrorCode::DefaultError
    )]
    pub feature_gate: UncheckedAccount<'info>,
}

#[cfg(test)]
mod feature_gate_tests {
    //! The permissionless slot duration sync reads the target IBRL gate's
    //! activation slot from its feature account and derives the effective slot
    //! from the cluster `EpochSchedule` (first slot of the epoch after the
    //! activation epoch, mirroring Agave). These pin the account parse, the
    //! epoch arithmetic, and every rejection path; the key/owner checks live as
    //! constraints on `SyncStateSlotDuration`.
    use {
        super::{
            feature_gate_effective_slot, ibrl_feature_gate, ibrl_slot_duration_ms,
            read_native_state_slot_clock, validated_slot_duration_archive_update,
            FEATURE_GATE_PROGRAM,
        },
        crate::state::state::State,
        anchor_lang::prelude::*,
    };

    fn activated(slot: u64) -> [u8; 9] {
        let mut d = [0u8; 9];
        d[0] = 1;
        d[1..9].copy_from_slice(&slot.to_le_bytes());
        d
    }

    fn account<'a>(
        key: &'a Pubkey,
        owner: &'a Pubkey,
        lamports: &'a mut u64,
        data: &'a mut [u8],
    ) -> AccountInfo<'a> {
        AccountInfo::new(key, false, false, lamports, data, owner, false)
    }

    #[test]
    fn gate_pubkeys_only_for_schedule_values() {
        for ms in [350, 300, 250, 200] {
            assert!(ibrl_feature_gate(ms).is_some());
        }
        // baseline, unset, and non-schedule values have no gate
        for ms in [400, 0, 375] {
            assert!(ibrl_feature_gate(ms).is_none());
        }
    }

    #[test]
    fn gate_keys_map_back_to_their_slot_duration() {
        for ms in [350u16, 300, 250, 200] {
            let key = ibrl_feature_gate(ms).unwrap();
            assert_eq!(ibrl_slot_duration_ms(&key), Some(ms));
        }
        assert_eq!(ibrl_slot_duration_ms(&Pubkey::new_unique()), None);
    }

    #[test]
    fn archive_sync_never_regresses_legacy_live_duration() {
        let mut state = State::default();
        state.slot_duration_ms = 200;

        // Starting catch-up with an old slower gate would make the archive
        // authoritative at 350ms and is rejected.
        assert!(validated_slot_duration_archive_update(&state, 0, 100, 200).is_err());

        // Sync the currently active fastest gate first, then historical gates
        // may be backfilled without changing the live 200ms duration.
        let slots = validated_slot_duration_archive_update(&state, 3, 100, 200).unwrap();
        state.slot_duration_transition_slots = slots;
        assert!(validated_slot_duration_archive_update(&state, 0, 10, 200).is_ok());
    }

    #[test]
    fn archive_sync_allows_skips_but_not_early_forward_advances() {
        let state = State::default();
        // No predecessor is required, so an abandoned 350ms gate cannot brick
        // synchronization of a verified 300ms transition.
        assert!(validated_slot_duration_archive_update(&state, 1, 100, 200).is_ok());

        let mut state = State::default();
        state.slot_duration_transition_slots[0] = 300;
        assert!(validated_slot_duration_archive_update(&state, 2, 400, 299).is_err());
        assert!(validated_slot_duration_archive_update(&state, 2, 400, 300).is_ok());
        state.slot_duration_transition_slots[2] = 400;
        assert!(validated_slot_duration_archive_update(&state, 1, 301, 300).is_ok());
        assert!(validated_slot_duration_archive_update(&state, 1, 401, 300).is_err());
    }

    #[test]
    fn effective_slot_is_first_slot_of_the_following_epoch() {
        let key = ibrl_feature_gate(200).unwrap();
        let owner = FEATURE_GATE_PROGRAM;
        let schedule = EpochSchedule::without_warmup();
        let mut lamports = 1u64;

        // mid epoch activation rounds up to the next epoch boundary, never
        // accelerating accounting for the rest of the activation epoch
        let mut data = activated(500_000);
        let acct = account(&key, &owner, &mut lamports, &mut data);
        assert_eq!(
            feature_gate_effective_slot(&acct, &schedule).unwrap(),
            2 * 432_000
        );

        // epoch aligned activation still waits one full epoch
        let mut data = activated(432_000);
        let acct = account(&key, &owner, &mut lamports, &mut data);
        assert_eq!(
            feature_gate_effective_slot(&acct, &schedule).unwrap(),
            2 * 432_000
        );

        let mut data = activated(1_000);
        let acct = account(&key, &owner, &mut lamports, &mut data);
        assert_eq!(
            feature_gate_effective_slot(&acct, &schedule).unwrap(),
            432_000
        );
    }

    #[test]
    fn effective_slot_respects_warmup_epochs() {
        // With warmup the early epochs are shorter than 432,000 slots; the
        // boundary must come from the schedule, not a hardcoded epoch length.
        let key = ibrl_feature_gate(350).unwrap();
        let owner = FEATURE_GATE_PROGRAM;
        let schedule = EpochSchedule::default();
        let mut lamports = 1u64;
        let activation = 1_000u64;
        let mut data = activated(activation);
        let acct = account(&key, &owner, &mut lamports, &mut data);
        let expected =
            schedule.get_first_slot_in_epoch(schedule.get_epoch(activation).saturating_add(1));
        assert_eq!(
            feature_gate_effective_slot(&acct, &schedule).unwrap(),
            expected
        );
    }

    #[test]
    fn inactive_is_rejected() {
        let key = ibrl_feature_gate(200).unwrap();
        let owner = FEATURE_GATE_PROGRAM;
        let mut lamports = 1u64;
        let mut data = [0u8; 9]; // data[0] == 0 => not activated by Anza yet
        let acct = account(&key, &owner, &mut lamports, &mut data);
        assert!(feature_gate_effective_slot(&acct, &EpochSchedule::without_warmup()).is_err());
    }

    #[test]
    fn malformed_flag_is_rejected() {
        let key = ibrl_feature_gate(200).unwrap();
        let owner = FEATURE_GATE_PROGRAM;
        let mut lamports = 1u64;
        let mut data = [2u8; 9]; // data[0] not in {0, 1}
        let acct = account(&key, &owner, &mut lamports, &mut data);
        assert!(feature_gate_effective_slot(&acct, &EpochSchedule::without_warmup()).is_err());
    }

    #[test]
    fn wrong_length_is_rejected() {
        let key = ibrl_feature_gate(200).unwrap();
        let owner = FEATURE_GATE_PROGRAM;
        let mut lamports = 1u64;
        let mut data = [1u8; 8]; // not the 9-byte feature layout
        let acct = account(&key, &owner, &mut lamports, &mut data);
        assert!(feature_gate_effective_slot(&acct, &EpochSchedule::without_warmup()).is_err());
    }

    // Exercises `State::slot_duration_from_account_info` — the validated reader
    // foreign programs (vaults) use to read velocity's live slot duration from a
    // bare AccountInfo, since AccountLoader needs a `'info` borrow they lack.
    #[test]
    fn foreign_state_reader_validates_and_switches() {
        let (key, _) = Pubkey::find_program_address(&[b"velocity_state"], &crate::id());
        let mut lamports = 1u64;
        // 8-byte discriminator + zeroed State, with the staging fields written at
        // their real offsets
        let mut data = vec![0u8; 8 + std::mem::size_of::<State>()];
        data[..8].copy_from_slice(&State::DISCRIMINATOR);
        let put_u16 = |d: &mut [u8], off: usize, v: u16| {
            d[8 + off..8 + off + 2].copy_from_slice(&v.to_le_bytes())
        };
        put_u16(
            &mut data,
            std::mem::offset_of!(State, slot_duration_ms),
            350,
        );
        put_u16(
            &mut data,
            std::mem::offset_of!(State, pending_slot_duration_ms),
            300,
        );
        let eff_off = 8 + std::mem::offset_of!(State, slot_duration_effective_slot);
        data[eff_off..eff_off + 8].copy_from_slice(&1_000u64.to_le_bytes());

        let velocity_id = crate::id();
        {
            let acct = account(&key, &velocity_id, &mut lamports, &mut data);
            // before the effective slot: base 350; at/after: staged 300
            assert_eq!(
                State::slot_duration_from_account_info(&acct, 999)
                    .unwrap()
                    .as_ms(),
                350
            );
            assert_eq!(
                State::slot_duration_from_account_info(&acct, 1_000)
                    .unwrap()
                    .as_ms(),
                300
            );
        }
        // a correctly-owned State-shaped account at the wrong address is rejected
        let wrong_key = Pubkey::new_unique();
        {
            let acct = account(&wrong_key, &velocity_id, &mut lamports, &mut data);
            assert!(State::slot_duration_from_account_info(&acct, 0).is_err());
        }
        // wrong owner is rejected
        let not_velocity = Pubkey::new_unique();
        {
            let acct = account(&key, &not_velocity, &mut lamports, &mut data);
            assert!(State::slot_duration_from_account_info(&acct, 0).is_err());
        }
        // wrong discriminator is rejected
        data[0] ^= 0xff;
        {
            let acct = account(&key, &velocity_id, &mut lamports, &mut data);
            assert!(State::slot_duration_from_account_info(&acct, 0).is_err());
        }
    }

    // The native fast-path reader parses the slot-duration fields by raw byte
    // offset; verify it decodes the staged switch (not merely that the offset
    // constants match `offset_of!`, which the traits test covers).
    #[test]
    fn native_reader_switches_at_effective_slot() {
        let key = Pubkey::new_unique();
        let owner = crate::id();
        let mut lamports = 1u64;
        let mut data = vec![0u8; 8 + std::mem::size_of::<State>()];
        data[..8].copy_from_slice(&State::DISCRIMINATOR);
        let put_u16 = |d: &mut [u8], off: usize, v: u16| {
            d[8 + off..8 + off + 2].copy_from_slice(&v.to_le_bytes())
        };
        put_u16(
            &mut data,
            std::mem::offset_of!(State, slot_duration_ms),
            350,
        );
        put_u16(
            &mut data,
            std::mem::offset_of!(State, pending_slot_duration_ms),
            300,
        );
        let eff_off = 8 + std::mem::offset_of!(State, slot_duration_effective_slot);
        data[eff_off..eff_off + 8].copy_from_slice(&1_000u64.to_le_bytes());
        let acct = account(&key, &owner, &mut lamports, &mut data);
        // before the effective slot: base 350; at/after: staged 300
        assert_eq!(
            read_native_state_slot_clock(&acct)
                .unwrap()
                .slot_duration_at(999)
                .as_ms(),
            350
        );
        assert_eq!(
            read_native_state_slot_clock(&acct)
                .unwrap()
                .slot_duration_at(1_000)
                .as_ms(),
            300
        );
    }

    // Once any transition archive entry exists it is authoritative for both
    // validated readers: the legacy staging fields are ignored and the duration
    // switches exactly at each recorded transition slot.
    #[test]
    fn readers_prefer_the_transition_archive() {
        let (key, _) = Pubkey::find_program_address(&[b"velocity_state"], &crate::id());
        let mut lamports = 1u64;
        let mut data = vec![0u8; 8 + std::mem::size_of::<State>()];
        data[..8].copy_from_slice(&State::DISCRIMINATOR);
        // stale legacy staging fields that must lose to the archive
        let base_off = 8 + std::mem::offset_of!(State, slot_duration_ms);
        data[base_off..base_off + 2].copy_from_slice(&300u16.to_le_bytes());
        let transitions_off = 8 + std::mem::offset_of!(State, slot_duration_transition_slots);
        for (i, transition_slot) in [1_000u64, 2_000, 0, 0].iter().enumerate() {
            let off = transitions_off + i * 8;
            data[off..off + 8].copy_from_slice(&transition_slot.to_le_bytes());
        }

        let velocity_id = crate::id();
        let acct = account(&key, &velocity_id, &mut lamports, &mut data);
        for (slot, expected_ms) in [(999u64, 400u64), (1_000, 350), (1_999, 350), (2_000, 300)] {
            assert_eq!(
                State::slot_duration_from_account_info(&acct, slot)
                    .unwrap()
                    .as_ms(),
                expected_ms
            );
            assert_eq!(
                read_native_state_slot_clock(&acct)
                    .unwrap()
                    .slot_duration_at(slot)
                    .as_ms(),
                expected_ms
            );
        }
    }
}
