//! Tests for the native MM oracle handlers.

use super::*;

#[cfg(test)]
mod native_auth_tests {
    //! Negative tests for the pre-Anchor native dispatch authentication on
    //! `handle_update_mm_oracle_native`. These run under `cargo test` (default
    //! features, no `anchor-test`), so the signer check is compiled in. The
    //! structural account checks are always compiled in regardless of feature.
    use {
        super::*,
        crate::{
            create_anchor_account_info,
            math::time::SlotDuration,
            state::{
                perp_market::PerpMarket,
                state::{FeatureBitFlags, State},
            },
            test_utils::get_anchor_account_bytes,
        },
        anchor_lang::prelude::{AccountInfo, Pubkey},
    };

    // mm-oracle payload: 8-byte price + 8-byte sequence id + 8-byte source slot
    // (price and sequence non-zero, source slot matching the slot the tests
    // drive, so the happy path would proceed past every early-out check).
    fn mm_payload() -> [u8; 24] {
        let mut d = [0u8; 24];
        d[0..8].copy_from_slice(&100_i64.to_le_bytes());
        d[8..16].copy_from_slice(&1_u64.to_le_bytes());
        d[16..24].copy_from_slice(&100_u64.to_le_bytes());
        d
    }

    fn signer_info<'a>(
        key: &'a Pubkey,
        is_signer: bool,
        lamports: &'a mut u64,
        data: &'a mut [u8],
        owner: &'a Pubkey,
    ) -> AccountInfo<'a> {
        AccountInfo::new(key, is_signer, false, lamports, data, owner, false)
    }

    #[test]
    fn mm_oracle_native_rejects_forged_state() {
        // State account with the attacker's key at the hot-key field but owned by
        // a foreign program — the pre-fix bug authenticated against exactly this.
        let attacker = Pubkey::new_unique();
        let mut state = State::default();
        state.hot_mm_oracle_crank = attacker;
        state.feature_bit_flags = FeatureBitFlags::MmOracleUpdate as u8;
        let mut state_bytes = get_anchor_account_bytes(&mut state);
        let foreign_owner = Pubkey::new_unique();
        let state_key = Pubkey::new_unique();
        let mut state_lamports = 0u64;
        let forged_state = AccountInfo::new(
            &state_key,
            false,
            false,
            &mut state_lamports,
            &mut state_bytes[..],
            &foreign_owner, // NOT crate::ID
            false,
        );

        let mut perp_market = PerpMarket::default();
        create_anchor_account_info!(perp_market, PerpMarket, perp_market_info);

        let mut sig_lamports = 0u64;
        let mut sig_data: [u8; 0] = [];
        let sig_owner = Pubkey::new_unique();
        let signer = signer_info(
            &attacker,
            true,
            &mut sig_lamports,
            &mut sig_data,
            &sig_owner,
        );

        let accounts = [perp_market_info, signer, forged_state];
        let err = update_mm_oracle(&accounts, &mm_payload(), 100).unwrap_err();
        assert_eq!(err, ErrorCode::InvalidNativeStateAccount.into());
    }

    #[test]
    fn mm_oracle_native_rejects_non_perp_market_in_market_slot() {
        // Genuine state, but the "market" slot holds a non-PerpMarket account
        // (here a second State) — the pre-fix bug bytemuck-cast it blindly.
        let hot_key = Pubkey::new_unique();
        let mut state = State::default();
        state.hot_mm_oracle_crank = hot_key;
        state.feature_bit_flags = FeatureBitFlags::MmOracleUpdate as u8;
        create_anchor_account_info!(state, State, state_info);

        let mut not_a_market = State::default();
        create_anchor_account_info!(not_a_market, State, not_a_market_info);

        let mut sig_lamports = 0u64;
        let mut sig_data: [u8; 0] = [];
        let sig_owner = Pubkey::new_unique();
        let signer = signer_info(&hot_key, true, &mut sig_lamports, &mut sig_data, &sig_owner);

        let accounts = [not_a_market_info, signer, state_info];
        let err = update_mm_oracle(&accounts, &mm_payload(), 100).unwrap_err();
        assert_eq!(err, ErrorCode::InvalidNativePerpMarketAccount.into());
    }

    #[test]
    fn mm_oracle_native_rejects_unauthorized_signer() {
        // Genuine state + market, but the signer is not the configured hot key.
        let hot_key = Pubkey::new_unique();
        let mut state = State::default();
        state.hot_mm_oracle_crank = hot_key;
        state.feature_bit_flags = FeatureBitFlags::MmOracleUpdate as u8;
        create_anchor_account_info!(state, State, state_info);

        let mut perp_market = PerpMarket::default();
        create_anchor_account_info!(perp_market, PerpMarket, perp_market_info);

        let attacker = Pubkey::new_unique();
        let mut sig_lamports = 0u64;
        let mut sig_data: [u8; 0] = [];
        let sig_owner = Pubkey::new_unique();
        let signer = signer_info(
            &attacker,
            true,
            &mut sig_lamports,
            &mut sig_data,
            &sig_owner,
        );

        let accounts = [perp_market_info, signer, state_info];
        let err = update_mm_oracle(&accounts, &mm_payload(), 100).unwrap_err();
        assert_eq!(err, ErrorCode::Unauthorized.into());
    }

    // malformed input must error, not panic

    /// The handler runs before Anchor, so a malformed instruction reaches it
    /// verbatim. Short account lists and short payloads used to panic on the
    /// indexing, which aborts the transaction with no identifiable error.
    #[test]
    fn mm_oracle_native_rejects_malformed_shape_without_panicking() {
        let hot_key = Pubkey::new_unique();
        let mut state = State::default();
        state.hot_mm_oracle_crank = hot_key;
        state.feature_bit_flags = FeatureBitFlags::MmOracleUpdate as u8;
        create_anchor_account_info!(state, State, state_info);

        let mut perp_market = PerpMarket::default();
        create_anchor_account_info!(perp_market, PerpMarket, perp_market_info);

        let mut sig_lamports = 0u64;
        let mut sig_data: [u8; 0] = [];
        let sig_owner = Pubkey::new_unique();
        let signer = signer_info(&hot_key, true, &mut sig_lamports, &mut sig_data, &sig_owner);

        let accounts = [perp_market_info, signer, state_info];

        // Too few accounts. Zero of them, so nothing can be indexed at all.
        let err = update_mm_oracle(&[], &mm_payload(), 100).unwrap_err();
        assert_eq!(err, ErrorCode::InvalidNativeInstructionData.into());

        // Two accounts where three are required: the state slot is `accounts[2]`.
        let err = update_mm_oracle(&accounts[..2], &mm_payload(), 100).unwrap_err();
        assert_eq!(err, ErrorCode::InvalidNativeInstructionData.into());

        // Payload shorter than the 24 bytes the handler slices.
        for len in 0..24usize {
            let err = update_mm_oracle(&accounts, &mm_payload()[..len], 100).unwrap_err();
            assert_eq!(
                err,
                ErrorCode::InvalidNativeInstructionData.into(),
                "payload of {len} bytes did not return a clean error"
            );
        }
    }

    #[test]
    fn mm_oracle_native_kill_switch_returns_typed_error() {
        // Previously an `assert!`, i.e. a panic surfacing as "Program failed to
        // complete" with no way to tell it from any other abort.
        let hot_key = Pubkey::new_unique();
        let mut state = State::default();
        state.hot_mm_oracle_crank = hot_key;
        state.feature_bit_flags = 0; // MmOracleUpdate clear
        create_anchor_account_info!(state, State, state_info);

        let mut perp_market = PerpMarket::default();
        create_anchor_account_info!(perp_market, PerpMarket, perp_market_info);

        let mut sig_lamports = 0u64;
        let mut sig_data: [u8; 0] = [];
        let sig_owner = Pubkey::new_unique();
        let signer = signer_info(&hot_key, true, &mut sig_lamports, &mut sig_data, &sig_owner);

        let accounts = [perp_market_info, signer, state_info];
        let err = update_mm_oracle(&accounts, &mm_payload(), 100).unwrap_err();
        assert_eq!(err, ErrorCode::MmOracleUpdateDisabled.into());
    }

    // step cap clamps instead of freezing

    /// Drives the handler repeatedly against a fixed target price and returns
    /// the stored price after each accepted write.
    fn walk_price(start: i64, target: i64, writes: usize) -> Vec<i64> {
        let hot_key = Pubkey::new_unique();
        let mut state = State::default();
        state.hot_mm_oracle_crank = hot_key;
        state.feature_bit_flags = FeatureBitFlags::MmOracleUpdate as u8;
        create_anchor_account_info!(state, State, state_info);

        let mut perp_market = PerpMarket::default();
        perp_market.market_stats.mm_oracle_price = start;
        perp_market.market_stats.mm_oracle_slot = 0;
        perp_market.market_stats.mm_oracle_sequence_id = 0;
        create_anchor_account_info!(perp_market, PerpMarket, perp_market_info);

        let mut sig_lamports = 0u64;
        let mut sig_data: [u8; 0] = [];
        let sig_owner = Pubkey::new_unique();
        let signer = signer_info(&hot_key, true, &mut sig_lamports, &mut sig_data, &sig_owner);

        let accounts = [perp_market_info.clone(), signer, state_info];

        let mut observed = Vec::with_capacity(writes);
        for i in 0..writes {
            // Advance the slot past the MM-oracle write gap for each write.
            let gap =
                crate::math::constants::MM_ORACLE_MIN_WRITE_GAP.to_slots(SlotDuration::BASELINE);
            let slot = ((i as u64) + 1) * (gap + 1);

            let mut payload = [0u8; 24];
            payload[0..8].copy_from_slice(&target.to_le_bytes());
            payload[8..16].copy_from_slice(&((i as u64) + 1).to_le_bytes());
            payload[16..24].copy_from_slice(&slot.to_le_bytes()); // fresh source
            update_mm_oracle(&accounts, &payload, slot).unwrap();

            let data = perp_market_info.try_borrow_data().unwrap();
            let market: &PerpMarket =
                bytemuck::from_bytes(&data[8..8 + std::mem::size_of::<PerpMarket>()]);
            observed.push(market.market_stats.mm_oracle_price);
        }
        observed
    }

    /// A move beyond the cap is written at the cap and keeps closing the gap.
    /// Rejecting it, as the handler used to, left the stored price where it was,
    /// so the next update was still beyond the cap against the same stale value
    /// and the oracle never recovered.
    #[test]
    fn mm_oracle_native_clamps_and_converges_upward() {
        let start = 1_000_000i64;
        let target = start * 105 / 100; // 5% away, cap is 1%

        let observed = walk_price(start, target, 6);

        assert_eq!(observed[0], 1_010_000, "first write must land at the cap");
        // Monotonic toward the target, and strictly moving until it arrives.
        for pair in observed.windows(2) {
            assert!(pair[1] >= pair[0], "price moved backwards: {observed:?}");
            assert!(
                pair[0] == target || pair[1] > pair[0],
                "price stalled before reaching the target: {observed:?}"
            );
        }
        assert_eq!(
            *observed.last().unwrap(),
            target,
            "must converge on the target: {observed:?}"
        );
        assert!(
            observed.iter().all(|p| *p <= target),
            "must never overshoot: {observed:?}"
        );
    }

    /// The cap is symmetric, so the same must hold downward.
    #[test]
    fn mm_oracle_native_clamps_and_converges_downward() {
        let start = 1_000_000i64;
        let target = start * 95 / 100;

        let observed = walk_price(start, target, 6);

        assert_eq!(observed[0], 990_000, "first write must land at the cap");
        for pair in observed.windows(2) {
            assert!(pair[1] <= pair[0], "price moved backwards: {observed:?}");
            assert!(
                pair[0] == target || pair[1] < pair[0],
                "price stalled before reaching the target: {observed:?}"
            );
        }
        assert_eq!(*observed.last().unwrap(), target);
        assert!(observed.iter().all(|p| *p >= target));
    }

    /// A move inside the cap is written verbatim, unchanged from before.
    #[test]
    fn mm_oracle_native_leaves_in_range_steps_alone() {
        let start = 1_000_000i64;
        let target = start + 5_000; // 0.5%, inside the 1% cap
        assert_eq!(walk_price(start, target, 1)[0], target);
    }

    /// The cap is a percentage, so integer division rounds it to zero for very
    /// small prices. Floored at one unit so those markets still make progress
    /// rather than reintroducing the freeze this fix removes.
    #[test]
    fn mm_oracle_native_makes_progress_at_prices_below_the_cap_resolution() {
        // 1% of 50 rounds to 0.
        let observed = walk_price(50, 60, 3);
        assert_eq!(observed, vec![51, 52, 53], "must advance by at least one");
    }

    /// Any non-positive price is a hard error, not just exact zero. Rejecting
    /// only zero left a hole once the step cap clamped instead of skipping: a
    /// negative target was clamped against the stored price and written (e.g.
    /// -1 against 1,000,000 landed as 990,000, consuming the sequence id), and
    /// repeated negatives could walk the price to zero, resetting the bootstrap
    /// path and with it the step cap. At bootstrap (stored price 0) a negative
    /// was written verbatim.
    #[test]
    fn mm_oracle_native_rejects_non_positive_price() {
        // (stored price, incoming price)
        let cases: [(i64, i64); 5] = [
            (1_000_000, 0),
            (1_000_000, -1),
            (1_000_000, -1_000_000),
            (0, -1), // bootstrap: previously written verbatim
            (0, i64::MIN),
        ];

        for (stored, incoming) in cases {
            let hot_key = Pubkey::new_unique();
            let mut state = State::default();
            state.hot_mm_oracle_crank = hot_key;
            state.feature_bit_flags = FeatureBitFlags::MmOracleUpdate as u8;
            create_anchor_account_info!(state, State, state_info);

            let mut perp_market = PerpMarket::default();
            perp_market.market_stats.mm_oracle_price = stored;
            create_anchor_account_info!(perp_market, PerpMarket, perp_market_info);

            let mut sig_lamports = 0u64;
            let mut sig_data: [u8; 0] = [];
            let sig_owner = Pubkey::new_unique();
            let signer = signer_info(&hot_key, true, &mut sig_lamports, &mut sig_data, &sig_owner);

            let mut payload = [0u8; 24];
            payload[0..8].copy_from_slice(&incoming.to_le_bytes());
            payload[8..16].copy_from_slice(&1u64.to_le_bytes());
            payload[16..24].copy_from_slice(&100u64.to_le_bytes());

            let accounts = [perp_market_info.clone(), signer, state_info];
            let err = update_mm_oracle(&accounts, &payload, 100).unwrap_err();
            assert_eq!(
                err,
                ErrorCode::DefaultError.into(),
                "price {incoming} against stored {stored} must be a hard error"
            );

            let data = perp_market_info.try_borrow_data().unwrap();
            let market: &PerpMarket =
                bytemuck::from_bytes(&data[8..8 + std::mem::size_of::<PerpMarket>()]);
            assert_eq!(
                market.market_stats.mm_oracle_price, stored,
                "stored price must be untouched"
            );
            assert_eq!(market.market_stats.mm_oracle_sequence_id, 0);
        }
    }
}

#[cfg(test)]
mod native_batch_tests {
    //! Tests for `handle_update_mm_oracle_batch_native` (native dispatch opcode 2).
    //!
    //! Three halves:
    //!
    //! 1. Negative tests mirroring `native_auth_tests`, because the batch handler
    //!    re-implements the same pre-Anchor authentication and must not regress
    //!    any of the findings that motivated `auth::require_native_account`
    //!    (forged state account, blind `bytemuck` cast of an untyped account,
    //!    signer bypass), plus the framing checks that only a variable-length
    //!    handler needs. There is no clock to forge: the slot comes from the
    //!    Clock sysvar syscall, and tests drive it via the split handler bodies.
    //! 2. Functional tests pinning every accept and skip decision, asserted on
    //!    the returned reject bitmask as well as on written state.
    //! 3. An equivalence test against `handle_update_mm_oracle_native` so the two
    //!    copies of the gating logic cannot drift apart silently.
    //!
    //! These run under `cargo test` with default features, so the hot-key signer
    //! check is compiled in. The structural checks are compiled in regardless.
    use {
        super::*,
        crate::{
            create_anchor_account_info,
            math::time::SlotDuration,
            state::{
                perp_market::PerpMarket,
                state::{FeatureBitFlags, State},
            },
            test_utils::get_anchor_account_bytes,
        },
        anchor_lang::prelude::{AccountInfo, Pubkey},
    };

    /// `(price, slot, sequence_id)` triple of a market's stored MM oracle fields.
    type MmStats = (i64, u64, u64);

    const BASE_PRICE: i64 = 1_000_000;
    /// Slot every fixture's clock reports, chosen far enough above the fixtures'
    /// stored slots that the gap check is never the accidental reason a test
    /// passes.
    const SLOT: u64 = 100;

    /// Binds a valid prologue for the batch handler: a program-owned `State`
    /// with the kill switch on and `$hot` as the MM-oracle crank key, and
    /// `$hot` as a signing account. The slot is passed straight to the split
    /// handler bodies; there is no clock account since the handlers read the
    /// Clock sysvar via syscall.
    macro_rules! valid_prologue {
        ($hot:ident, $state:ident, $signer:ident) => {
            let mut state_struct = State::default();
            state_struct.hot_mm_oracle_crank = $hot;
            state_struct.feature_bit_flags = FeatureBitFlags::MmOracleUpdate as u8;
            create_anchor_account_info!(state_struct, State, $state);

            let mut sig_lamports = 0u64;
            let mut sig_data: [u8; 0] = [];
            let sig_owner = Pubkey::new_unique();
            let $signer = AccountInfo::new(
                &$hot,
                true,
                false,
                &mut sig_lamports,
                &mut sig_data,
                &sig_owner,
                false,
            );
        };
    }

    /// Binds `$name` to a writable, program-owned `PerpMarket` account carrying
    /// `market_index = $index` and the MM oracle fields in `$stats`.
    macro_rules! market_account {
        ($index:expr, $stats:expr, $name:ident) => {
            let market_key = Pubkey::new_unique();
            let mut market_struct = market_with($index, $stats);
            create_anchor_account_info!(market_struct, &market_key, PerpMarket, $name);
        };
    }

    /// Batch payload: count byte then `(market_index, price, sequence_id,
    /// source_slot)` per entry, little-endian, matching
    /// `MM_ORACLE_BATCH_ENTRY_LEN`.
    fn batch_payload_with_source(entries: &[(u16, i64, u64, u64)]) -> Vec<u8> {
        let mut data = Vec::with_capacity(1 + entries.len() * MM_ORACLE_BATCH_ENTRY_LEN);
        data.push(entries.len() as u8);
        for (market_index, price, sequence_id, source_slot) in entries {
            data.extend_from_slice(&market_index.to_le_bytes());
            data.extend_from_slice(&price.to_le_bytes());
            data.extend_from_slice(&sequence_id.to_le_bytes());
            data.extend_from_slice(&source_slot.to_le_bytes());
        }
        data
    }

    /// `batch_payload_with_source` with every entry's source slot pinned to
    /// `SLOT`, i.e. observed in the landing slot, so the source-age gate is
    /// never the accidental reason a test passes.
    fn batch_payload(entries: &[(u16, i64, u64)]) -> Vec<u8> {
        let with_source: Vec<(u16, i64, u64, u64)> = entries
            .iter()
            .map(|&(market_index, price, sequence_id)| (market_index, price, sequence_id, SLOT))
            .collect();
        batch_payload_with_source(&with_source)
    }

    /// Payload for the single-market handler (opcode 0): price, sequence id,
    /// source slot; no count byte and no market index.
    fn single_payload_with_source(price: i64, sequence_id: u64, source_slot: u64) -> [u8; 24] {
        let mut data = [0u8; 24];
        data[0..8].copy_from_slice(&price.to_le_bytes());
        data[8..16].copy_from_slice(&sequence_id.to_le_bytes());
        data[16..24].copy_from_slice(&source_slot.to_le_bytes());
        data
    }

    /// `single_payload_with_source` with the source slot pinned to `SLOT`.
    fn single_payload(price: i64, sequence_id: u64) -> [u8; 24] {
        single_payload_with_source(price, sequence_id, SLOT)
    }

    fn read_stats(info: &AccountInfo) -> MmStats {
        let data = info.try_borrow_data().unwrap();
        let market: &PerpMarket =
            bytemuck::from_bytes(&data[8..8 + std::mem::size_of::<PerpMarket>()]);
        (
            market.market_stats.mm_oracle_price,
            market.market_stats.mm_oracle_slot,
            market.market_stats.mm_oracle_sequence_id,
        )
    }

    fn market_with(market_index: u16, stats: MmStats) -> PerpMarket {
        let mut market = PerpMarket {
            market_index,
            ..PerpMarket::default()
        };
        market.market_stats.mm_oracle_price = stats.0;
        market.market_stats.mm_oracle_slot = stats.1;
        market.market_stats.mm_oracle_sequence_id = stats.2;
        market
    }

    // framing

    /// Framing is validated before any account is touched, so these cases need
    /// no account fixtures at all. Passing an empty `accounts` slice also proves
    /// the handler never indexes an account before checking lengths.
    #[test]
    fn batch_rejects_malformed_framing() {
        let cases: Vec<(&str, Vec<u8>)> = vec![
            ("empty payload", vec![]),
            ("zero count", vec![0u8]),
            ("count above max", {
                let mut d = vec![(MM_ORACLE_BATCH_MAX_MARKETS + 1) as u8];
                d.extend(std::iter::repeat_n(
                    0u8,
                    (MM_ORACLE_BATCH_MAX_MARKETS + 1) * MM_ORACLE_BATCH_ENTRY_LEN,
                ));
                d
            }),
            ("count says 2, one entry supplied", {
                let mut d = batch_payload(&[(0, BASE_PRICE, 1)]);
                d[0] = 2;
                d
            }),
            ("count says 1, trailing byte", {
                let mut d = batch_payload(&[(0, BASE_PRICE, 1)]);
                d.push(0);
                d
            }),
            ("entry truncated by one byte", {
                let mut d = batch_payload(&[(0, BASE_PRICE, 1)]);
                d.pop();
                d
            }),
        ];

        for (label, data) in cases {
            let err = update_mm_oracle_batch(&[], &data, SLOT).unwrap_err();
            assert_eq!(
                err,
                ErrorCode::InvalidNativeInstructionData.into(),
                "framing case did not reject: {label}"
            );
        }
    }

    #[test]
    fn batch_rejects_too_few_accounts_for_declared_count() {
        let hot_key = Pubkey::new_unique();
        valid_prologue!(hot_key, state_info, signer);
        market_account!(0, (0, 0, 0), market_info);

        // Declares two markets, supplies one.
        let payload = batch_payload(&[(0, BASE_PRICE, 1), (1, BASE_PRICE, 2)]);
        let accounts = [signer, state_info, market_info];
        let err = update_mm_oracle_batch(&accounts, &payload, SLOT).unwrap_err();
        assert_eq!(err, ErrorCode::InvalidNativeInstructionData.into());
    }

    // authentication

    #[test]
    fn batch_rejects_forged_state() {
        // State carrying the attacker's key at the hot-key offset, but owned by a
        // foreign program. This is the exact shape of the original finding.
        let attacker = Pubkey::new_unique();
        let mut state_struct = State::default();
        state_struct.hot_mm_oracle_crank = attacker;
        state_struct.feature_bit_flags = FeatureBitFlags::MmOracleUpdate as u8;
        let mut state_bytes = get_anchor_account_bytes(&mut state_struct);
        let foreign_owner = Pubkey::new_unique();
        let state_key = Pubkey::new_unique();
        let mut state_lamports = 0u64;
        let forged_state = AccountInfo::new(
            &state_key,
            false,
            false,
            &mut state_lamports,
            &mut state_bytes[..],
            &foreign_owner, // NOT crate::ID
            false,
        );

        market_account!(0, (0, 0, 0), market_info);

        let mut sig_lamports = 0u64;
        let mut sig_data: [u8; 0] = [];
        let sig_owner = Pubkey::new_unique();
        let signer = AccountInfo::new(
            &attacker,
            true,
            false,
            &mut sig_lamports,
            &mut sig_data,
            &sig_owner,
            false,
        );

        let accounts = [signer, forged_state, market_info];
        let err = update_mm_oracle_batch(&accounts, &batch_payload(&[(0, BASE_PRICE, 1)]), SLOT)
            .unwrap_err();
        assert_eq!(err, ErrorCode::InvalidNativeStateAccount.into());
    }

    #[test]
    fn batch_rejects_non_state_account_in_state_slot() {
        // Program-owned, but a PerpMarket rather than State: the discriminator
        // arm of require_native_account, which the forged-state test (foreign
        // owner) does not reach.
        let hot_key = Pubkey::new_unique();
        market_account!(0, (0, 0, 0), not_a_state_info);
        market_account!(0, (0, 0, 0), market_info);

        let mut sig_lamports = 0u64;
        let mut sig_data: [u8; 0] = [];
        let sig_owner = Pubkey::new_unique();
        let signer = AccountInfo::new(
            &hot_key,
            true,
            false,
            &mut sig_lamports,
            &mut sig_data,
            &sig_owner,
            false,
        );

        let accounts = [signer, not_a_state_info, market_info];
        let err = update_mm_oracle_batch(&accounts, &batch_payload(&[(0, BASE_PRICE, 1)]), SLOT)
            .unwrap_err();
        assert_eq!(err, ErrorCode::InvalidNativeStateAccount.into());
    }

    #[test]
    fn batch_rejects_non_perp_market_in_market_slot() {
        // Genuine state, but a second State account sits in the market region.
        // Without the discriminator check this would be `bytemuck`-cast blindly.
        let hot_key = Pubkey::new_unique();
        valid_prologue!(hot_key, state_info, signer);

        let mut not_a_market = State::default();
        create_anchor_account_info!(not_a_market, State, not_a_market_info);

        let accounts = [signer, state_info, not_a_market_info];
        let err = update_mm_oracle_batch(&accounts, &batch_payload(&[(0, BASE_PRICE, 1)]), SLOT)
            .unwrap_err();
        assert_eq!(err, ErrorCode::InvalidNativePerpMarketAccount.into());
    }

    #[test]
    fn batch_rejects_foreign_owned_market() {
        // Correct PerpMarket discriminator but owned by another program, i.e. the
        // owner arm of require_native_account on the market side. Anyone can
        // create an account with arbitrary bytes under a program they control.
        let hot_key = Pubkey::new_unique();
        valid_prologue!(hot_key, state_info, signer);

        let mut market_struct = market_with(0, (0, 0, 0));
        let mut market_bytes = get_anchor_account_bytes(&mut market_struct);
        let foreign_owner = Pubkey::new_unique();
        let market_key = Pubkey::new_unique();
        let mut market_lamports = 0u64;
        let foreign_market = AccountInfo::new(
            &market_key,
            false,
            true,
            &mut market_lamports,
            &mut market_bytes[..],
            &foreign_owner, // NOT crate::ID
            false,
        );

        let accounts = [signer, state_info, foreign_market];
        let err = update_mm_oracle_batch(&accounts, &batch_payload(&[(0, BASE_PRICE, 1)]), SLOT)
            .unwrap_err();
        assert_eq!(err, ErrorCode::InvalidNativePerpMarketAccount.into());
    }

    #[test]
    fn batch_rejects_truncated_market_account() {
        // Program-owned and correctly discriminated, but too short to hold a
        // PerpMarket. Opcode 0 panics on this input; the batch must not.
        let hot_key = Pubkey::new_unique();
        valid_prologue!(hot_key, state_info, signer);

        let mut truncated = [0u8; 64];
        truncated[..8].copy_from_slice(PerpMarket::DISCRIMINATOR);
        let market_key = Pubkey::new_unique();
        let mut market_lamports = 0u64;
        let market_owner = crate::ID;
        let truncated_market = AccountInfo::new(
            &market_key,
            false,
            true,
            &mut market_lamports,
            &mut truncated,
            &market_owner,
            false,
        );

        let accounts = [signer, state_info, truncated_market];
        let err = update_mm_oracle_batch(&accounts, &batch_payload(&[(0, BASE_PRICE, 1)]), SLOT)
            .unwrap_err();
        assert_eq!(err, ErrorCode::InvalidNativePerpMarketAccount.into());
    }

    /// A batch must authenticate every market, not just the first. Market 0 is
    /// genuine and market 1 is not, so the error can only come from index 1.
    #[test]
    fn batch_authenticates_every_market_not_just_the_first() {
        let hot_key = Pubkey::new_unique();
        valid_prologue!(hot_key, state_info, signer);
        market_account!(0, (0, 0, 0), market_info);

        let mut not_a_market = State::default();
        create_anchor_account_info!(not_a_market, State, not_a_market_info);

        let payload = batch_payload(&[(0, BASE_PRICE, 1), (1, BASE_PRICE, 2)]);
        let accounts = [signer, state_info, market_info, not_a_market_info];
        let err = update_mm_oracle_batch(&accounts, &payload, SLOT).unwrap_err();
        assert_eq!(err, ErrorCode::InvalidNativePerpMarketAccount.into());
    }

    /// The handler is not internally transactional: entries before the failing
    /// one have already been written when the error returns. That is safe only
    /// because the runtime discards every account mutation when an instruction
    /// returns `Err`. Pinned here so the assumption is explicit rather than
    /// incidental.
    #[test]
    fn batch_is_not_internally_transactional() {
        let hot_key = Pubkey::new_unique();
        valid_prologue!(hot_key, state_info, signer);
        market_account!(0, (0, 0, 0), market_info);

        let mut not_a_market = State::default();
        create_anchor_account_info!(not_a_market, State, not_a_market_info);

        let payload = batch_payload(&[(0, BASE_PRICE, 1), (1, BASE_PRICE, 2)]);
        let accounts = [signer, state_info, market_info.clone(), not_a_market_info];
        assert!(update_mm_oracle_batch(&accounts, &payload, SLOT).is_err());
        assert_eq!(
            read_stats(&market_info),
            (BASE_PRICE, SLOT, 1),
            "entry 0 is written before entry 1 fails; the runtime, not the \
             handler, is what rolls this back"
        );
    }

    #[test]
    fn batch_rejects_market_index_mismatch() {
        // The account list and the payload disagree about which market entry 0
        // is for. Without the market-index field this would silently write one
        // market's price onto another.
        let hot_key = Pubkey::new_unique();
        valid_prologue!(hot_key, state_info, signer);
        market_account!(7, (0, 0, 0), market_info);

        let payload = batch_payload(&[(3, BASE_PRICE, 1)]); // account says 7
        let accounts = [signer, state_info, market_info.clone()];
        let err = update_mm_oracle_batch(&accounts, &payload, SLOT).unwrap_err();
        assert_eq!(err, ErrorCode::InvalidNativePerpMarketAccount.into());
        assert_eq!(
            read_stats(&market_info),
            (0, 0, 0),
            "nothing may be written"
        );
    }

    #[test]
    fn batch_rejects_read_only_market() {
        let hot_key = Pubkey::new_unique();
        valid_prologue!(hot_key, state_info, signer);

        let mut market_struct = market_with(0, (0, 0, 0));
        let mut market_bytes = get_anchor_account_bytes(&mut market_struct);
        let market_key = Pubkey::new_unique();
        let mut market_lamports = 0u64;
        let market_owner = crate::ID;
        let read_only_market = AccountInfo::new(
            &market_key,
            false,
            false, // not writable
            &mut market_lamports,
            &mut market_bytes[..],
            &market_owner,
            false,
        );

        let accounts = [signer, state_info, read_only_market];
        let err = update_mm_oracle_batch(&accounts, &batch_payload(&[(0, BASE_PRICE, 1)]), SLOT)
            .unwrap_err();
        assert_eq!(err, ErrorCode::InvalidNativePerpMarketAccount.into());
    }

    #[test]
    fn batch_rejects_unauthorized_and_non_signing_hot_key() {
        // Two cases: the wrong key that signs, and the right key that does not.
        for wrong_key in [true, false] {
            let hot_key = Pubkey::new_unique();
            let attacker = Pubkey::new_unique();
            let mut state_struct = State::default();
            state_struct.hot_mm_oracle_crank = hot_key;
            state_struct.feature_bit_flags = FeatureBitFlags::MmOracleUpdate as u8;
            create_anchor_account_info!(state_struct, State, state_info);

            market_account!(0, (0, 0, 0), market_info);

            let signer_key = if wrong_key { attacker } else { hot_key };
            let mut sig_lamports = 0u64;
            let mut sig_data: [u8; 0] = [];
            let sig_owner = Pubkey::new_unique();
            let signer = AccountInfo::new(
                &signer_key,
                wrong_key, // signs only in the wrong-key case
                false,
                &mut sig_lamports,
                &mut sig_data,
                &sig_owner,
                false,
            );

            let accounts = [signer, state_info, market_info];
            let err =
                update_mm_oracle_batch(&accounts, &batch_payload(&[(0, BASE_PRICE, 1)]), SLOT)
                    .unwrap_err();
            assert_eq!(err, ErrorCode::Unauthorized.into());
        }
    }

    #[test]
    fn batch_rejects_when_kill_switch_is_off() {
        let hot_key = Pubkey::new_unique();
        let mut state_struct = State::default();
        state_struct.hot_mm_oracle_crank = hot_key;
        state_struct.feature_bit_flags = 0; // MmOracleUpdate clear
        create_anchor_account_info!(state_struct, State, state_info);

        market_account!(0, (0, 0, 0), market_info);

        let mut sig_lamports = 0u64;
        let mut sig_data: [u8; 0] = [];
        let sig_owner = Pubkey::new_unique();
        let signer = AccountInfo::new(
            &hot_key,
            true,
            false,
            &mut sig_lamports,
            &mut sig_data,
            &sig_owner,
            false,
        );

        let accounts = [signer, state_info, market_info.clone()];
        let err = update_mm_oracle_batch(&accounts, &batch_payload(&[(0, BASE_PRICE, 1)]), SLOT)
            .unwrap_err();
        assert_eq!(err, ErrorCode::MmOracleUpdateDisabled.into());
        assert_eq!(
            read_stats(&market_info),
            (0, 0, 0),
            "nothing may be written"
        );
    }

    // functional

    /// The core property: four markets in one call, three of which must be
    /// skipped for a different reason each. The skips must not disturb the
    /// healthy market, the call must succeed, and the reject mask must name
    /// exactly the skipped positions. Prices are distinct per entry so a
    /// positional mix-up between entries and accounts would be visible.
    #[test]
    fn batch_skips_bad_entries_and_still_writes_the_good_one() {
        let hot_key = Pubkey::new_unique();
        valid_prologue!(hot_key, state_info, signer);

        // 0: cranked one slot ago, below MM_ORACLE_MIN_SLOT_GAP.
        market_account!(0, (BASE_PRICE, SLOT - 1, 5), rate_limited_info);
        // 1: healthy.
        market_account!(1, (BASE_PRICE, SLOT - 10, 5), healthy_info);
        // 2: feed produced a zero price.
        market_account!(2, (BASE_PRICE, SLOT - 10, 5), zero_priced_info);
        // 3: sequence id has not advanced.
        market_account!(3, (BASE_PRICE, SLOT - 10, 9), stale_sequence_info);

        let payload = batch_payload(&[
            (0, BASE_PRICE + 1_000, 6),
            (1, BASE_PRICE + 2_000, 6),
            (2, 0, 6),
            (3, BASE_PRICE + 4_000, 9),
        ]);
        let accounts = [
            signer,
            state_info,
            rate_limited_info.clone(),
            healthy_info.clone(),
            zero_priced_info.clone(),
            stale_sequence_info.clone(),
        ];

        let (mask, _) = update_mm_oracle_batch(&accounts, &payload, SLOT).unwrap();
        assert_eq!(mask, 0b1101, "entries 0, 2 and 3 must be reported rejected");

        assert_eq!(
            read_stats(&rate_limited_info),
            (BASE_PRICE, SLOT - 1, 5),
            "rate-limited market must be untouched"
        );
        assert_eq!(
            read_stats(&healthy_info),
            (BASE_PRICE + 2_000, SLOT, 6),
            "healthy market must be written despite its neighbours failing"
        );
        assert_eq!(
            read_stats(&zero_priced_info),
            (BASE_PRICE, SLOT - 10, 5),
            "zero-priced market must be untouched"
        );
        assert_eq!(
            read_stats(&stale_sequence_info),
            (BASE_PRICE, SLOT - 10, 9),
            "stale-sequence market must be untouched"
        );
    }

    #[test]
    fn batch_reports_empty_mask_when_everything_lands() {
        let hot_key = Pubkey::new_unique();
        valid_prologue!(hot_key, state_info, signer);
        market_account!(0, (BASE_PRICE, SLOT - 10, 5), a_info);
        market_account!(1, (BASE_PRICE, SLOT - 10, 5), b_info);

        let payload = batch_payload(&[(0, BASE_PRICE + 1, 6), (1, BASE_PRICE + 2, 6)]);
        let accounts = [signer, state_info, a_info.clone(), b_info.clone()];

        assert_eq!(
            update_mm_oracle_batch(&accounts, &payload, SLOT).unwrap(),
            (0, 0)
        );
        assert_eq!(read_stats(&a_info), (BASE_PRICE + 1, SLOT, 6));
        assert_eq!(read_stats(&b_info), (BASE_PRICE + 2, SLOT, 6));
    }

    /// Skip conditions the equivalence matrix does not reach: a strictly
    /// decreasing sequence id and a strictly decreasing slot. The slot case is
    /// what stops the gap subtraction underflowing.
    #[test]
    fn batch_skips_strictly_regressing_sequence_id_and_slot() {
        let hot_key = Pubkey::new_unique();
        valid_prologue!(hot_key, state_info, signer);
        market_account!(0, (BASE_PRICE, SLOT - 10, 50), older_sequence_info);
        market_account!(1, (BASE_PRICE, SLOT + 10, 5), future_slot_info);

        let payload = batch_payload(&[(0, BASE_PRICE + 1, 6), (1, BASE_PRICE + 1, 6)]);
        let accounts = [
            signer,
            state_info,
            older_sequence_info.clone(),
            future_slot_info.clone(),
        ];

        assert_eq!(
            update_mm_oracle_batch(&accounts, &payload, SLOT).unwrap(),
            (0b11, 0)
        );
        assert_eq!(
            read_stats(&older_sequence_info),
            (BASE_PRICE, SLOT - 10, 50)
        );
        assert_eq!(read_stats(&future_slot_info), (BASE_PRICE, SLOT + 10, 5));
    }

    /// The step cap is symmetric, a move of exactly 1% is written verbatim, and
    /// a move beyond the cap is clamped to the cap rather than skipped, so it
    /// counts as accepted in the reject mask.
    #[test]
    fn batch_step_cap_is_symmetric_and_clamps_beyond_the_cap() {
        let hot_key = Pubkey::new_unique();
        valid_prologue!(hot_key, state_info, signer);

        // BASE_PRICE is 1_000_000, so exactly 1% is 10_000.
        market_account!(0, (BASE_PRICE, SLOT - 10, 5), down_over_info);
        market_account!(1, (BASE_PRICE, SLOT - 10, 5), down_at_cap_info);
        market_account!(2, (BASE_PRICE, SLOT - 10, 5), up_at_cap_info);
        market_account!(3, (BASE_PRICE, SLOT - 10, 5), up_over_info);

        let payload = batch_payload(&[
            (0, BASE_PRICE - 50_000, 6),
            (1, BASE_PRICE - 10_000, 6),
            (2, BASE_PRICE + 10_000, 6),
            (3, BASE_PRICE + 50_000, 6),
        ]);
        let accounts = [
            signer,
            state_info,
            down_over_info.clone(),
            down_at_cap_info.clone(),
            up_at_cap_info.clone(),
            up_over_info.clone(),
        ];

        assert_eq!(
            update_mm_oracle_batch(&accounts, &payload, SLOT).unwrap(),
            (0, 0b1001),
            "a clamped write is accepted (empty reject mask) and reported in \
             the clamped mask"
        );
        assert_eq!(
            read_stats(&down_over_info),
            (BASE_PRICE - 10_000, SLOT, 6),
            "beyond-cap move must be clamped to the cap"
        );
        assert_eq!(
            read_stats(&down_at_cap_info),
            (BASE_PRICE - 10_000, SLOT, 6)
        );
        assert_eq!(read_stats(&up_at_cap_info), (BASE_PRICE + 10_000, SLOT, 6));
        assert_eq!(
            read_stats(&up_over_info),
            (BASE_PRICE + 10_000, SLOT, 6),
            "beyond-cap move must be clamped to the cap"
        );
    }

    #[test]
    fn batch_skips_negative_price() {
        // `oracle_validity` classifies a stored non-positive price as
        // `NonPositive` on read, so storing one buys nothing.
        let hot_key = Pubkey::new_unique();
        valid_prologue!(hot_key, state_info, signer);
        market_account!(0, (0, 0, 0), market_info); // bootstrap: stored price 0

        let accounts = [signer, state_info, market_info.clone()];
        let (mask, _) =
            update_mm_oracle_batch(&accounts, &batch_payload(&[(0, -BASE_PRICE, 1)]), SLOT)
                .unwrap();
        assert_eq!(mask, 0b1);
        assert_eq!(read_stats(&market_info), (0, 0, 0));
    }

    /// The same market listed twice must not alias its `RefCell` borrow, and must
    /// not be written twice: the first entry advances `mm_oracle_slot` to the
    /// current slot, so the second falls out on the slot check. Run at the
    /// maximum batch size, which also exercises the top bit of the reject mask.
    #[test]
    fn batch_tolerates_duplicate_market_accounts_at_max_size() {
        let hot_key = Pubkey::new_unique();
        valid_prologue!(hot_key, state_info, signer);
        market_account!(0, (BASE_PRICE, SLOT - 10, 5), market_info);

        // Every entry targets the same market with a strictly increasing
        // sequence id, so only the slot check can stop entries 1..n.
        let entries: Vec<(u16, i64, u64)> = (0..MM_ORACLE_BATCH_MAX_MARKETS)
            .map(|i| (0u16, BASE_PRICE + 1 + i as i64, 6 + i as u64))
            .collect();
        let payload = batch_payload(&entries);

        let mut accounts = vec![signer, state_info];
        accounts.extend(std::iter::repeat_n(
            market_info.clone(),
            MM_ORACLE_BATCH_MAX_MARKETS,
        ));

        let (mask, _) = update_mm_oracle_batch(&accounts, &payload, SLOT).unwrap();
        assert_eq!(
            mask, !1u64,
            "only the first entry for a duplicated market may land"
        );
        assert_eq!(read_stats(&market_info), (BASE_PRICE + 1, SLOT, 6));
    }

    #[test]
    fn batch_ignores_trailing_accounts() {
        let hot_key = Pubkey::new_unique();
        valid_prologue!(hot_key, state_info, signer);
        market_account!(0, (BASE_PRICE, SLOT - 10, 5), market_a_info);
        // Declared count is 1, so this must never be touched even though it is a
        // perfectly valid perp market sitting in the account list.
        market_account!(1, (BASE_PRICE, SLOT - 10, 5), market_b_info);

        let accounts = [
            signer,
            state_info,
            market_a_info.clone(),
            market_b_info.clone(),
        ];
        update_mm_oracle_batch(&accounts, &batch_payload(&[(0, BASE_PRICE + 1, 6)]), SLOT).unwrap();

        assert_eq!(read_stats(&market_a_info), (BASE_PRICE + 1, SLOT, 6));
        assert_eq!(
            read_stats(&market_b_info),
            (BASE_PRICE, SLOT - 10, 5),
            "account beyond the declared count must be untouched"
        );
    }

    // equivalence with the single-market handler

    fn run_single_with_source(
        initial: MmStats,
        price: i64,
        sequence_id: u64,
        source_slot: u64,
    ) -> MmStats {
        let hot_key = Pubkey::new_unique();
        valid_prologue!(hot_key, state_info, signer);
        market_account!(0, initial, market_info);

        // Opcode 0 takes [market, signer, state].
        let accounts = [market_info.clone(), signer, state_info];
        update_mm_oracle(
            &accounts,
            &single_payload_with_source(price, sequence_id, source_slot),
            SLOT,
        )
        .unwrap();
        read_stats(&market_info)
    }

    fn run_single(initial: MmStats, price: i64, sequence_id: u64) -> MmStats {
        run_single_with_source(initial, price, sequence_id, SLOT)
    }

    fn run_batch_with_source(
        initial: MmStats,
        price: i64,
        sequence_id: u64,
        source_slot: u64,
    ) -> MmStats {
        let hot_key = Pubkey::new_unique();
        valid_prologue!(hot_key, state_info, signer);
        market_account!(0, initial, market_info);

        let accounts = [signer, state_info, market_info.clone()];
        update_mm_oracle_batch(
            &accounts,
            &batch_payload_with_source(&[(0, price, sequence_id, source_slot)]),
            SLOT,
        )
        .unwrap();
        read_stats(&market_info)
    }

    fn run_batch(initial: MmStats, price: i64, sequence_id: u64) -> MmStats {
        run_batch_with_source(initial, price, sequence_id, SLOT)
    }

    /// Pins opcode 2 to opcode 0 at the wire level. The per-market gating is
    /// shared (`apply_mm_oracle_update`), so this now guards the wrappers: the
    /// payload parsing, the prologue differences, and any future divergence.
    /// Zero and negative prices are excluded because their divergence is
    /// deliberate and is asserted separately below.
    ///
    /// Each case also asserts the expected result outright, so a bug in the
    /// shared core cannot pass by agreeing with itself.
    #[test]
    fn batch_matches_single_market_handler() {
        // (label, initial, price, sequence_id, expected)
        let cases: [(&str, MmStats, i64, u64, MmStats); 8] = [
            (
                "bootstrap from zero",
                (0, 0, 0),
                BASE_PRICE,
                1,
                (BASE_PRICE, SLOT, 1),
            ),
            (
                "accepts after a wide enough gap",
                (BASE_PRICE, SLOT - 10, 5),
                BASE_PRICE + 1_000,
                6,
                (BASE_PRICE + 1_000, SLOT, 6),
            ),
            (
                "accepts at exactly MM_ORACLE_MIN_SLOT_GAP",
                (BASE_PRICE, SLOT - 2, 5),
                BASE_PRICE + 1_000,
                6,
                (BASE_PRICE + 1_000, SLOT, 6),
            ),
            (
                "rejects one slot below the gap",
                (BASE_PRICE, SLOT - 1, 5),
                BASE_PRICE + 1_000,
                6,
                (BASE_PRICE, SLOT - 1, 5),
            ),
            (
                "rejects an equal sequence id",
                (BASE_PRICE, SLOT - 10, 6),
                BASE_PRICE + 1_000,
                6,
                (BASE_PRICE, SLOT - 10, 6),
            ),
            (
                "rejects a lower sequence id",
                (BASE_PRICE, SLOT - 10, 7),
                BASE_PRICE + 1_000,
                6,
                (BASE_PRICE, SLOT - 10, 7),
            ),
            (
                "rejects a slot that is not in the future",
                (BASE_PRICE, SLOT, 5),
                BASE_PRICE + 1_000,
                6,
                (BASE_PRICE, SLOT, 5),
            ),
            (
                "clamps a step above the cap and consumes the sequence id",
                (BASE_PRICE, SLOT - 10, 5),
                BASE_PRICE + 20_000,
                6,
                (BASE_PRICE + 10_000, SLOT, 6),
            ),
        ];

        for (label, initial, price, sequence_id, expected) in cases {
            let single = run_single(initial, price, sequence_id);
            let batch = run_batch(initial, price, sequence_id);
            assert_eq!(single, expected, "single handler wrong: {label}");
            assert_eq!(batch, expected, "batch handler wrong: {label}");
        }
    }

    /// The one documented behavioural divergence: opcode 0 returns `Err` on a
    /// non-positive price, which in a batch would destroy every other market's
    /// write, so opcode 2 skips it instead. Both reject; only the failure mode
    /// differs.
    #[test]
    fn non_positive_price_divergence_is_deliberate() {
        let initial = (BASE_PRICE, SLOT - 10, 5);

        for price in [0i64, -1, -BASE_PRICE] {
            // Batch: skipped, call succeeds, market untouched.
            assert_eq!(run_batch(initial, price, 6), initial);

            // Single: hard error, market untouched.
            let hot_key = Pubkey::new_unique();
            valid_prologue!(hot_key, state_info, signer);
            market_account!(0, initial, market_info);

            let accounts = [market_info.clone(), signer, state_info];
            assert!(
                update_mm_oracle(&accounts, &single_payload(price, 6), SLOT).is_err(),
                "price {price} must be a hard error on opcode 0"
            );
            assert_eq!(read_stats(&market_info), initial);
        }
    }

    /// Source-observation freshness on both handlers, symmetric around the
    /// landing slot: an update whose source slot is more than
    /// `MM_ORACLE_MAX_SOURCE_AGE` away in either direction is skipped —
    /// behind means it landed too late to be fresh, ahead means a wrong-unit
    /// or wrong-scale source value that must not silently disable the gate.
    /// Exactly at the bound lands on both sides, so a crank may still estimate
    /// its landing slot.
    #[test]
    fn stale_source_slot_is_skipped_by_both_handlers() {
        let initial = (BASE_PRICE, SLOT - 10, 5);
        let written = (BASE_PRICE + 1_000, SLOT, 6);
        let max_age =
            crate::math::constants::MM_ORACLE_MAX_SOURCE_AGE.to_slots(SlotDuration::BASELINE);

        // (label, source_slot, expected)
        let cases: [(&str, u64, MmStats); 5] = [
            (
                "one slot beyond the bound is skipped",
                SLOT - max_age - 1,
                initial,
            ),
            ("exactly at the bound lands", SLOT - max_age, written),
            (
                "a landing-slot estimate at the forward bound lands",
                SLOT + max_age,
                written,
            ),
            (
                "one slot beyond the forward bound is skipped",
                SLOT + max_age + 1,
                initial,
            ),
            (
                "a wrong-unit source value is skipped, not accepted",
                1_700_000_000_000, // a millisecond timestamp
                initial,
            ),
        ];

        for (label, source_slot, expected) in cases {
            let single = run_single_with_source(initial, BASE_PRICE + 1_000, 6, source_slot);
            let batch = run_batch_with_source(initial, BASE_PRICE + 1_000, 6, source_slot);
            assert_eq!(single, expected, "single handler wrong: {label}");
            assert_eq!(batch, expected, "batch handler wrong: {label}");
        }

        // The skip is reported in the batch reject mask.
        let hot_key = Pubkey::new_unique();
        valid_prologue!(hot_key, state_info, signer);
        market_account!(0, initial, market_info);
        let accounts = [signer, state_info, market_info.clone()];
        let stale = SLOT - max_age - 1;
        let (mask, clamped) = update_mm_oracle_batch(
            &accounts,
            &batch_payload_with_source(&[(0, BASE_PRICE + 1_000, 6, stale)]),
            SLOT,
        )
        .unwrap();
        assert_eq!(mask, 0b1);
        assert_eq!(clamped, 0);
        assert_eq!(read_stats(&market_info), initial);
    }

    #[test]
    fn source_age_integrates_across_slot_duration_transition() {
        let clock = SlotClock::from_state_fields([1, 1, 1, 100], 0, 0, 0);
        // Four slots priced only at the 200ms endpoint would look exactly 800ms
        // old. Two of them were actually 250ms, so the observation is 900ms old.
        assert!(mm_oracle_source_slot_out_of_range(clock, 102, 98));
        assert!(!mm_oracle_source_slot_out_of_range(clock, 102, 99));
    }
}
