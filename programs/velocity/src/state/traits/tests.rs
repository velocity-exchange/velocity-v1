mod size {
    use crate::state::{
        events::OrderActionRecord,
        insurance_fund_stake::InsuranceFundStake,
        perp_market::PerpMarket,
        spot_market::SpotMarket,
        state::State,
        traits::Size,
        user::{User, UserStats},
    };

    #[test]
    fn order_action_records() {
        let expected_size = std::mem::size_of::<OrderActionRecord>() + 8;
        let actual_size = OrderActionRecord::SIZE;
        assert_eq!(actual_size, expected_size);
    }

    #[test]
    fn perp_market() {
        let expected_size = std::mem::size_of::<PerpMarket>() + 8;
        let actual_size = PerpMarket::SIZE;
        assert_eq!(actual_size, expected_size);
    }

    #[test]
    fn spot_market() {
        let expected_size = std::mem::size_of::<SpotMarket>() + 8;
        let actual_size = SpotMarket::SIZE;
        assert_eq!(actual_size, expected_size);
    }

    #[test]
    fn state() {
        let expected_size = std::mem::size_of::<State>() + 8;
        let actual_size = State::SIZE;
        assert_eq!(actual_size, expected_size);
    }

    #[test]
    fn user() {
        let expected_size = std::mem::size_of::<User>() + 8;
        let actual_size = User::SIZE;
        assert_eq!(actual_size, expected_size);
    }

    #[test]
    fn user_stats() {
        let expected_size = std::mem::size_of::<UserStats>() + 8;
        let actual_size = UserStats::SIZE;
        assert_eq!(actual_size, expected_size);

        // `padding1` replaced the removed `if_staked_gov_token_amount: u64` plus the
        // 1 byte of repr(C) alignment padding that preceded it; offsets of the fields
        // around it must not move for existing on-chain accounts to stay valid.
        assert_eq!(std::mem::offset_of!(UserStats, padding1), 159);
        assert_eq!(std::mem::offset_of!(UserStats, delegate_permissions), 168);
        assert_eq!(
            std::mem::offset_of!(UserStats, accelerated_referral_status),
            170
        );
    }

    #[test]
    fn insurance_fund_stake() {
        let expected_size = std::mem::size_of::<InsuranceFundStake>() + 8;
        let actual_size = InsuranceFundStake::SIZE;
        assert_eq!(actual_size, expected_size);
    }
}

/// Guards the hardcoded `State` byte offsets read by the two native (non-Anchor)
/// instruction handlers (`handle_update_mm_oracle_native`,
/// `handle_update_amm_spread_adjustment_native`).
///
/// Those handlers run before Anchor and, after validating ownership +
/// discriminator (`auth::require_native_account`), read the State auth fields by
/// fixed offset rather than deserializing the whole account:
///
/// * `feature_bit_flags` (byte 1374) — MM-oracle kill switch
/// * `hot_mm_oracle_crank` (bytes 360..392) — MM-oracle signer
/// * `hot_amm_spread_adjust` (bytes 392..424) — native spread-adjustment bot signer
///
/// The `PerpMarket`/`AMM` offsets below are not read by raw index (the handlers
/// `bytemuck`-cast the account and use typed field access) but are asserted here
/// as layout invariants. `State` is `#[account(zero_copy(unsafe))]` + `repr(C)`;
/// use `std::mem::offset_of!(_, field) + 8` (discriminator). If any test fails
/// after a struct change, update the literal in the handler AND here together.
mod native_instruction_offsets {
    use crate::state::{
        perp_market::{MarketStats, PerpMarket, AMM},
        state::State,
    };

    const DISC: usize = 8; // Anchor 8-byte account discriminator

    #[test]
    fn amm_zero_copy_offsets() {
        let amm_start = DISC + std::mem::offset_of!(PerpMarket, amm);
        let stats_start = DISC + std::mem::offset_of!(PerpMarket, market_stats);
        assert_eq!(
            stats_start + std::mem::offset_of!(MarketStats, mm_oracle_price),
            800,
            "mm_oracle_price offset changed"
        );
        assert_eq!(
            stats_start + std::mem::offset_of!(MarketStats, mm_oracle_slot),
            808,
            "mm_oracle_slot offset changed"
        );
        assert_eq!(
            stats_start + std::mem::offset_of!(MarketStats, mm_oracle_sequence_id),
            816,
            "mm_oracle_sequence_id offset changed"
        );
        assert_eq!(
            std::mem::offset_of!(PerpMarket, fee_ledger) % 16,
            0,
            "fee_ledger must be 16-aligned (host/SBF layout parity)"
        );
        assert_eq!(
            amm_start + std::mem::offset_of!(AMM, amm_spread_adjustment),
            1282,
            "amm_spread_adjustment offset changed"
        );
        // Repurposed the former 4-byte trailing padding before market_stats;
        // it must keep occupying exactly those bytes (4-aligned) so every
        // other offset stays fixed and legacy accounts read the initial 0
        // (= the default floor).
        assert_eq!(
            DISC + std::mem::offset_of!(PerpMarket, bankruptcy_if_floor_pct),
            stats_start - 4,
            "bankruptcy_if_floor_pct must sit in the 4 bytes before market_stats"
        );
        // Repurposed the first 2 of the 6 alignment-padding bytes before
        // last_fill_price. The remaining 4 stay padding, so last_fill_price
        // and every later field keep their offsets and legacy accounts read
        // the initial 0 (= no pending claim).
        assert_eq!(
            DISC + std::mem::offset_of!(PerpMarket, pending_bankruptcy_claims),
            DISC + std::mem::offset_of!(PerpMarket, last_fill_price) - 6,
            "pending_bankruptcy_claims must sit in the 6 bytes before last_fill_price"
        );
        assert_eq!(
            std::mem::offset_of!(PerpMarket, pending_bankruptcy_claims) % 2,
            0,
            "pending_bankruptcy_claims must be 2-aligned"
        );
    }

    /// State.feature_bit_flags is read at byte 1374 by the MM-oracle kill switch.
    #[test]
    fn state_feature_bit_flags_offset() {
        assert_eq!(
            std::mem::offset_of!(State, feature_bit_flags) + DISC,
            1374,
            "State::feature_bit_flags offset changed — update handle_update_mm_oracle_native"
        );
    }

    /// The native MM-oracle handlers read the live slot duration straight from
    /// account bytes: `slot_duration_ms` at 1506..1508, `pending_slot_duration_ms`
    /// at 1508..1510, and `slot_duration_effective_slot` at 1512..1520. If any of
    /// these move, update `read_native_state_slot_duration` (and its
    /// `STATE_*_OFFSET` constants) to match.
    #[test]
    fn state_slot_duration_offsets() {
        assert_eq!(
            std::mem::offset_of!(State, slot_duration_ms) + DISC,
            1506,
            "State::slot_duration_ms offset changed — update read_native_state_slot_duration"
        );
        assert_eq!(
            std::mem::offset_of!(State, pending_slot_duration_ms) + DISC,
            1508,
            "State::pending_slot_duration_ms offset changed — update read_native_state_slot_duration"
        );
        assert_eq!(
            std::mem::offset_of!(State, slot_duration_effective_slot) + DISC,
            1512,
            "State::slot_duration_effective_slot offset changed — update read_native_state_slot_duration"
        );
    }

    /// A staged switch takes effect exactly at its effective slot and not before;
    /// with nothing staged the base value always holds. Same logic the native
    /// reader mirrors byte-for-byte.
    #[test]
    fn active_slot_duration_switches_at_effective_slot() {
        let mut state = State::default();
        state.slot_duration_ms = 350;
        state.pending_slot_duration_ms = 300;
        state.slot_duration_effective_slot = 1_000;
        assert_eq!(state.active_slot_duration_ms(999), 350); // before: base
        assert_eq!(state.active_slot_duration_ms(1_000), 300); // at boundary: pending
        assert_eq!(state.active_slot_duration_ms(5_000), 300); // after: pending
        state.pending_slot_duration_ms = 0; // nothing staged
        assert_eq!(state.active_slot_duration_ms(5_000), 350);
    }

    /// State.hot_mm_oracle_crank is read at bytes 360..392 by the MM-oracle handler.
    #[test]
    fn state_hot_mm_oracle_crank_offset() {
        assert_eq!(
            std::mem::offset_of!(State, hot_mm_oracle_crank) + DISC,
            360,
            "State::hot_mm_oracle_crank offset changed — update handle_update_mm_oracle_native"
        );
    }

    /// State.hot_amm_spread_adjust is read at bytes 392..424 by the spread handler.
    #[test]
    fn state_hot_amm_spread_adjust_offset() {
        assert_eq!(
            std::mem::offset_of!(State, hot_amm_spread_adjust) + DISC,
            392,
            "State::hot_amm_spread_adjust offset changed — update handle_update_amm_spread_adjustment_native"
        );
    }

    /// The quote management authority consumes the first 32 bytes of former padding.
    #[test]
    fn state_hot_vamm_quote_management_offset() {
        assert_eq!(
            std::mem::offset_of!(State, hot_vamm_quote_management) + DISC,
            1552,
            "State::hot_vamm_quote_management must remain in former padding"
        );
    }
}

mod hot_role_ordinals {
    use {crate::state::state::HotRole, anchor_lang::AnchorSerialize};

    fn encode(role: HotRole) -> Vec<u8> {
        let mut bytes = Vec::new();
        role.serialize(&mut bytes).unwrap();
        bytes
    }

    #[test]
    fn preserves_existing_roles_and_appends_quote_management() {
        assert_eq!(encode(HotRole::AmmSpreadAdjust), vec![9]);
        assert_eq!(encode(HotRole::FeeWithdraw), vec![10]);
        assert_eq!(encode(HotRole::AccountExtension), vec![11]);
        assert_eq!(encode(HotRole::VammQuoteManagement), vec![12]);
    }
}

mod market_index_offset {
    // PoolBalance padding was widened so sizeof(PoolBalance) == 32 on both
    // x86_64 and SBF.  Struct fields were reordered so all u128-containing
    // types appear before the PoolBalance fields, eliminating architecture-
    // specific alignment gaps.  MARKET_INDEX_OFFSET is now the same value on
    // both architectures and these tests can run everywhere.
    use {
        crate::{
            create_anchor_account_info,
            state::{perp_market::PerpMarket, spot_market::SpotMarket, traits::MarketIndexOffset},
        },
        arrayref::array_ref,
    };

    #[test]
    fn spot_market() {
        let mut spot_market = SpotMarket {
            market_index: 11,
            ..SpotMarket::default()
        };
        create_anchor_account_info!(spot_market, SpotMarket, spot_market_account_info);

        let data = spot_market_account_info.try_borrow_data().unwrap();
        let market_index =
            u16::from_le_bytes(*array_ref![data, SpotMarket::MARKET_INDEX_OFFSET, 2]);
        assert_eq!(market_index, spot_market.market_index);
    }

    #[test]
    fn perp_market() {
        let mut perp_market = PerpMarket {
            market_index: 11,
            ..PerpMarket::default()
        };
        create_anchor_account_info!(perp_market, PerpMarket, perp_market_account_info);

        let data = perp_market_account_info.try_borrow_data().unwrap();
        let market_index =
            u16::from_le_bytes(*array_ref![data, PerpMarket::MARKET_INDEX_OFFSET, 2]);
        assert_eq!(market_index, perp_market.market_index);
    }
}
