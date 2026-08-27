mod calculate_fee_for_taker_and_maker {
    use crate::{
        math::{
            constants::QUOTE_PRECISION_U64,
            fees::{calculate_fee_for_fulfillment_with_match, FillFees},
            time::SlotClock,
        },
        state::{
            state::FeeStructure,
            user::{MarketType, UserStats},
        },
    };

    #[test]
    fn no_filler() {
        let quote_asset_amount = 100 * QUOTE_PRECISION_U64;
        let taker_stats = UserStats::default();
        let mut maker_stats = UserStats::default();

        let FillFees {
            user_fee: taker_fee,
            maker_rebate,
            fee_to_market,
            protocol_fee,
            if_fee,
            filler_reward,
            referee_discount,
            referrer_reward,
            ..
        } = calculate_fee_for_fulfillment_with_match(
            &taker_stats,
            &Some(&mut maker_stats),
            quote_asset_amount,
            &FeeStructure::test_default(),
            0,
            0,
            0,
            false,
            false,
            &MarketType::Perp,
            0,
            None,
            0,
            0,
            0,
            SlotClock::baseline(),
            0,
        )
        .unwrap();

        assert_eq!(taker_fee, 100000);
        assert_eq!(maker_rebate, 60000);
        assert_eq!(fee_to_market, 0);
        assert_eq!(protocol_fee, 40000);
        assert_eq!(if_fee, 0);
        assert_eq!(filler_reward, 0);
        assert_eq!(referrer_reward, 0);
        assert_eq!(referee_discount, 0);
    }

    #[test]
    fn filler_size_reward() {
        let quote_asset_amount = 100 * QUOTE_PRECISION_U64;

        let taker_stats = UserStats::default();
        let mut maker_stats = UserStats::default();

        let mut fee_structure = FeeStructure::test_default();
        fee_structure
            .filler_reward_structure
            .time_based_reward_lower_bound = 10000000000000000; // big number

        let FillFees {
            user_fee: taker_fee,
            maker_rebate,
            fee_to_market,
            protocol_fee,
            if_fee,
            filler_reward,
            referee_discount,
            referrer_reward,
            ..
        } = calculate_fee_for_fulfillment_with_match(
            &taker_stats,
            &Some(&mut maker_stats),
            quote_asset_amount,
            &fee_structure,
            0,
            0,
            1,
            false,
            false,
            &MarketType::Perp,
            0,
            None,
            0,
            0,
            0,
            SlotClock::baseline(),
            0,
        )
        .unwrap();

        assert_eq!(taker_fee, 100000);
        assert_eq!(maker_rebate, 60000);
        assert_eq!(fee_to_market, 0);
        assert_eq!(protocol_fee, 30000);
        assert_eq!(if_fee, 0);
        assert_eq!(filler_reward, 10000);
        assert_eq!(referrer_reward, 0);
        assert_eq!(referee_discount, 0);
    }

    #[test]
    fn time_reward_no_time_passed() {
        let quote_asset_amount = 100 * QUOTE_PRECISION_U64;

        let taker_stats = UserStats::default();
        let mut maker_stats = UserStats::default();

        let mut fee_structure = FeeStructure::test_default();
        fee_structure.filler_reward_structure.reward_numerator = 1; // will make size reward the whole fee
        fee_structure.filler_reward_structure.reward_denominator = 1;

        let FillFees {
            user_fee: taker_fee,
            maker_rebate,
            fee_to_market,
            protocol_fee,
            if_fee,
            filler_reward,
            referee_discount,
            referrer_reward,
            ..
        } = calculate_fee_for_fulfillment_with_match(
            &taker_stats,
            &Some(&mut maker_stats),
            quote_asset_amount,
            &fee_structure,
            0,
            0,
            1,
            false,
            false,
            &MarketType::Perp,
            0,
            None,
            0,
            0,
            0,
            SlotClock::baseline(),
            0,
        )
        .unwrap();

        assert_eq!(taker_fee, 100000);
        assert_eq!(maker_rebate, 60000);
        assert_eq!(fee_to_market, 0);
        assert_eq!(protocol_fee, 30000);
        assert_eq!(if_fee, 0);
        assert_eq!(filler_reward, 10000);
        assert_eq!(referrer_reward, 0);
        assert_eq!(referee_discount, 0);
    }

    #[test]
    fn time_reward_time_passed() {
        let quote_asset_amount = 100 * QUOTE_PRECISION_U64;

        let taker_stats = UserStats::default();
        let mut maker_stats = UserStats::default();

        let mut fee_structure = FeeStructure::test_default();
        fee_structure.filler_reward_structure.reward_numerator = 1; // will make size reward the whole fee
        fee_structure.filler_reward_structure.reward_denominator = 1;

        let FillFees {
            user_fee: taker_fee,
            maker_rebate,
            fee_to_market,
            protocol_fee,
            if_fee,
            filler_reward,
            referee_discount,
            referrer_reward,
            ..
        } = calculate_fee_for_fulfillment_with_match(
            &taker_stats,
            &Some(&mut maker_stats),
            quote_asset_amount,
            &fee_structure,
            0,
            60,
            1,
            false,
            false,
            &MarketType::Perp,
            0,
            None,
            0,
            0,
            0,
            SlotClock::baseline(),
            0,
        )
        .unwrap();

        assert_eq!(taker_fee, 100000);
        assert_eq!(maker_rebate, 60000);
        assert_eq!(fee_to_market, 0);
        assert_eq!(protocol_fee, 12200);
        assert_eq!(if_fee, 0);
        assert_eq!(filler_reward, 27800);
        assert_eq!(referrer_reward, 0);
        assert_eq!(referee_discount, 0);
    }

    #[test]
    fn referrer() {
        let quote_asset_amount = 100 * QUOTE_PRECISION_U64;

        let taker_stats = UserStats::default();
        let mut maker_stats = UserStats::default();

        let fee_structure = FeeStructure::test_default();

        let FillFees {
            user_fee: taker_fee,
            maker_rebate,
            fee_to_market,
            protocol_fee,
            if_fee,
            filler_reward,
            referee_discount,
            referrer_reward,
            ..
        } = calculate_fee_for_fulfillment_with_match(
            &taker_stats,
            &Some(&mut maker_stats),
            quote_asset_amount,
            &fee_structure,
            0,
            0,
            0,
            true,
            false,
            &MarketType::Perp,
            0,
            None,
            0,
            0,
            0,
            SlotClock::baseline(),
            0,
        )
        .unwrap();

        assert_eq!(taker_fee, 90000);
        assert_eq!(maker_rebate, 60000);
        assert_eq!(fee_to_market, 0);
        assert_eq!(protocol_fee, 20000);
        assert_eq!(if_fee, 0);
        assert_eq!(filler_reward, 0);
        assert_eq!(referrer_reward, 10000);
        assert_eq!(referee_discount, 10000);
    }

    #[test]
    fn accelerated_referrer_gets_fixed_reward_without_changing_referee_discount() {
        let mut maker_stats = UserStats::default();
        let mut fee_structure = FeeStructure::test_default();
        fee_structure.fee_tiers[0].referrer_reward_numerator = 7;
        let fees = calculate_fee_for_fulfillment_with_match(
            &UserStats::default(),
            &Some(&mut maker_stats),
            100 * QUOTE_PRECISION_U64,
            &fee_structure,
            0,
            0,
            0,
            true,
            true,
            &MarketType::Perp,
            0,
            None,
            0,
            0,
            0,
            SlotClock::baseline(),
            0,
        )
        .unwrap();

        assert_eq!(fees.referrer_reward, 20000);
        assert_eq!(fees.referee_discount, 10000);
    }

    #[test]
    fn fee_adjustment() {
        let quote_asset_amount = 100 * QUOTE_PRECISION_U64;
        let taker_stats = UserStats::default();
        let mut maker_stats = UserStats::default();

        let FillFees {
            user_fee: taker_fee,
            maker_rebate,
            fee_to_market,
            protocol_fee,
            if_fee,
            filler_reward,
            referee_discount,
            referrer_reward,
            ..
        } = calculate_fee_for_fulfillment_with_match(
            &taker_stats,
            &Some(&mut maker_stats),
            quote_asset_amount,
            &FeeStructure::test_default(),
            0,
            0,
            0,
            false,
            false,
            &MarketType::Perp,
            -50,
            None,
            0,
            0,
            0,
            SlotClock::baseline(),
            0,
        )
        .unwrap();

        assert_eq!(taker_fee, 50000);
        assert_eq!(maker_rebate, 30000);
        assert_eq!(fee_to_market, 0);
        assert_eq!(protocol_fee, 20000);
        assert_eq!(if_fee, 0);
        assert_eq!(filler_reward, 0);
        assert_eq!(referrer_reward, 0);
        assert_eq!(referee_discount, 0);

        let FillFees {
            user_fee: taker_fee,
            maker_rebate,
            fee_to_market,
            protocol_fee,
            if_fee,
            filler_reward,
            referee_discount,
            referrer_reward,
            ..
        } = calculate_fee_for_fulfillment_with_match(
            &taker_stats,
            &Some(&mut maker_stats),
            quote_asset_amount,
            &FeeStructure::test_default(),
            0,
            0,
            0,
            false,
            false,
            &MarketType::Perp,
            50,
            None,
            0,
            0,
            0,
            SlotClock::baseline(),
            0,
        )
        .unwrap();

        assert_eq!(taker_fee, 150000);
        assert_eq!(maker_rebate, 90000);
        assert_eq!(fee_to_market, 0);
        assert_eq!(protocol_fee, 60000);
        assert_eq!(if_fee, 0);
        assert_eq!(filler_reward, 0);
        assert_eq!(referrer_reward, 0);
        assert_eq!(referee_discount, 0);

        // reward referrer
        let FillFees {
            user_fee: taker_fee,
            maker_rebate,
            fee_to_market,
            protocol_fee,
            if_fee,
            filler_reward,
            referee_discount,
            referrer_reward,
            ..
        } = calculate_fee_for_fulfillment_with_match(
            &taker_stats,
            &Some(&mut maker_stats),
            quote_asset_amount,
            &FeeStructure::test_default(),
            0,
            0,
            0,
            true,
            false,
            &MarketType::Perp,
            -50,
            None,
            0,
            0,
            0,
            SlotClock::baseline(),
            0,
        )
        .unwrap();

        assert_eq!(taker_fee, 45000);
        assert_eq!(maker_rebate, 30000);
        assert_eq!(fee_to_market, 0);
        assert_eq!(protocol_fee, 10000);
        assert_eq!(if_fee, 0);
        assert_eq!(filler_reward, 0);
        assert_eq!(referrer_reward, 5000);
        assert_eq!(referee_discount, 5000);

        // reward referrer + filler
        let FillFees {
            user_fee: taker_fee,
            maker_rebate,
            fee_to_market,
            protocol_fee,
            if_fee,
            filler_reward,
            referee_discount,
            referrer_reward,
            ..
        } = calculate_fee_for_fulfillment_with_match(
            &taker_stats,
            &Some(&mut maker_stats),
            quote_asset_amount,
            &FeeStructure::test_default(),
            0,
            0,
            1,
            true,
            false,
            &MarketType::Perp,
            -50,
            None,
            0,
            0,
            0,
            SlotClock::baseline(),
            0,
        )
        .unwrap();

        assert_eq!(taker_fee, 45000);
        assert_eq!(maker_rebate, 30000);
        assert_eq!(fee_to_market, 0);
        assert_eq!(protocol_fee, 5500);
        assert_eq!(if_fee, 0);
        assert_eq!(filler_reward, 4500);
        assert_eq!(referrer_reward, 5000);
        assert_eq!(referee_discount, 5000);
    }

    #[test]
    fn fee_adjustment_free() {
        let quote_asset_amount = 100 * QUOTE_PRECISION_U64;
        let taker_stats = UserStats::default();
        let mut maker_stats = UserStats::default();

        let FillFees {
            user_fee: taker_fee,
            maker_rebate,
            fee_to_market,
            protocol_fee,
            if_fee,
            filler_reward,
            referee_discount,
            referrer_reward,
            ..
        } = calculate_fee_for_fulfillment_with_match(
            &taker_stats,
            &Some(&mut maker_stats),
            quote_asset_amount,
            &FeeStructure::test_default(),
            0,
            0,
            0,
            false,
            false,
            &MarketType::Perp,
            -100,
            None,
            0,
            0,
            0,
            SlotClock::baseline(),
            0,
        )
        .unwrap();

        assert_eq!(taker_fee, 0);
        assert_eq!(maker_rebate, 0);
        assert_eq!(fee_to_market, 0);
        assert_eq!(protocol_fee, 0);
        assert_eq!(if_fee, 0);
        assert_eq!(filler_reward, 0);
        assert_eq!(referrer_reward, 0);
        assert_eq!(referee_discount, 0);

        let FillFees {
            user_fee: taker_fee,
            maker_rebate,
            fee_to_market,
            protocol_fee,
            if_fee,
            filler_reward,
            referee_discount,
            referrer_reward,
            ..
        } = calculate_fee_for_fulfillment_with_match(
            &taker_stats,
            &Some(&mut maker_stats),
            quote_asset_amount,
            &FeeStructure::test_default(),
            0,
            0,
            0,
            false,
            false,
            &MarketType::Perp,
            -100,
            None,
            0,
            0,
            0,
            SlotClock::baseline(),
            0,
        )
        .unwrap();

        assert_eq!(taker_fee, 0);
        assert_eq!(maker_rebate, 0);
        assert_eq!(fee_to_market, 0);
        assert_eq!(protocol_fee, 0);
        assert_eq!(if_fee, 0);
        assert_eq!(filler_reward, 0);
        assert_eq!(referrer_reward, 0);
        assert_eq!(referee_discount, 0);

        let FillFees {
            user_fee: taker_fee,
            maker_rebate,
            fee_to_market,
            protocol_fee,
            if_fee,
            filler_reward,
            referee_discount,
            referrer_reward,
            ..
        } = calculate_fee_for_fulfillment_with_match(
            &taker_stats,
            &Some(&mut maker_stats),
            quote_asset_amount,
            &FeeStructure::test_default(),
            0,
            0,
            0,
            false,
            false,
            &MarketType::Perp,
            -100,
            None,
            0,
            0,
            0,
            SlotClock::baseline(),
            0,
        )
        .unwrap();

        assert_eq!(taker_fee, 0);
        assert_eq!(maker_rebate, 0);
        assert_eq!(fee_to_market, 0);
        assert_eq!(protocol_fee, 0);
        assert_eq!(if_fee, 0);
        assert_eq!(filler_reward, 0);
        assert_eq!(referrer_reward, 0);
        assert_eq!(referee_discount, 0);

        // reward referrer
        let FillFees {
            user_fee: taker_fee,
            maker_rebate,
            fee_to_market,
            protocol_fee,
            if_fee,
            filler_reward,
            referee_discount,
            referrer_reward,
            ..
        } = calculate_fee_for_fulfillment_with_match(
            &taker_stats,
            &Some(&mut maker_stats),
            quote_asset_amount,
            &FeeStructure::test_default(),
            0,
            0,
            0,
            true,
            false,
            &MarketType::Perp,
            -100,
            None,
            0,
            0,
            0,
            SlotClock::baseline(),
            0,
        )
        .unwrap();

        assert_eq!(taker_fee, 0);
        assert_eq!(maker_rebate, 0);
        assert_eq!(fee_to_market, 0);
        assert_eq!(protocol_fee, 0);
        assert_eq!(if_fee, 0);
        assert_eq!(filler_reward, 0);
        assert_eq!(referrer_reward, 0);
        assert_eq!(referee_discount, 0);

        // reward referrer + filler
        let FillFees {
            user_fee: taker_fee,
            maker_rebate,
            fee_to_market,
            protocol_fee,
            if_fee,
            filler_reward,
            referee_discount,
            referrer_reward,
            ..
        } = calculate_fee_for_fulfillment_with_match(
            &taker_stats,
            &Some(&mut maker_stats),
            quote_asset_amount,
            &FeeStructure::test_default(),
            0,
            0,
            1,
            true,
            false,
            &MarketType::Perp,
            -100,
            None,
            0,
            0,
            0,
            SlotClock::baseline(),
            0,
        )
        .unwrap();

        assert_eq!(taker_fee, 0);
        assert_eq!(maker_rebate, 0);
        assert_eq!(fee_to_market, 0);
        assert_eq!(protocol_fee, 0);
        assert_eq!(if_fee, 0);
        assert_eq!(filler_reward, 0);
        assert_eq!(referrer_reward, 0);
        assert_eq!(referee_discount, 0);
    }
}

mod calculate_fee_for_order_fulfill_against_amm {
    use crate::{
        math::{
            constants::{MAX_TAKER_FEE_ADDON_TENTH_BPS, QUOTE_PRECISION_U64},
            fees::{
                calculate_fee_for_fulfillment_with_amm, calculate_fee_for_fulfillment_with_match,
                FillFees,
            },
            time::SlotClock,
        },
        state::{
            state::FeeStructure,
            user::{MarketType, UserStats},
        },
    };

    #[test]
    fn referrer() {
        let quote_asset_amount = 100 * QUOTE_PRECISION_U64;

        let taker_stats = UserStats::default();
        let fee_structure = FeeStructure::test_default();

        let FillFees {
            user_fee,
            fee_to_market,
            filler_reward,
            referee_discount,
            referrer_reward,
            protocol_fee,
            if_fee,
            amm_fee,
            ..
        } = calculate_fee_for_fulfillment_with_amm(
            &taker_stats,
            quote_asset_amount,
            &fee_structure,
            0,
            60,
            false,
            true,
            false,
            0,
            false,
            0,
            None,
            false,
            0,
            0,
            0,
            SlotClock::baseline(),
            0,
        )
        .unwrap();

        assert_eq!(user_fee, 90000);
        // amm numerator is 0 in test_default: the AMM books nothing; the full
        // 80000 remainder is the protocol's residual carveout
        assert_eq!(fee_to_market, 0);
        assert_eq!(protocol_fee, 80000);
        assert_eq!(if_fee, 0);
        assert_eq!(amm_fee, 0);
        assert_eq!(filler_reward, 0);
        assert_eq!(referrer_reward, 10000);
        assert_eq!(referee_discount, 10000);
    }

    #[test]
    fn accelerated_referrer_gets_fixed_reward_without_changing_referee_discount() {
        let mut fee_structure = FeeStructure::test_default();
        fee_structure.fee_tiers[0].referrer_reward_numerator = 7;
        let fees = calculate_fee_for_fulfillment_with_amm(
            &UserStats::default(),
            100 * QUOTE_PRECISION_U64,
            &fee_structure,
            0,
            60,
            false,
            true,
            true,
            0,
            false,
            0,
            None,
            false,
            0,
            0,
            0,
            SlotClock::baseline(),
            0,
        )
        .unwrap();

        assert_eq!(fees.referrer_reward, 20000);
        assert_eq!(fees.referee_discount, 10000);
    }

    #[test]
    fn fee_adjustment() {
        let quote_asset_amount = 100 * QUOTE_PRECISION_U64;

        let taker_stats = UserStats::default();
        let fee_structure = FeeStructure::test_default();

        let FillFees {
            user_fee,
            fee_to_market,
            filler_reward,
            referee_discount,
            referrer_reward,
            protocol_fee,
            ..
        } = calculate_fee_for_fulfillment_with_amm(
            &taker_stats,
            quote_asset_amount,
            &fee_structure,
            0,
            60,
            false,
            false,
            false,
            0,
            false,
            -50,
            None,
            false,
            0,
            0,
            0,
            SlotClock::baseline(),
            0,
        )
        .unwrap();

        assert_eq!(user_fee, 50000);
        assert_eq!(fee_to_market, 0);
        assert_eq!(protocol_fee, 50000);
        assert_eq!(filler_reward, 0);
        assert_eq!(referrer_reward, 0);
        assert_eq!(referee_discount, 0);

        let FillFees {
            user_fee,
            fee_to_market,
            filler_reward,
            referee_discount,
            referrer_reward,
            protocol_fee,
            ..
        } = calculate_fee_for_fulfillment_with_amm(
            &taker_stats,
            quote_asset_amount,
            &fee_structure,
            0,
            60,
            false,
            false,
            false,
            0,
            false,
            50,
            None,
            false,
            0,
            0,
            0,
            SlotClock::baseline(),
            0,
        )
        .unwrap();

        assert_eq!(user_fee, 150000);
        assert_eq!(fee_to_market, 0);
        assert_eq!(protocol_fee, 150000);
        assert_eq!(filler_reward, 0);
        assert_eq!(referrer_reward, 0);
        assert_eq!(referee_discount, 0);

        // reward referrer
        let FillFees {
            user_fee,
            fee_to_market,
            filler_reward,
            referee_discount,
            referrer_reward,
            protocol_fee,
            ..
        } = calculate_fee_for_fulfillment_with_amm(
            &taker_stats,
            quote_asset_amount,
            &fee_structure,
            0,
            60,
            false,
            true,
            false,
            0,
            false,
            -50,
            None,
            false,
            0,
            0,
            0,
            SlotClock::baseline(),
            0,
        )
        .unwrap();

        assert_eq!(user_fee, 45000);
        assert_eq!(fee_to_market, 0);
        assert_eq!(protocol_fee, 40000);
        assert_eq!(filler_reward, 0);
        assert_eq!(referrer_reward, 5000);
        assert_eq!(referee_discount, 5000);

        // reward referrer + filler
        let FillFees {
            user_fee,
            fee_to_market,
            filler_reward,
            referee_discount,
            referrer_reward,
            protocol_fee,
            ..
        } = calculate_fee_for_fulfillment_with_amm(
            &taker_stats,
            quote_asset_amount,
            &fee_structure,
            0,
            60,
            true,
            true,
            false,
            0,
            false,
            -50,
            None,
            false,
            0,
            0,
            0,
            SlotClock::baseline(),
            0,
        )
        .unwrap();

        assert_eq!(user_fee, 45000);
        assert_eq!(fee_to_market, 0);
        assert_eq!(protocol_fee, 35500);
        assert_eq!(filler_reward, 4500);
        assert_eq!(referrer_reward, 5000);
        assert_eq!(referee_discount, 5000);
    }

    #[test]
    fn taker_fee_addon() {
        let quote_asset_amount = 100 * QUOTE_PRECISION_U64;

        let taker_stats = UserStats::default();
        let fee_structure = FeeStructure::test_default();

        // +1.5bp addon on the 10bps tier fee
        let FillFees {
            user_fee,
            protocol_fee,
            ..
        } = calculate_fee_for_fulfillment_with_amm(
            &taker_stats,
            quote_asset_amount,
            &fee_structure,
            0,
            60,
            false,
            false,
            false,
            0,
            false,
            0,
            None,
            false,
            15,
            0,
            0,
            SlotClock::baseline(),
            0,
        )
        .unwrap();
        assert_eq!(user_fee, 115000);
        assert_eq!(protocol_fee, 115000);

        // ordering: (tier + addon) scaled by fee_adjustment, not the reverse
        // ((100000 + 15000) * 0.5 = 57500; addon-after-adjustment would be 65000)
        let FillFees { user_fee, .. } = calculate_fee_for_fulfillment_with_amm(
            &taker_stats,
            quote_asset_amount,
            &fee_structure,
            0,
            60,
            false,
            false,
            false,
            0,
            false,
            -50,
            None,
            false,
            15,
            0,
            0,
            SlotClock::baseline(),
            0,
        )
        .unwrap();
        assert_eq!(user_fee, 57500);

        // match path at the max addon: the surcharge only grows the taker
        // fee, so the remainder always funds the full maker rebate (addon is
        // unsigned precisely so this can never underflow and revert fills)
        let FillFees {
            user_fee,
            maker_rebate,
            fee_to_market,
            protocol_fee,
            ..
        } = calculate_fee_for_fulfillment_with_match(
            &taker_stats,
            &None,
            quote_asset_amount,
            &fee_structure,
            0,
            60,
            0,
            false,
            false,
            &MarketType::Perp,
            0,
            None,
            MAX_TAKER_FEE_ADDON_TENTH_BPS,
            0,
            0,
            SlotClock::baseline(),
            0,
        )
        .unwrap();
        // 10bps tier fee + 10bps max addon
        assert_eq!(user_fee, 200000);
        assert_eq!(maker_rebate, 60000);
        assert_eq!(fee_to_market, 0);
        assert_eq!(protocol_fee, 140000);
    }

    #[test]
    fn vamm_maker_rebate() {
        let quote_asset_amount = 100 * QUOTE_PRECISION_U64;

        let taker_stats = UserStats::default();
        let fee_structure = FeeStructure::test_default();

        // flag on: 6bps rebate carved off the remainder, folded into amm_fee
        let FillFees {
            user_fee,
            fee_to_market,
            maker_rebate,
            protocol_fee,
            if_fee,
            amm_fee,
            ..
        } = calculate_fee_for_fulfillment_with_amm(
            &taker_stats,
            quote_asset_amount,
            &fee_structure,
            0,
            60,
            false,
            false,
            false,
            0,
            false,
            0,
            None,
            true,
            0,
            0,
            0,
            SlotClock::baseline(),
            0,
        )
        .unwrap();

        assert_eq!(user_fee, 100000);
        assert_eq!(amm_fee, 60000);
        assert_eq!(fee_to_market, 60000);
        assert_eq!(protocol_fee, 40000);
        assert_eq!(if_fee, 0);
        // the rebate is the AMM's, not a user maker's
        assert_eq!(maker_rebate, 0);

        // rebate stacks with a nonzero amm/if split of the residual
        let mut split_structure = FeeStructure::test_default();
        split_structure.amm_fee_numerator = 20;
        split_structure.if_fee_numerator = 10;

        let FillFees {
            user_fee,
            fee_to_market,
            protocol_fee,
            if_fee,
            amm_fee,
            ..
        } = calculate_fee_for_fulfillment_with_amm(
            &taker_stats,
            quote_asset_amount,
            &split_structure,
            0,
            60,
            false,
            false,
            false,
            0,
            false,
            0,
            None,
            true,
            0,
            0,
            0,
            SlotClock::baseline(),
            0,
        )
        .unwrap();

        assert_eq!(user_fee, 100000);
        assert_eq!(amm_fee, 68000);
        assert_eq!(fee_to_market, 68000);
        assert_eq!(if_fee, 4000);
        assert_eq!(protocol_fee, 28000);

        // rebate larger than the remainder clamps instead of underflowing
        let mut inverted_structure = FeeStructure::test_default();
        inverted_structure.fee_tiers[0].maker_rebate_numerator = 200;

        let FillFees {
            user_fee,
            fee_to_market,
            protocol_fee,
            if_fee,
            amm_fee,
            ..
        } = calculate_fee_for_fulfillment_with_amm(
            &taker_stats,
            quote_asset_amount,
            &inverted_structure,
            0,
            60,
            false,
            false,
            false,
            0,
            false,
            0,
            None,
            true,
            0,
            0,
            0,
            SlotClock::baseline(),
            0,
        )
        .unwrap();

        assert_eq!(user_fee, 100000);
        assert_eq!(amm_fee, 100000);
        assert_eq!(fee_to_market, 100000);
        assert_eq!(if_fee, 0);
        assert_eq!(protocol_fee, 0);

        // the rebate is pinned to tier 0, not the taker's tier
        let mut tiered_structure = FeeStructure::test_default();
        tiered_structure.fee_tiers[1] = tiered_structure.fee_tiers[0];
        tiered_structure.fee_tiers[1].fee_numerator = 80;
        tiered_structure.fee_tiers[1].maker_rebate_numerator = 30;

        let mut tiered_taker_stats = UserStats::default();
        tiered_taker_stats.taker_volume_30d = 5_000_000 * QUOTE_PRECISION_U64; // tier 1

        let FillFees {
            user_fee,
            fee_to_market,
            protocol_fee,
            if_fee,
            amm_fee,
            ..
        } = calculate_fee_for_fulfillment_with_amm(
            &tiered_taker_stats,
            quote_asset_amount,
            &tiered_structure,
            0,
            60,
            false,
            false,
            false,
            0,
            false,
            0,
            None,
            true,
            0,
            0,
            0,
            SlotClock::baseline(),
            0,
        )
        .unwrap();

        // taker pays tier 1's 8bps; the AMM earns tier 0's 6bps rebate,
        // not tier 1's 3bps
        assert_eq!(user_fee, 80000);
        assert_eq!(amm_fee, 60000);
        assert_eq!(fee_to_market, 60000);
        assert_eq!(if_fee, 0);
        assert_eq!(protocol_fee, 20000);
    }
}

mod calcuate_fee_tiers {

    use crate::{
        math::{
            constants::{FEE_DENOMINATOR, FEE_PERCENTAGE_DENOMINATOR, QUOTE_PRECISION_U64},
            fees::{determine_user_fee_tier, OrderFillerRewardStructure},
        },
        state::{
            state::{FeeStructure, FeeTier},
            user::{MarketType, UserStats},
        },
    };

    #[test]
    fn test_calc_taker_tiers() {
        let mut taker_stats = UserStats::default();
        let mut fee_tiers = [FeeTier::default(); 10];

        fee_tiers[0] = FeeTier {
            fee_numerator: 35,
            fee_denominator: FEE_DENOMINATOR, // 3.5 bps
            maker_rebate_numerator: 25,
            maker_rebate_denominator: FEE_DENOMINATOR * 10, // .25 bps
            referrer_reward_numerator: 10,
            referrer_reward_denominator: FEE_PERCENTAGE_DENOMINATOR, // 10% of taker fee
            referee_fee_numerator: 5,
            referee_fee_denominator: FEE_PERCENTAGE_DENOMINATOR, // 5%
        };
        fee_tiers[1] = FeeTier {
            fee_numerator: 30,
            fee_denominator: FEE_DENOMINATOR, // 3 bps
            maker_rebate_numerator: 25,
            maker_rebate_denominator: FEE_DENOMINATOR * 10, // .25 bps
            referrer_reward_numerator: 10,
            referrer_reward_denominator: FEE_PERCENTAGE_DENOMINATOR, // 10% of taker fee
            referee_fee_numerator: 5,
            referee_fee_denominator: FEE_PERCENTAGE_DENOMINATOR, // 5%
        };
        fee_tiers[2] = FeeTier {
            fee_numerator: 275,
            fee_denominator: FEE_DENOMINATOR * 10, // 2.75 bps
            maker_rebate_numerator: 25,
            maker_rebate_denominator: FEE_DENOMINATOR * 10, // .25 bps
            referrer_reward_numerator: 10,
            referrer_reward_denominator: FEE_PERCENTAGE_DENOMINATOR, // 10% of taker fee
            referee_fee_numerator: 5,
            referee_fee_denominator: FEE_PERCENTAGE_DENOMINATOR, // 5%
        };
        fee_tiers[3] = FeeTier {
            fee_numerator: 25,
            fee_denominator: FEE_DENOMINATOR, // 2.5 bps
            maker_rebate_numerator: 25,
            maker_rebate_denominator: FEE_DENOMINATOR * 10, // .25 bps
            referrer_reward_numerator: 10,
            referrer_reward_denominator: FEE_PERCENTAGE_DENOMINATOR, // 10% of taker fee
            referee_fee_numerator: 5,
            referee_fee_denominator: FEE_PERCENTAGE_DENOMINATOR, // 5%
        };
        fee_tiers[4] = FeeTier {
            fee_numerator: 225,
            fee_denominator: FEE_DENOMINATOR * 10, // 2.25 bps
            maker_rebate_numerator: 25,
            maker_rebate_denominator: FEE_DENOMINATOR * 10, // .25 bps
            referrer_reward_numerator: 10,
            referrer_reward_denominator: FEE_PERCENTAGE_DENOMINATOR, // 10% of taker fee
            referee_fee_numerator: 5,
            referee_fee_denominator: FEE_PERCENTAGE_DENOMINATOR, // 5%
        };
        fee_tiers[5] = FeeTier {
            fee_numerator: 20,
            fee_denominator: FEE_DENOMINATOR, // 2 bps
            maker_rebate_numerator: 25,
            maker_rebate_denominator: FEE_DENOMINATOR * 10, // .25 bps
            referrer_reward_numerator: 10,
            referrer_reward_denominator: FEE_PERCENTAGE_DENOMINATOR, // 10% of taker fee
            referee_fee_numerator: 5,
            referee_fee_denominator: FEE_PERCENTAGE_DENOMINATOR, // 5%
        };
        let fee_structure = FeeStructure {
            fee_tiers,
            filler_reward_structure: OrderFillerRewardStructure {
                time_based_reward_lower_bound: 10_000, // 1 cent
                reward_numerator: 10,
                reward_denominator: FEE_PERCENTAGE_DENOMINATOR,
                _padding: [0; 8],
            },
            flat_filler_fee: 10_000,
            amm_fee_numerator: 0,
            if_fee_numerator: 0,
        };

        // no volume -> tier 0
        let res =
            determine_user_fee_tier(&taker_stats, &fee_structure, &MarketType::Perp, 0, 0).unwrap();
        assert_eq!(res.fee_numerator, 35);
        assert_eq!(res.maker_rebate_numerator, 25);
        assert_eq!(res.maker_rebate_denominator, 1000000);

        // below the 5M threshold -> still tier 0
        taker_stats.taker_volume_30d = 4_999_999 * QUOTE_PRECISION_U64;
        let res =
            determine_user_fee_tier(&taker_stats, &fee_structure, &MarketType::Perp, 0, 0).unwrap();
        assert_eq!(res.fee_numerator, 35);

        // 5M-80M -> tier 1
        taker_stats.taker_volume_30d = 70_000_000 * QUOTE_PRECISION_U64;
        let res =
            determine_user_fee_tier(&taker_stats, &fee_structure, &MarketType::Perp, 0, 0).unwrap();
        assert_eq!(res.fee_numerator, 30);

        // above 80M -> tier 2
        taker_stats.taker_volume_30d = 280_000_000 * QUOTE_PRECISION_U64;
        let res =
            determine_user_fee_tier(&taker_stats, &fee_structure, &MarketType::Perp, 0, 0).unwrap();
        assert_eq!(res.fee_numerator, 275);

        // maker volume counts toward the total too
        taker_stats.taker_volume_30d = 3_000_000 * QUOTE_PRECISION_U64;
        taker_stats.maker_volume_30d = 3_000_000 * QUOTE_PRECISION_U64;
        let res =
            determine_user_fee_tier(&taker_stats, &fee_structure, &MarketType::Perp, 0, 0).unwrap();
        assert_eq!(res.fee_numerator, 30);
    }

    #[test]
    fn promo_fee_tier_floor() {
        let mut taker_stats = UserStats::default();
        let fee_structure = FeeStructure::perps_default();

        // promo 0 = disabled: pure volume tier
        let res =
            determine_user_fee_tier(&taker_stats, &fee_structure, &MarketType::Perp, 0, 0).unwrap();
        assert_eq!(res.fee_numerator, 40);

        // promo forces tier 2 floor for a zero-volume account
        let res =
            determine_user_fee_tier(&taker_stats, &fee_structure, &MarketType::Perp, 0, 2).unwrap();
        assert_eq!(res.fee_numerator, 20);

        // out-of-range promo clamps to the top tier (defensive only:
        // update_promo_fee_tier validates against PERP_FEE_TIER_MAX_INDEX,
        // so a stored value above it is unreachable via the admin ix)
        let res =
            determine_user_fee_tier(&taker_stats, &fee_structure, &MarketType::Perp, 0, 9).unwrap();
        assert_eq!(res.fee_numerator, 20);

        // account already above the promo floor keeps its volume tier
        taker_stats.taker_volume_30d = 100_000_000 * QUOTE_PRECISION_U64;
        let res =
            determine_user_fee_tier(&taker_stats, &fee_structure, &MarketType::Perp, 0, 1).unwrap();
        assert_eq!(res.fee_numerator, 20);
    }

    #[test]
    fn tier_volume_decays_at_read_time() {
        let mut taker_stats = UserStats::default();
        let fee_structure = FeeStructure::perps_default();

        // 10M traded, last update at t=0 -> tier 1 when read at the same time
        taker_stats.taker_volume_30d = 10_000_000 * QUOTE_PRECISION_U64;
        taker_stats.last_taker_volume_30d_ts = 0;
        let res =
            determine_user_fee_tier(&taker_stats, &fee_structure, &MarketType::Perp, 0, 0).unwrap();
        assert_eq!(res.fee_numerator, 30);

        // 15 days idle: projection halves the sum to 5M -> still tier 1 (boundary)
        let fifteen_days: i64 = 60 * 60 * 24 * 15;
        let res = determine_user_fee_tier(
            &taker_stats,
            &fee_structure,
            &MarketType::Perp,
            fifteen_days,
            0,
        )
        .unwrap();
        assert_eq!(res.fee_numerator, 30);

        // 16 days idle: below 5M -> demoted to tier 0 without any write
        let sixteen_days: i64 = 60 * 60 * 24 * 16;
        let res = determine_user_fee_tier(
            &taker_stats,
            &fee_structure,
            &MarketType::Perp,
            sixteen_days,
            0,
        )
        .unwrap();
        assert_eq!(res.fee_numerator, 40);

        // 30+ days idle: volume fully rolled off
        let forty_days: i64 = 60 * 60 * 24 * 40;
        let res = determine_user_fee_tier(
            &taker_stats,
            &fee_structure,
            &MarketType::Perp,
            forty_days,
            0,
        )
        .unwrap();
        assert_eq!(res.fee_numerator, 40);
    }
}

/// R5: what the cranker earns for resolving a taker-origin cross, and the
/// invariant that bounds it — the taker's net after the taker fee and the
/// reward must beat the price it was resting at.
mod taker_origin_cross_fee {
    use crate::{
        controller::position::PositionDirection,
        error::ErrorCode,
        math::{constants::QUOTE_PRECISION_U64, fees::calculate_taker_origin_cross_fee},
        state::state::{FeeStructure, FeeTier},
    };

    /// One unit of base at $101 rested / $99 crossed, one slot old, at the
    /// oracle (multiplier 1x). `test_default` charges 10bps taker.
    fn fee(
        direction: PositionDirection,
        rest_quote: u64,
        counterparty_quote: u64,
    ) -> crate::error::VelocityResult<crate::math::fees::TakerOriginCrossFee> {
        let fee_structure = FeeStructure::test_default();
        calculate_taker_origin_cross_fee(
            direction,
            rest_quote,
            counterparty_quote,
            &fee_structure.fee_tiers[0],
            0,
            0,
            0,
            1,
            crate::math::time::SlotClock::baseline(),
            1_000,
            &fee_structure.filler_reward_structure,
        )
    }

    /// A taker-origin bid resting at 101 crossed by an ask at 99: the taker
    /// keeps $2 less the cranker's cut, and the cut is the ordinary filler
    /// reward — 10% of the taker fee on the price it actually filled at.
    #[test]
    fn the_cranker_is_paid_out_of_the_improvement() {
        let rest = 101 * QUOTE_PRECISION_U64;
        let cross = 99 * QUOTE_PRECISION_U64;
        let fee = fee(PositionDirection::Long, rest, cross).unwrap();

        assert_eq!(fee.improvement, 2 * QUOTE_PRECISION_U64);
        // Improvement plus the fee saved on the smaller notional (101_000 at
        // 10bps vs 99_000).
        assert_eq!(fee.budget, 2 * QUOTE_PRECISION_U64 + 2_000);
        assert_eq!(fee.crank_reward, 9_900, "10% of the 99_000 taker fee");
        assert_eq!(fee.taker_surplus().unwrap(), 1_992_100);
        assert!(
            fee.taker_surplus().unwrap() > 0,
            "the invariant: crossing beats resting"
        );
    }

    /// The sell side, where the better price is the *bigger* notional and so
    /// carries the bigger fee: the improvement is net of that difference.
    #[test]
    fn selling_pays_the_fee_on_the_better_price() {
        let rest = 99 * QUOTE_PRECISION_U64;
        let cross = 101 * QUOTE_PRECISION_U64;
        let fee = fee(PositionDirection::Short, rest, cross).unwrap();

        assert_eq!(fee.improvement, 2 * QUOTE_PRECISION_U64);
        assert_eq!(
            fee.budget,
            2 * QUOTE_PRECISION_U64 - 2_000,
            "the extra fee on the larger notional comes out of the improvement"
        );
        assert_eq!(
            fee.crank_reward, 10_000,
            "the time-based allowance (1 cent at 1x, one slot old) is the              smaller half of the reward here, so it is what the cranker gets"
        );
        assert!(fee.taker_surplus().unwrap() > 0);
    }

    /// The counterparty's price equals the rest price: nothing to share, so
    /// the cross resolves for free rather than staying gated. The taker gets
    /// the fill it asked for at a price it had already accepted.
    #[test]
    fn a_wash_resolves_for_free() {
        let quote = 100 * QUOTE_PRECISION_U64;
        let fee = fee(PositionDirection::Long, quote, quote).unwrap();

        assert_eq!(fee.improvement, 0);
        assert_eq!(fee.budget, 0);
        assert_eq!(fee.crank_reward, 0);
        assert_eq!(fee.taker_surplus().unwrap(), 0);
    }

    /// An improvement too small to cover the reward pays nothing rather than a
    /// shaved reward: the cranker's revenue stays predictable and the taker
    /// keeps every unit of a gain that could not have funded one. The cross
    /// still resolves — refusing it would leave the remainder gated with
    /// nothing able to clear the gate.
    #[test]
    fn dust_improvement_resolves_without_paying_the_cranker() {
        let rest = 100 * QUOTE_PRECISION_U64;
        let fee = fee(PositionDirection::Long, rest, rest - 10).unwrap();

        assert_eq!(fee.improvement, 10);
        assert_eq!(fee.budget, 10);
        assert_eq!(
            fee.crank_reward, 0,
            "9_999 of reward does not fit in 10 of budget"
        );
        assert_eq!(fee.taker_surplus().unwrap(), 10);
    }

    /// The boundary, on a $100 notional either side of a ~1-cent reward:
    /// nothing paid until the improvement clears it, and the taker keeps the
    /// difference once it does. The reward never comes out shaved.
    #[test]
    fn the_reward_is_paid_in_full_or_not_at_all() {
        let rest = 100 * QUOTE_PRECISION_U64;
        let under = fee(PositionDirection::Long, rest, rest - 9_000).unwrap();
        assert_eq!(under.budget, 9_009);
        assert_eq!(under.crank_reward, 0, "9_999 of reward does not fit");
        assert_eq!(under.taker_surplus().unwrap(), 9_009);

        let over = fee(PositionDirection::Long, rest, rest - 11_000).unwrap();
        assert_eq!(over.budget, 11_011);
        assert_eq!(over.crank_reward, 9_998);
        assert_eq!(
            over.taker_surplus().unwrap(),
            1_013,
            "the taker keeps what the reward did not take"
        );
    }

    /// The invariant's hard edge: a taker fee that eats more than the
    /// improvement makes crossing worse than resting, and is refused rather
    /// than resolved for free. Only a schedule charging more than the whole
    /// trade can get here.
    #[test]
    fn a_cross_worse_than_resting_is_refused() {
        let fee_structure = FeeStructure::test_default();
        let err = calculate_taker_origin_cross_fee(
            PositionDirection::Short,
            100 * QUOTE_PRECISION_U64,
            101 * QUOTE_PRECISION_U64,
            &FeeTier {
                fee_numerator: 200_000,
                fee_denominator: 100_000,
                ..fee_structure.fee_tiers[0]
            },
            0,
            0,
            0,
            1,
            crate::math::time::SlotClock::baseline(),
            1_000,
            &fee_structure.filler_reward_structure,
        )
        .unwrap_err();
        assert_eq!(err, ErrorCode::TakerOriginCrossWorseForTaker);
    }
}
