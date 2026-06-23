mod size {
    use crate::state::events::OrderActionRecord;
    use crate::state::insurance_fund_stake::InsuranceFundStake;
    use crate::state::perp_market::PerpMarket;
    use crate::state::spot_market::SpotMarket;
    use crate::state::state::State;
    use crate::state::traits::Size;
    use crate::state::user::{User, UserStats};

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
    }

    #[test]
    fn insurance_fund_stake() {
        let expected_size = std::mem::size_of::<InsuranceFundStake>() + 8;
        let actual_size = InsuranceFundStake::SIZE;
        assert_eq!(actual_size, expected_size);
    }
}


mod market_index_offset {
    // PoolBalance padding was widened so sizeof(PoolBalance) == 32 on both
    // x86_64 and SBF.  Struct fields were reordered so all u128-containing
    // types appear before the PoolBalance fields, eliminating architecture-
    // specific alignment gaps.  MARKET_INDEX_OFFSET is now the same value on
    // both architectures and these tests can run everywhere.
    use crate::create_anchor_account_info;
    use crate::state::perp_market::PerpMarket;
    use crate::state::spot_market::SpotMarket;
    use crate::state::traits::MarketIndexOffset;
    use arrayref::array_ref;

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
