mod calculate_fee_for_taker_and_maker {
    use crate::{
        math::{
            constants::QUOTE_PRECISION_U64,
            fees::{calculate_fee_for_fulfillment_with_match, FillFees},
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
            &MarketType::Perp,
            0,
            None,
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
            &MarketType::Perp,
            0,
            None,
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
            &MarketType::Perp,
            0,
            None,
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
            &MarketType::Perp,
            0,
            None,
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
            &MarketType::Perp,
            0,
            None,
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
            &MarketType::Perp,
            -50,
            None,
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
            &MarketType::Perp,
            50,
            None,
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
            &MarketType::Perp,
            -50,
            None,
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
            &MarketType::Perp,
            -50,
            None,
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
            &MarketType::Perp,
            -100,
            None,
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
            &MarketType::Perp,
            -100,
            None,
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
            &MarketType::Perp,
            -100,
            None,
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
            &MarketType::Perp,
            -100,
            None,
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
            &MarketType::Perp,
            -100,
            None,
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
            constants::QUOTE_PRECISION_U64,
            fees::{calculate_fee_for_fulfillment_with_amm, FillFees},
        },
        state::{state::FeeStructure, user::UserStats},
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
            0,
            false,
            0,
            None,
            false,
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
            0,
            false,
            -50,
            None,
            false,
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
            0,
            false,
            50,
            None,
            false,
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
            0,
            false,
            -50,
            None,
            false,
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
            0,
            false,
            -50,
            None,
            false,
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
            0,
            false,
            0,
            None,
            true,
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
            0,
            false,
            0,
            None,
            true,
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
            0,
            false,
            0,
            None,
            true,
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
            0,
            false,
            0,
            None,
            true,
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

        let res = determine_user_fee_tier(&taker_stats, &fee_structure, &MarketType::Perp).unwrap();
        assert_eq!(res.fee_numerator, 35);
        assert_eq!(res.fee_denominator, 100000);

        assert_eq!(res.maker_rebate_numerator, 25);
        assert_eq!(res.maker_rebate_denominator, 1000000);

        taker_stats.taker_volume_30d = 70_000_000 * QUOTE_PRECISION_U64;

        let res: FeeTier =
            determine_user_fee_tier(&taker_stats, &fee_structure, &MarketType::Perp).unwrap();
        assert_eq!(res.fee_numerator, 25);
        assert_eq!(res.fee_denominator, 100000);

        assert_eq!(res.maker_rebate_numerator, 25);
        assert_eq!(res.maker_rebate_denominator, 1000000);

        taker_stats.taker_volume_30d = 280_000_000 * QUOTE_PRECISION_U64;
        let res: FeeTier =
            determine_user_fee_tier(&taker_stats, &fee_structure, &MarketType::Perp).unwrap();

        assert_eq!(res.fee_numerator, 20);
        assert_eq!(res.fee_denominator, 100000);

        assert_eq!(res.maker_rebate_numerator, 25);
        assert_eq!(res.maker_rebate_denominator, 1000000);
    }
}
