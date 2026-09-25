#[cfg(test)]
mod test {
    use crate::{
        error::VelocityResult,
        math::constants::{
            AMM_RESERVE_PRECISION, BASE_PRECISION_I128, BID_ASK_SPREAD_PRECISION,
            BID_ASK_SPREAD_PRECISION_I64, LAZER_CONF_FLOOR_PCT, PRICE_PRECISION_U64,
            QUOTE_PRECISION, QUOTE_PRECISION_I128, REFERENCE_PRICE_OFFSET_FULL_INVENTORY_PCT,
        },
        state::perp_market::{PerpMarket, AMM},
        vlp::amm::math::{amm::calculate_price, spread::*},
    };

    /// Test-local shim keeping the legacy 23-scalar `calculate_spread`
    /// signature: builds the `AMM` + `SpreadInputs` the refactored function
    /// takes, so the scalar-to-field mapping lives in exactly one place
    /// instead of being transcribed at every call site. Shadows the glob
    /// import of the real function.
    #[allow(clippy::too_many_arguments)]
    fn calculate_spread(
        base_spread: u32,
        last_oracle_reserve_price_spread_pct: i64,
        last_oracle_conf_pct: u64,
        max_spread: u32,
        quote_asset_reserve: u128,
        terminal_quote_asset_reserve: u128,
        peg_multiplier: u128,
        base_asset_amount_with_amm: i128,
        reserve_price: u64,
        total_fee_minus_distributions: i128,
        net_revenue_since_last_funding: i64,
        base_asset_reserve: u128,
        min_base_asset_reserve: u128,
        max_base_asset_reserve: u128,
        mark_std: u64,
        oracle_std: u64,
        long_intensity_volume: u64,
        short_intensity_volume: u64,
        volume_24h: u64,
        amm_inventory_spread_adjustment: i8,
        last_24h_avg_funding_rate: i64,
        last_funding_oracle_twap: i64,
        funding_bias_sensitivity: u8,
    ) -> VelocityResult<(u32, u32)> {
        let amm = AMM {
            base_spread,
            max_spread,
            quote_asset_reserve,
            terminal_quote_asset_reserve,
            peg_multiplier,
            base_asset_amount_with_amm,
            total_fee_minus_distributions,
            net_revenue_since_last_funding,
            base_asset_reserve,
            min_base_asset_reserve,
            max_base_asset_reserve,
            amm_inventory_spread_adjustment,
            funding_bias_sensitivity,
            ..AMM::default()
        };
        let inputs = SpreadInputs {
            last_oracle_conf_pct,
            mark_std,
            oracle_std,
            long_intensity_volume,
            short_intensity_volume,
            volume_24h,
            last_24h_avg_funding_rate,
            last_funding_oracle_twap,
        };
        crate::vlp::amm::math::spread::calculate_spread(
            &amm,
            &inputs,
            reserve_price,
            last_oracle_reserve_price_spread_pct,
        )
    }

    #[test]
    fn max_spread_tests() {
        let (l, s) = cap_to_max_spread(3905832905, 3582930, 1000).unwrap();
        assert_eq!(l, 1000);
        assert_eq!(s, 0);

        let (l, s) = cap_to_max_spread(9999, 1, 1000).unwrap();
        assert_eq!(l, 1000);
        assert_eq!(s, 0);

        let (l, s) = cap_to_max_spread(999, 1, 1000).unwrap();
        assert_eq!(l, 999);
        assert_eq!(s, 1);

        let (l, s) = cap_to_max_spread(444, 222, 1000).unwrap();
        assert_eq!(l, 444);
        assert_eq!(s, 222);

        let (l, s) = cap_to_max_spread(150, 2221, 1000).unwrap();
        assert_eq!(l, 63);
        assert_eq!(s, 1000 - 63);

        let (l, s) = cap_to_max_spread(2500 - 10, 11, 2500).unwrap();
        assert_eq!(l, 2490);
        assert_eq!(s, 10);

        let (l, s) = cap_to_max_spread(2510, 110, 2500).unwrap();
        assert_eq!(l, 2396);
        assert_eq!(s, 104);
    }

    #[test]
    fn spread_reserve_delta_uses_exact_composite_spread() {
        let amm = AMM {
            quote_asset_reserve: 2_000_000_000,
            sqrt_k: 2_000_000_000,
            ..AMM::default()
        };

        let (_, quote_long_one) =
            compute_spread_reserves_for_direction(&amm, 1, 0, PositionDirection::Long).unwrap();
        let (_, quote_short_one) =
            compute_spread_reserves_for_direction(&amm, 1, 0, PositionDirection::Short).unwrap();
        assert_eq!(quote_long_one, 2_000_001_000);
        assert_eq!(quote_short_one, 1_999_999_000);

        let (_, quote_long_odd) =
            compute_spread_reserves_for_direction(&amm, 3, 0, PositionDirection::Long).unwrap();
        assert_eq!(quote_long_odd, 2_000_003_000);

        let (_, quote_long_wide) =
            compute_spread_reserves_for_direction(&amm, 600_000, 0, PositionDirection::Long)
                .unwrap();
        let (_, quote_short_wide) =
            compute_spread_reserves_for_direction(&amm, 600_000, 0, PositionDirection::Short)
                .unwrap();
        assert_eq!(quote_long_wide, 2_600_000_000);
        assert_eq!(quote_short_wide, 1_400_000_000);
    }

    #[test]
    fn ordered_cap_preserves_priority() {
        // raw = divergence (20, 0) + floor (10, 10) + steering (80, 0)
        //     + padding (50, 50)
        let components = SpreadComponents::from_raw(
            SpreadPair {
                long: 160,
                short: 60,
            },
            SpreadPair { long: 20, short: 0 },
            SpreadPair {
                long: 10,
                short: 10,
            },
            SpreadPair { long: 80, short: 0 },
        )
        .unwrap();

        // Only padding yields: its two sides remain proportional.
        assert_eq!(
            components.cap_total_ordered(180).unwrap(),
            SpreadPair {
                long: 140,
                short: 40,
            }
        );

        // Padding is gone before steering is touched, but the floor remains.
        assert_eq!(
            components.cap_total_ordered(90).unwrap(),
            SpreadPair {
                long: 80,
                short: 10,
            }
        );

        // Divergence is still the last layer compressed when the ceiling
        // cannot hold even the known oracle gap.
        assert_eq!(
            components.cap_total_ordered(10).unwrap(),
            SpreadPair { long: 10, short: 0 }
        );
    }

    #[test]
    fn ordered_cap_handles_opposing_divergence_and_steering() {
        // Divergence protects long while inventory steering protects short.
        let components = SpreadComponents::from_raw(
            SpreadPair {
                long: 80,
                short: 100,
            },
            SpreadPair { long: 60, short: 0 },
            SpreadPair { long: 5, short: 5 },
            SpreadPair { long: 0, short: 50 },
        )
        .unwrap();

        let capped = components.cap_total_ordered(130).unwrap();
        assert_eq!(capped.total().unwrap(), 130);
        assert_eq!(capped.long, 67);
        assert_eq!(capped.short, 63);
        assert!(capped.long >= 60); // full divergence requirement survives
        assert!(capped.short >= 50); // full steering requirement survives
    }

    #[test]
    fn calculate_spread_maps_negative_oracle_gap_to_long_side() {
        let amm = AMM {
            base_spread: 1_000,
            max_spread: 10_000,
            total_fee_minus_distributions: 1,
            ..AMM::default()
        };

        let spread = crate::vlp::amm::math::spread::calculate_spread(
            &amm,
            &SpreadInputs::default(),
            PRICE_PRECISION_U64,
            -5_000,
        )
        .unwrap();

        // Reserve below oracle retreats the long/ask side. The short side
        // remains at its half-base floor.
        assert_eq!(spread, (5_000, 500));
    }

    #[test]
    fn ordered_cap_is_identity_below_ceiling() {
        let raw = SpreadPair {
            long: 12_345,
            short: 6_789,
        };
        let components = SpreadComponents::from_raw(
            raw,
            SpreadPair {
                long: 1_000,
                short: 0,
            },
            SpreadPair {
                long: 100,
                short: 100,
            },
            SpreadPair {
                long: 5_000,
                short: 0,
            },
        )
        .unwrap();

        assert_eq!(components.cap_total_ordered(20_000).unwrap(), raw);
    }

    #[test]
    fn calculate_reference_price_offset_tests() {
        let rev_price = 4216 * 10000;
        let max_offset: i64 = 2500; // 25 bps
                                    // liquidity fractions are PERCENTAGE_PRECISION; the ramp saturates at 10%
        let full = REFERENCE_PRICE_OFFSET_FULL_INVENTORY_PCT;
        assert_eq!(full, 100_000);

        // mark above oracle on both twaps with positive funding: positive premium
        let positive_premium = |liquidity_fraction: i128| {
            calculate_reference_price_offset(
                rev_price,
                1,
                liquidity_fraction,
                4216 * 10000,
                4217 * 10000,
                4216 * 10000,
                4217 * 10000,
                max_offset,
            )
            .unwrap()
        };
        // mark below oracle with negative funding: negative premium
        let negative_premium = |liquidity_fraction: i128| {
            calculate_reference_price_offset(
                rev_price,
                -43_000_000,
                liquidity_fraction,
                4216 * 10000,
                4214 * 10000,
                4216 * 10000,
                4214 * 10000,
                max_offset,
            )
            .unwrap()
        };

        let res =
            calculate_reference_price_offset(rev_price, 0, 0, 0, 0, 0, 0, max_offset).unwrap();
        assert_eq!(res, 0);

        // size comes from inventory alone: linear up to 10% of liquidity
        assert_eq!(positive_premium(10), 0); // 0.001%: the old formula gave 290
        assert_eq!(positive_premium(full / 100), 25); // 0.1%
        assert_eq!(positive_premium(full / 10), 250); // 1%
        assert_eq!(positive_premium(full / 2), 1250); // 5%
        assert_eq!(positive_premium(full), 2500); // 10%
        assert_eq!(positive_premium(full * 5), 2500); // capped past 10%
        assert_eq!(negative_premium(-full / 10), -250);
        assert_eq!(negative_premium(-full), -2500);
        assert_eq!(negative_premium(-full * 5), -2500);

        // the premium magnitude does not size the offset
        let wider_premium = calculate_reference_price_offset(
            rev_price,
            1,
            full / 2,
            4216 * 10000,
            4223 * 10000,
            4216 * 10000,
            4223 * 10000,
            max_offset,
        )
        .unwrap();
        assert_eq!(wider_premium, positive_premium(full / 2));

        // premium and inventory disagree: no offset
        assert_eq!(negative_premium(full / 2), 0);
        assert_eq!(positive_premium(-full / 2), 0);
        let res = calculate_reference_price_offset(
            rev_price,
            -43_000_000,
            full / 2,
            4216 * 10000,
            4218 * 10000,
            4216 * 10000,
            4218 * 10000,
            max_offset,
        )
        .unwrap();
        assert_eq!(res, 0); // funding leg outweighs the twap legs

        // max offset = 0
        let res = calculate_reference_price_offset(
            rev_price,
            -10_000_000,
            -full,
            4216 * 10000,
            4123 * 10000,
            6 * 10000,
            4123 * 10000,
            0,
        )
        .unwrap();
        assert_eq!(res, 0);

        // counteracting fast/slow twaps net to a zero premium: no offset
        let res = calculate_reference_price_offset(
            rev_price,
            -1,
            full,
            4216 * 10000,
            4123 * 10000,
            4123 * 10000,
            4216 * 10000,
            max_offset,
        )
        .unwrap();
        assert_eq!(res, 0);
    }

    // `calculate_reference_price_offset_deadband_tests` was removed: it
    // drove the deleted `update_spreads` mutator and asserted on cached
    // spread fields that no longer exist on AMM. The underlying behaviour
    // is covered by `calculate_reference_price_offset_tests` against the
    // pure helper.

    #[test]
    fn calculate_spread_tests() {
        let base_spread = 1000; // .1%
        let mut last_oracle_reserve_price_spread_pct = 0;
        let mut last_oracle_conf_pct = 0;
        let quote_asset_reserve = AMM_RESERVE_PRECISION * 10;
        let mut terminal_quote_asset_reserve = AMM_RESERVE_PRECISION * 10;
        let peg_multiplier = 34000000;
        let mut base_asset_amount_with_amm = 0;
        let reserve_price = 34562304;
        let mut total_fee_minus_distributions = 0;
        let net_revenue_since_last_funding = 0;

        let base_asset_reserve = AMM_RESERVE_PRECISION * 10;
        let min_base_asset_reserve = 0_u128;
        let max_base_asset_reserve = AMM_RESERVE_PRECISION * 100000;

        let margin_ratio_initial = 2000; // 5x max leverage
        let max_spread = margin_ratio_initial * 100;

        let mark_std = 0;
        let oracle_std = 0;
        let long_intensity_volume = 0;
        let short_intensity_volume = 0;
        let volume_24h = 0;

        // at 0 fee be max spread
        let (long_spread1, short_spread1) = calculate_spread(
            base_spread,
            last_oracle_reserve_price_spread_pct,
            last_oracle_conf_pct,
            max_spread,
            quote_asset_reserve,
            terminal_quote_asset_reserve,
            peg_multiplier,
            base_asset_amount_with_amm,
            reserve_price,
            total_fee_minus_distributions,
            net_revenue_since_last_funding,
            base_asset_reserve,
            min_base_asset_reserve,
            max_base_asset_reserve,
            mark_std,
            oracle_std,
            long_intensity_volume,
            short_intensity_volume,
            volume_24h,
            0,
            0,
            0,
            0,
        )
        .unwrap();
        assert_eq!(long_spread1, (base_spread * 10 / 2));
        assert_eq!(short_spread1, (base_spread * 10 / 2));

        // even at imbalance with 0 fee, be max spread
        terminal_quote_asset_reserve -= AMM_RESERVE_PRECISION;
        base_asset_amount_with_amm += AMM_RESERVE_PRECISION as i128;

        let (long_spread2, short_spread2) = calculate_spread(
            base_spread,
            last_oracle_reserve_price_spread_pct,
            last_oracle_conf_pct,
            base_spread * 20,
            quote_asset_reserve,
            terminal_quote_asset_reserve,
            peg_multiplier,
            base_asset_amount_with_amm,
            reserve_price,
            total_fee_minus_distributions,
            net_revenue_since_last_funding,
            base_asset_reserve,
            min_base_asset_reserve,
            max_base_asset_reserve,
            mark_std,
            oracle_std,
            long_intensity_volume,
            short_intensity_volume,
            volume_24h,
            0,
            0,
            0,
            0,
        )
        .unwrap();
        assert_eq!(long_spread2, 16667);
        assert_eq!(short_spread2, 3333);

        // oracle retreat * skew that increases long spread
        last_oracle_reserve_price_spread_pct = BID_ASK_SPREAD_PRECISION_I64 / 20; //5%
        last_oracle_conf_pct = BID_ASK_SPREAD_PRECISION / 100; //1%
        total_fee_minus_distributions = QUOTE_PRECISION as i128;
        let (long_spread3, short_spread3) = calculate_spread(
            base_spread,
            last_oracle_reserve_price_spread_pct,
            last_oracle_conf_pct,
            max_spread,
            quote_asset_reserve,
            terminal_quote_asset_reserve,
            peg_multiplier,
            base_asset_amount_with_amm,
            reserve_price,
            total_fee_minus_distributions,
            net_revenue_since_last_funding,
            base_asset_reserve,
            min_base_asset_reserve,
            max_base_asset_reserve,
            mark_std,
            oracle_std,
            long_intensity_volume,
            short_intensity_volume,
            volume_24h,
            0,
            0,
            0,
            0,
        )
        .unwrap();

        // a 1% confidence counts as 1% / 20 + (1% - 20bp) = 8500 (the Lazer
        // floor is discounted), which the inventory and leverage scales
        // then widen on the long side
        assert_eq!(long_spread3, 44524);

        // last_oracle_reserve_price_spread_pct + conf retreat: 50000 + 8500
        assert_eq!(short_spread3, 58500);
        assert!(short_spread3 > long_spread3);
        assert_eq!(short_spread3 + long_spread3, 103024);

        last_oracle_reserve_price_spread_pct = -BID_ASK_SPREAD_PRECISION_I64 / 777;
        last_oracle_conf_pct = 1;
        let (long_spread4, short_spread4) = calculate_spread(
            base_spread,
            last_oracle_reserve_price_spread_pct,
            last_oracle_conf_pct,
            max_spread,
            quote_asset_reserve,
            terminal_quote_asset_reserve,
            peg_multiplier,
            base_asset_amount_with_amm,
            reserve_price,
            total_fee_minus_distributions,
            net_revenue_since_last_funding,
            base_asset_reserve,
            min_base_asset_reserve,
            max_base_asset_reserve,
            mark_std,
            oracle_std,
            long_intensity_volume,
            short_intensity_volume,
            volume_24h,
            0,
            0,
            0,
            0,
        )
        .unwrap();
        assert!(short_spread4 < long_spread4);
        // (1000000/777 + 1 )* 1.562 * 2 -> 2012 * 2
        assert_eq!(long_spread4, 33255); // lower one for conf_component change
                                         // base_spread
        assert_eq!(short_spread4, 500);

        // increases to fee pool will decrease long spread (all else equal)
        let (long_spread5, short_spread5) = calculate_spread(
            base_spread,
            last_oracle_reserve_price_spread_pct,
            last_oracle_conf_pct,
            max_spread,
            quote_asset_reserve,
            terminal_quote_asset_reserve,
            peg_multiplier,
            base_asset_amount_with_amm,
            reserve_price,
            total_fee_minus_distributions * 2,
            net_revenue_since_last_funding,
            base_asset_reserve,
            min_base_asset_reserve,
            max_base_asset_reserve,
            mark_std,
            oracle_std,
            long_intensity_volume,
            short_intensity_volume,
            volume_24h,
            0,
            0,
            0,
            0,
        )
        .unwrap();

        assert!(long_spread5 < long_spread4);
        assert_eq!(short_spread5, short_spread4);
        assert_eq!(long_spread5, 27270);
        assert_eq!(short_spread5, 500);

        let amm = AMM {
            base_asset_reserve: 2 * AMM_RESERVE_PRECISION,
            quote_asset_reserve: 2 * AMM_RESERVE_PRECISION,
            sqrt_k: 2 * AMM_RESERVE_PRECISION,
            peg_multiplier: PEG_PRECISION,
            max_spread: 1000,
            curve_update_intensity: 100,
            ..AMM::default()
        };

        let mut market = PerpMarket {
            amm,
            ..PerpMarket::default()
        };

        let max_ref_offset = amm.get_max_reference_price_offset().unwrap();
        assert_eq!(max_ref_offset, 0);

        market.amm.curve_update_intensity = 110;
        let max_ref_offset = market.amm.get_max_reference_price_offset().unwrap();
        assert_eq!(max_ref_offset, 500); // 5 bps

        market.amm.curve_update_intensity = 200;
        let max_ref_offset = market.amm.get_max_reference_price_offset().unwrap();
        assert_eq!(max_ref_offset, 10000); // 100 bps

        market.amm.max_spread = 10000 * 5; // 5%
        let max_ref_offset = market.amm.get_max_reference_price_offset().unwrap();
        assert_eq!(max_ref_offset, 25000); // 250 bps (5% of max spread)

        let orig_price = calculate_price(
            amm.quote_asset_reserve,
            amm.base_asset_reserve,
            amm.peg_multiplier,
        )
        .unwrap();
        assert_eq!(orig_price, 1000000);

        // Spread fields are no longer cached on `AMM`; invoke the pure
        // helper with explicit spread + reference-price-offset to mirror
        // what the old cached values would have produced for this market.
        let test_long_spread = long_spread5;
        let test_short_spread = short_spread5;

        let (bar_l, qar_l) = compute_spread_reserves_for_direction(
            &market.amm,
            test_long_spread,
            0,
            PositionDirection::Long,
        )
        .unwrap();
        let (bar_s, qar_s) = compute_spread_reserves_for_direction(
            &market.amm,
            test_short_spread,
            0,
            PositionDirection::Short,
        )
        .unwrap();

        assert_eq!(bar_s, 2000500125);
        assert_eq!(bar_l, 1973096824);
        assert_eq!(qar_l, 2027270000);
        assert_eq!(qar_s, 1999500000);

        assert!(qar_l > amm.quote_asset_reserve);
        assert!(bar_l < amm.base_asset_reserve);
        assert!(qar_s < amm.quote_asset_reserve);
        assert!(bar_s > amm.base_asset_reserve);

        let l_price = calculate_price(qar_l, bar_l, amm.peg_multiplier).unwrap();
        let s_price = calculate_price(qar_s, bar_s, amm.peg_multiplier).unwrap();
        assert_eq!(l_price, 1027455);
        assert_eq!(s_price, 999500);
        assert!(l_price > s_price);

        let test_ref_offset: i32 = 1000; // 10 bps

        let (bar_l, qar_l) = compute_spread_reserves_for_direction(
            &market.amm,
            test_long_spread,
            test_ref_offset,
            PositionDirection::Long,
        )
        .unwrap();
        let (bar_s, qar_s) = compute_spread_reserves_for_direction(
            &market.amm,
            test_short_spread,
            test_ref_offset,
            PositionDirection::Short,
        )
        .unwrap();

        assert_eq!(amm.quote_asset_reserve, 2000000000);
        assert_eq!(qar_s, 2000500000); // down

        assert!(qar_l > amm.quote_asset_reserve);
        assert!(bar_l < amm.base_asset_reserve);
        assert!(qar_s > amm.quote_asset_reserve);
        assert!(bar_s < amm.base_asset_reserve);
        assert_eq!(bar_s, 1999500124); // up
        assert_eq!(bar_l, 1972124026); // down
        assert_eq!(qar_l, 2028270000); // up

        let l_price = calculate_price(qar_l, bar_l, amm.peg_multiplier).unwrap();
        let s_price = calculate_price(qar_s, bar_s, amm.peg_multiplier).unwrap();
        assert_eq!(l_price, 1028469);
        assert_eq!(s_price, 1000500);
        assert!(l_price > s_price);

        let test_ref_offset: i32 = -1000; // -10 bps
        let (bar_l, qar_l) = compute_spread_reserves_for_direction(
            &market.amm,
            test_long_spread,
            test_ref_offset,
            PositionDirection::Long,
        )
        .unwrap();
        let (bar_s, qar_s) = compute_spread_reserves_for_direction(
            &market.amm,
            test_short_spread,
            test_ref_offset,
            PositionDirection::Short,
        )
        .unwrap();

        assert!(qar_l > amm.quote_asset_reserve);
        assert!(bar_l < amm.base_asset_reserve);
        assert!(qar_s < amm.quote_asset_reserve);
        assert!(bar_s > amm.base_asset_reserve);
        assert_eq!(bar_s, 2001501125); // up
        assert_eq!(bar_l, 1974070582); // up
        assert_eq!(qar_l, 2026270000); // down
        assert_eq!(qar_s, 1998500000); // down

        let l_price = calculate_price(qar_l, bar_l, amm.peg_multiplier).unwrap();
        let s_price = calculate_price(qar_s, bar_s, amm.peg_multiplier).unwrap();
        assert_eq!(l_price, 1026442);
        assert_eq!(s_price, 998500);
        assert!(l_price > s_price);

        let (long_spread_btc, short_spread_btc) = calculate_spread(
            500,
            62099,
            411,
            margin_ratio_initial * 100,
            94280030695,
            94472846843,
            21966868000,
            -193160000,
            21927763871,
            50457675,
            net_revenue_since_last_funding,
            base_asset_reserve,
            min_base_asset_reserve,
            max_base_asset_reserve,
            mark_std,
            oracle_std,
            long_intensity_volume,
            short_intensity_volume,
            volume_24h,
            0,
            0,
            0,
            0,
        )
        .unwrap();

        assert_eq!(long_spread_btc, 250);
        assert_eq!(short_spread_btc, 74117);

        let (long_spread_btc1, short_spread_btc1) = calculate_spread(
            500,
            70719,
            0,
            margin_ratio_initial * 100,
            92113762421,
            92306488219,
            21754071000,
            -193060000,
            21671071573,
            4876326,
            net_revenue_since_last_funding,
            base_asset_reserve,
            min_base_asset_reserve,
            max_base_asset_reserve,
            mark_std,
            oracle_std,
            long_intensity_volume,
            short_intensity_volume,
            volume_24h,
            0,
            0,
            0,
            0,
        )
        .unwrap();

        // Steering consumes every byte above the explicit half-base floor on
        // the healing side.
        assert_eq!(long_spread_btc1, 250);
        assert_eq!(short_spread_btc1, 200000 - long_spread_btc1); // max spread
    }

    #[test]
    fn calculate_spread_inventory_tests() {
        let base_spread = 1000; // .1%
        let last_oracle_reserve_price_spread_pct = 0;
        let last_oracle_conf_pct = 0;
        let quote_asset_reserve = AMM_RESERVE_PRECISION * 9;
        let mut terminal_quote_asset_reserve = AMM_RESERVE_PRECISION * 10;
        let peg_multiplier = 34000000;
        let mut base_asset_amount_with_amm = -(AMM_RESERVE_PRECISION as i128);
        let reserve_price = 34562304;
        let mut total_fee_minus_distributions = 10000 * QUOTE_PRECISION_I128;
        let net_revenue_since_last_funding = 0;

        let base_asset_reserve = AMM_RESERVE_PRECISION * 11;
        let min_base_asset_reserve = AMM_RESERVE_PRECISION * 7;
        let max_base_asset_reserve = AMM_RESERVE_PRECISION * 14;

        let margin_ratio_initial = 2000; // 5x max leverage
        let max_spread = margin_ratio_initial * 100;

        let mark_std = 0;
        let oracle_std = 0;
        let long_intensity_volume = 0;
        let short_intensity_volume = 0;
        let volume_24h = 0;

        let (long_spread1, short_spread1) = calculate_spread(
            base_spread,
            last_oracle_reserve_price_spread_pct,
            last_oracle_conf_pct,
            max_spread,
            quote_asset_reserve,
            terminal_quote_asset_reserve,
            peg_multiplier,
            base_asset_amount_with_amm,
            reserve_price,
            total_fee_minus_distributions,
            net_revenue_since_last_funding,
            base_asset_reserve,
            min_base_asset_reserve,
            max_base_asset_reserve,
            mark_std,
            oracle_std,
            long_intensity_volume,
            short_intensity_volume,
            volume_24h,
            0,
            0,
            0,
            0,
        )
        .unwrap();

        // inventory scale
        let (max_bids, max_asks) = _calculate_market_open_bids_asks(
            base_asset_reserve,
            min_base_asset_reserve,
            max_base_asset_reserve,
        )
        .unwrap();
        assert_eq!(max_bids, 4000000000);
        assert_eq!(max_asks, -3000000000);

        let total_liquidity = max_bids.safe_add(max_asks.abs()).unwrap();
        assert_eq!(total_liquidity, 7000000000);
        // inventory scale
        let inventory_scale = base_asset_amount_with_amm
            .safe_mul(BID_ASK_SPREAD_PRECISION_I128 * 5)
            .unwrap()
            .safe_div(total_liquidity)
            .unwrap()
            .unsigned_abs();
        assert_eq!(inventory_scale, 714285);

        assert_eq!(long_spread1, 500);
        assert_eq!(short_spread1, 67166);

        base_asset_amount_with_amm *= 2;
        let (long_spread1, short_spread1) = calculate_spread(
            base_spread,
            last_oracle_reserve_price_spread_pct,
            last_oracle_conf_pct,
            max_spread,
            quote_asset_reserve,
            terminal_quote_asset_reserve,
            peg_multiplier,
            base_asset_amount_with_amm,
            reserve_price,
            total_fee_minus_distributions,
            net_revenue_since_last_funding,
            base_asset_reserve,
            min_base_asset_reserve,
            max_base_asset_reserve,
            mark_std,
            oracle_std,
            long_intensity_volume,
            short_intensity_volume,
            volume_24h,
            0,
            0,
            0,
            0,
        )
        .unwrap();
        assert_eq!(long_spread1, 500);
        assert_eq!(short_spread1, 133833);

        terminal_quote_asset_reserve = AMM_RESERVE_PRECISION * 11;
        total_fee_minus_distributions = QUOTE_PRECISION_I128 * 5;
        let (long_spread1, short_spread1) = calculate_spread(
            base_spread,
            last_oracle_reserve_price_spread_pct,
            last_oracle_conf_pct,
            max_spread,
            quote_asset_reserve,
            terminal_quote_asset_reserve,
            peg_multiplier,
            base_asset_amount_with_amm,
            reserve_price * 9 / 10,
            total_fee_minus_distributions,
            net_revenue_since_last_funding,
            base_asset_reserve,
            min_base_asset_reserve,
            max_base_asset_reserve,
            mark_std,
            oracle_std,
            long_intensity_volume,
            short_intensity_volume,
            volume_24h,
            0,
            0,
            0,
            0,
        )
        .unwrap();
        assert_eq!(long_spread1, 500);
        assert_eq!(short_spread1, 199500);

        total_fee_minus_distributions = QUOTE_PRECISION_I128;
        let (long_spread1, short_spread1) = calculate_spread(
            base_spread,
            last_oracle_reserve_price_spread_pct,
            last_oracle_conf_pct,
            max_spread,
            quote_asset_reserve,
            terminal_quote_asset_reserve,
            peg_multiplier,
            base_asset_amount_with_amm,
            reserve_price * 9 / 10,
            total_fee_minus_distributions,
            net_revenue_since_last_funding,
            base_asset_reserve,
            min_base_asset_reserve,
            max_base_asset_reserve,
            mark_std,
            oracle_std,
            long_intensity_volume,
            short_intensity_volume,
            volume_24h,
            0,
            0,
            0,
            0,
        )
        .unwrap();
        assert_eq!(long_spread1, 500);
        assert_eq!(short_spread1, 199500);

        // flip sign
        let (d1, _) = calculate_long_short_vol_spread(
            last_oracle_conf_pct, // 0
            reserve_price,
            mark_std,               // 0
            oracle_std,             // 0
            long_intensity_volume,  // 0
            short_intensity_volume, // 0
            volume_24h,             // 0
        )
        .unwrap();
        assert_eq!(d1, 0); // no volatility measured at all from input data -_-

        let iscale = calculate_spread_inventory_scale(
            -base_asset_amount_with_amm,
            base_asset_reserve,
            min_base_asset_reserve,
            max_base_asset_reserve,
            d1,
            max_spread as u64,
        )
        .unwrap();
        assert_eq!(iscale, 133334200000);

        let (long_spread1, short_spread1) = calculate_spread(
            base_spread,
            last_oracle_reserve_price_spread_pct,
            last_oracle_conf_pct,
            max_spread,
            quote_asset_reserve,
            terminal_quote_asset_reserve,
            peg_multiplier,
            -base_asset_amount_with_amm,
            reserve_price * 9 / 10,
            total_fee_minus_distributions,
            net_revenue_since_last_funding,
            base_asset_reserve,
            min_base_asset_reserve,
            max_base_asset_reserve,
            mark_std,
            oracle_std,
            long_intensity_volume,
            short_intensity_volume,
            volume_24h,
            0,
            0,
            0,
            0,
        )
        .unwrap();
        assert_eq!(long_spread1, 199500);
        assert_eq!(short_spread1, max_spread - long_spread1);

        let (long_spread1, short_spread1) = calculate_spread(
            base_spread,
            last_oracle_reserve_price_spread_pct,
            last_oracle_conf_pct,
            max_spread,
            quote_asset_reserve,
            terminal_quote_asset_reserve,
            peg_multiplier,
            -base_asset_amount_with_amm * 5,
            reserve_price * 9 / 10,
            total_fee_minus_distributions,
            net_revenue_since_last_funding,
            base_asset_reserve,
            min_base_asset_reserve,
            max_base_asset_reserve,
            mark_std,
            oracle_std,
            long_intensity_volume,
            short_intensity_volume,
            volume_24h,
            0,
            0,
            0,
            0,
        )
        .unwrap();
        assert_eq!(long_spread1, 199500);
        assert_eq!(short_spread1, max_spread - long_spread1); // max on long

        let (long_spread1, short_spread1) = calculate_spread(
            base_spread,
            last_oracle_reserve_price_spread_pct,
            last_oracle_conf_pct,
            max_spread,
            quote_asset_reserve,
            terminal_quote_asset_reserve,
            peg_multiplier,
            -base_asset_amount_with_amm,
            reserve_price * 9 / 10,
            total_fee_minus_distributions,
            net_revenue_since_last_funding,
            base_asset_reserve,
            min_base_asset_reserve / 2,
            max_base_asset_reserve * 2,
            mark_std,
            oracle_std,
            long_intensity_volume,
            short_intensity_volume,
            volume_24h,
            0,
            0,
            0,
            0,
        )
        .unwrap();
        assert_eq!(long_spread1, 199500);
        assert_eq!(short_spread1, 500);
    }

    #[test]
    fn calculate_spread_inventory_scale_2_tests() {
        assert_eq!(
            calculate_inventory_liquidity_ratio(1, 10, 0, 20,)
                .unwrap()
                .unsigned_abs(),
            PERCENTAGE_PRECISION / 10
        );
        assert_eq!(
            calculate_inventory_liquidity_ratio(1000000000, 10000000000, 0, 20000000000)
                .unwrap()
                .unsigned_abs(),
            PERCENTAGE_PRECISION / 10
        );
        assert_eq!(
            calculate_inventory_liquidity_ratio(-1000000000, 10000000000, 0, 20000000000)
                .unwrap()
                .unsigned_abs(),
            PERCENTAGE_PRECISION / 10
        );
        assert_eq!(
            calculate_inventory_liquidity_ratio(-1000000000, 10000000000, 5000, 20000000000)
                .unwrap()
                .unsigned_abs(),
            PERCENTAGE_PRECISION / 10
        );
        assert_eq!(
            calculate_inventory_liquidity_ratio(-1000000000, 10000000000, 5000000, 20000000000)
                .unwrap()
                .unsigned_abs(),
            100050
        );
        assert_eq!(
            calculate_inventory_liquidity_ratio(-1000000000, 10000000000, 9000000000, 20000000000)
                .unwrap()
                .unsigned_abs(),
            1000000 // 100%
        );
        assert_eq!(
            calculate_inventory_liquidity_ratio(-1000000000, 10000000000, 11000000000, 20000000000)
                .unwrap()
                .unsigned_abs(),
            1000000 // way over but clamped to 100%
        );

        assert_eq!(
            calculate_inventory_liquidity_ratio(
                941291801615,
                443370320987941,
                435296619793629,
                453513306290427
            )
            .unwrap()
            .unsigned_abs(),
            116587 // 11.6587%
        );

        assert_eq!(
            calculate_spread_inventory_scale(
                100000,
                AMM_RESERVE_PRECISION + 100000,
                AMM_RESERVE_PRECISION / 2,
                AMM_RESERVE_PRECISION * 3 / 2,
                250,
                30000,
            )
            .unwrap(),
            1024000
        );

        assert_eq!(
            calculate_spread_inventory_scale(
                30228000000000000,
                2496788386034912600,
                2443167585342470000,
                2545411471321696000,
                3500,
                100000,
            )
            .unwrap(),
            18762285
        );
        assert_eq!(3500_u128 * 18762285_u128 / 1000000_u128, 65667_u128);

        let d1 = 250;
        let max_spread = 300000;
        let iscale = calculate_spread_inventory_scale(
            941291801615,
            443370320987941,
            435296619793629,
            453513306290427,
            d1,
            max_spread,
        )
        .unwrap();

        assert_eq!(max_spread / d1, 1200);
        assert_eq!(iscale / BID_ASK_SPREAD_PRECISION, 140);
        assert_eq!(250 * iscale / BID_ASK_SPREAD_PRECISION, 35226);

        let iscale = calculate_spread_inventory_scale(
            0,
            AMM_RESERVE_PRECISION,
            AMM_RESERVE_PRECISION / 10,
            AMM_RESERVE_PRECISION * 19 / 10,
            250,
            300000,
        )
        .unwrap();
        assert_eq!(iscale, 1_000_000);
        assert_eq!(
            calculate_inventory_liquidity_ratio(
                450000000_i128,
                AMM_RESERVE_PRECISION,
                AMM_RESERVE_PRECISION / 10,
                AMM_RESERVE_PRECISION * 19 / 10,
            )
            .unwrap()
            .unsigned_abs(),
            500000 // 50%
        );
        let iscale = calculate_spread_inventory_scale(
            450000000_i128,
            AMM_RESERVE_PRECISION,
            AMM_RESERVE_PRECISION / 10,
            AMM_RESERVE_PRECISION * 19 / 10,
            250,
            300_000,
        )
        .unwrap();
        assert_eq!(250 * iscale / 1000000, 150250);
        assert_eq!(iscale / 1000000, 601); //601x base spread gets you to half of max spread

        assert_eq!(
            calculate_inventory_liquidity_ratio(
                450000000_i128,
                AMM_RESERVE_PRECISION + 450000000,
                AMM_RESERVE_PRECISION / 10,
                AMM_RESERVE_PRECISION * 19 / 10,
            )
            .unwrap()
            .unsigned_abs(),
            1000000 // 100%
        );
        let iscale = calculate_spread_inventory_scale(
            450000000_i128,
            AMM_RESERVE_PRECISION + 450000000,
            AMM_RESERVE_PRECISION / 10,
            AMM_RESERVE_PRECISION * 19 / 10,
            250,
            300_000,
        )
        .unwrap();
        assert_eq!(250 * iscale / 1000000, 300000);
        assert_eq!(iscale / 1000000, 1200); //1200x base spread gets you to max spread
    }

    #[test]
    fn calculate_spread_leverage_scales_tests() {
        let lscale = calculate_spread_leverage_scale(
            AMM_RESERVE_PRECISION,
            AMM_RESERVE_PRECISION,
            12 * PEG_PRECISION,
            BASE_PRECISION_I128,
            (12.5 * PRICE_PRECISION as f64) as u64,
            QUOTE_PRECISION_I128,
        )
        .unwrap();
        assert_eq!(lscale, 10000000); // 10x

        // more total fee minus dist => lower leverage
        let lscale = calculate_spread_leverage_scale(
            AMM_RESERVE_PRECISION,
            AMM_RESERVE_PRECISION,
            12 * PEG_PRECISION,
            BASE_PRECISION_I128,
            (12.5 * PRICE_PRECISION as f64) as u64,
            QUOTE_PRECISION_I128 * 100,
        )
        .unwrap();
        assert_eq!(lscale, 1125000); // 1.125x

        // less base => lower leverage
        let lscale = calculate_spread_leverage_scale(
            AMM_RESERVE_PRECISION,
            AMM_RESERVE_PRECISION,
            12 * PEG_PRECISION,
            BASE_PRECISION_I128 / 100,
            (12.5 * PRICE_PRECISION as f64) as u64,
            QUOTE_PRECISION_I128,
        )
        .unwrap();
        assert_eq!(lscale, 1125000); // 1.125x (inc)

        // user long => bar < sqrt_k < qar => tqar < qar => peg < reserve_price
        let lscale = calculate_spread_leverage_scale(
            AMM_RESERVE_PRECISION * 1000,
            AMM_RESERVE_PRECISION * 9999 / 10000,
            12 * PEG_PRECISION,
            BASE_PRECISION_I128,
            (12.1 * PRICE_PRECISION as f64) as u64,
            QUOTE_PRECISION_I128,
        )
        .unwrap();
        assert_eq!(lscale, 1000001); // 1.000001x (min)

        // from mainnet 2022/11/22
        let lscale = calculate_spread_leverage_scale(
            455362349720024,
            454386986330347,
            11760127,
            968409950546,
            11869992,
            7978239165,
        )
        .unwrap();
        assert_eq!(lscale, 1003087); // 1.003087x

        let rra = calculate_spread_revenue_retreat_amount(
            250,
            30000,
            (15 * QUOTE_PRECISION_I128 + 835) as i64,
        )
        .unwrap();
        assert_eq!(rra, 0);

        let rra = calculate_spread_revenue_retreat_amount(2150, 30000, 0).unwrap();
        assert_eq!(rra, 0);

        let rra = calculate_spread_revenue_retreat_amount(340, 30000, -1).unwrap();
        assert_eq!(rra, 0);

        let rra = calculate_spread_revenue_retreat_amount(
            250,
            30000,
            (-10 * QUOTE_PRECISION_I128) as i64,
        )
        .unwrap();
        assert_eq!(rra, 0);

        let rra = calculate_spread_revenue_retreat_amount(
            250,
            30000,
            (-91 * QUOTE_PRECISION_I128) as i64,
        )
        .unwrap();
        assert_eq!(rra, 250 * 3 + 160); //every additional dollar adds

        let rra = calculate_spread_revenue_retreat_amount(
            250,
            30000,
            (-14000 * QUOTE_PRECISION_I128) as i64,
        )
        .unwrap();
        assert_eq!(rra, 30000 / 10); //every additional dollar adds
    }

    #[test]
    fn confidence_component_discounts_the_lazer_floor() {
        let floor = LAZER_CONF_FLOOR_PCT;
        assert_eq!(floor, 2000);
        assert_eq!(calculate_spread_conf_component(0).unwrap(), 0);
        // below the floor: 1/20 weight, as upstream Drift
        assert_eq!(calculate_spread_conf_component(1000).unwrap(), 50);
        // at the floor, where Lazer confidence sits in practice: 1bp
        assert_eq!(calculate_spread_conf_component(floor).unwrap(), 100);
        // above it the excess counts at full weight
        assert_eq!(calculate_spread_conf_component(floor + 1).unwrap(), 101);
        assert_eq!(calculate_spread_conf_component(2500).unwrap(), 625);
        assert_eq!(calculate_spread_conf_component(4000).unwrap(), 2200);
        assert_eq!(calculate_spread_conf_component(10000).unwrap(), 8500);
        // meets the raw confidence at 4% and follows it from there
        assert_eq!(calculate_spread_conf_component(40000).unwrap(), 40000);
        assert_eq!(calculate_spread_conf_component(50000).unwrap(), 50000);
        assert_eq!(
            calculate_spread_conf_component(BID_ASK_SPREAD_PRECISION).unwrap(),
            BID_ASK_SPREAD_PRECISION
        );

        // continuous and monotone everywhere, never above the confidence,
        // and no step: one unit of confidence moves it by at most two units
        let mut previous = 0;
        for confidence in 0..=60_000 {
            let component = calculate_spread_conf_component(confidence).unwrap();
            assert!(component >= previous);
            assert!(component - previous <= 2);
            assert!(component <= confidence);
            previous = component;
        }
    }

    #[test]
    fn calculate_vol_spread_tests() {
        let base_spread = 250; // .025%
        let last_oracle_reserve_price_spread_pct = 0;
        let last_oracle_conf_pct = 0;
        let quote_asset_reserve = AMM_RESERVE_PRECISION * 10;
        let terminal_quote_asset_reserve = AMM_RESERVE_PRECISION * 10;
        let peg_multiplier = 34000000;
        let base_asset_amount_with_amm = 0;
        let reserve_price = 34562304;
        let total_fee_minus_distributions = 0;
        let net_revenue_since_last_funding = 0;

        let base_asset_reserve = AMM_RESERVE_PRECISION * 10;
        let min_base_asset_reserve = 0_u128;
        let max_base_asset_reserve = AMM_RESERVE_PRECISION * 100000;

        let margin_ratio_initial = 2000; // 5x max leverage
        let max_spread = margin_ratio_initial * 100; //20%

        let mark_std = 34000000 / 50; // 2% of price
        let oracle_std = 34000000 / 150; // .66% of price
        let long_intensity_volume = (QUOTE_PRECISION * 10000) as u64; //10k
        let short_intensity_volume = (QUOTE_PRECISION * 30000) as u64; //30k
        let volume_24h = (QUOTE_PRECISION * 40000) as u64; // 40k

        let (long_vspread, short_vspread) = calculate_long_short_vol_spread(
            last_oracle_conf_pct,
            reserve_price,
            mark_std,
            oracle_std,
            long_intensity_volume,
            short_intensity_volume,
            volume_24h,
        )
        .unwrap();
        assert_eq!(long_vspread, 819);
        assert_eq!(short_vspread, 2459);

        // since short volume ~= 3 * long volume intensity, expect short spread to be larger by this factor
        assert_eq!(short_vspread >= long_vspread * 3, true);

        // inventory scale
        let (max_bids, max_asks) = _calculate_market_open_bids_asks(
            base_asset_reserve,
            min_base_asset_reserve,
            max_base_asset_reserve,
        )
        .unwrap();
        assert_eq!(max_bids, 10000000000);
        assert_eq!(max_asks, -99990000000000);

        let min_side_liquidity = max_bids.min(max_asks.abs());
        assert_eq!(min_side_liquidity, 10000000000);

        // inventory scale
        let inventory_scale = base_asset_amount_with_amm
            .safe_mul(DEFAULT_LARGE_BID_ASK_FACTOR.cast::<i128>().unwrap())
            .unwrap()
            .safe_div(min_side_liquidity.max(1))
            .unwrap()
            .unsigned_abs();

        assert_eq!(inventory_scale, 0);

        let inventory_scale_capped = min(
            MAX_BID_ASK_INVENTORY_SKEW_FACTOR,
            BID_ASK_SPREAD_PRECISION
                .safe_add(inventory_scale.cast().unwrap())
                .unwrap(),
        );
        assert_eq!(inventory_scale_capped, BID_ASK_SPREAD_PRECISION);

        let (long_spread, short_spread) = calculate_spread(
            base_spread,
            last_oracle_reserve_price_spread_pct,
            last_oracle_conf_pct,
            max_spread,
            quote_asset_reserve,
            terminal_quote_asset_reserve,
            peg_multiplier,
            base_asset_amount_with_amm,
            reserve_price,
            total_fee_minus_distributions,
            net_revenue_since_last_funding,
            base_asset_reserve,
            min_base_asset_reserve,
            max_base_asset_reserve,
            mark_std,
            oracle_std,
            long_intensity_volume,
            short_intensity_volume,
            volume_24h,
            0,
            0,
            0,
            0,
        )
        .unwrap();

        // since total_fee_minus_distributions <=0, 10 * vol spread
        assert_eq!(long_spread, 8190); // vs 2500
        assert_eq!(
            long_spread
                > (base_spread
                    * ((DEFAULT_LARGE_BID_ASK_FACTOR / BID_ASK_SPREAD_PRECISION) as u32)),
            true
        );

        assert_eq!(short_spread, 24590);
        assert_eq!(
            short_spread
                > (base_spread
                    * ((DEFAULT_LARGE_BID_ASK_FACTOR / BID_ASK_SPREAD_PRECISION) as u32)),
            true
        );

        let (long_spread, short_spread) = calculate_spread(
            base_spread,
            last_oracle_reserve_price_spread_pct,
            last_oracle_conf_pct,
            max_spread,
            quote_asset_reserve,
            terminal_quote_asset_reserve,
            peg_multiplier,
            base_asset_amount_with_amm,
            reserve_price,
            total_fee_minus_distributions,
            net_revenue_since_last_funding,
            base_asset_reserve,
            min_base_asset_reserve,
            max_base_asset_reserve,
            mark_std,
            oracle_std,
            long_intensity_volume,
            short_intensity_volume,
            volume_24h,
            -50,
            0,
            0,
            0,
        )
        .unwrap();

        assert_eq!(long_spread, 4095);
        assert_eq!(short_spread, 12295);

        let (long_spread, short_spread) = calculate_spread(
            base_spread,
            last_oracle_reserve_price_spread_pct,
            last_oracle_conf_pct,
            max_spread,
            quote_asset_reserve,
            terminal_quote_asset_reserve,
            peg_multiplier,
            base_asset_amount_with_amm,
            reserve_price,
            total_fee_minus_distributions,
            net_revenue_since_last_funding,
            base_asset_reserve,
            min_base_asset_reserve,
            max_base_asset_reserve,
            mark_std,
            oracle_std,
            long_intensity_volume,
            short_intensity_volume,
            volume_24h,
            -100,
            0,
            0,
            0,
        )
        .unwrap();

        assert_eq!(long_spread, 819);
        assert_eq!(short_spread, 2459);

        let (long_spread, short_spread) = calculate_spread(
            base_spread,
            last_oracle_reserve_price_spread_pct,
            last_oracle_conf_pct,
            max_spread,
            quote_asset_reserve,
            terminal_quote_asset_reserve,
            peg_multiplier,
            base_asset_amount_with_amm,
            reserve_price,
            total_fee_minus_distributions + 1000,
            net_revenue_since_last_funding,
            base_asset_reserve,
            min_base_asset_reserve,
            max_base_asset_reserve,
            mark_std,
            oracle_std,
            long_intensity_volume,
            short_intensity_volume,
            volume_24h,
            0,
            0,
            0,
            0,
        )
        .unwrap();

        assert_eq!(long_spread, 819);
        assert_eq!(short_spread, 2459);

        let (long_spread, short_spread) = calculate_spread(
            base_spread,
            last_oracle_reserve_price_spread_pct,
            last_oracle_conf_pct,
            max_spread,
            quote_asset_reserve,
            terminal_quote_asset_reserve,
            peg_multiplier,
            base_asset_amount_with_amm + BASE_PRECISION_I128,
            reserve_price,
            total_fee_minus_distributions + 1000,
            net_revenue_since_last_funding,
            base_asset_reserve,
            min_base_asset_reserve,
            max_base_asset_reserve,
            mark_std,
            oracle_std,
            long_intensity_volume,
            short_intensity_volume,
            volume_24h,
            0,
            0,
            0,
            0,
        )
        .unwrap();
        assert_eq!(long_spread, 197541);
        assert_eq!(short_spread, 2459);

        let (long_spread, short_spread) = calculate_spread(
            base_spread,
            last_oracle_reserve_price_spread_pct,
            last_oracle_conf_pct,
            max_spread,
            quote_asset_reserve,
            terminal_quote_asset_reserve,
            peg_multiplier,
            base_asset_amount_with_amm - BASE_PRECISION_I128,
            reserve_price,
            total_fee_minus_distributions + 1000,
            net_revenue_since_last_funding,
            base_asset_reserve,
            min_base_asset_reserve,
            max_base_asset_reserve,
            mark_std,
            oracle_std,
            long_intensity_volume,
            short_intensity_volume,
            volume_24h,
            0,
            0,
            0,
            0,
        )
        .unwrap();
        assert_eq!(long_spread, 819);
        assert_eq!(short_spread, 22458);
    }

    #[test]
    fn calculate_vol_oracle_reserve_price_spread_pct_tests() {
        let base_spread = 250; // .025%
        let last_oracle_reserve_price_spread_pct = 5000; //.5%
        let last_oracle_conf_pct = 250; // .025%
        let quote_asset_reserve = AMM_RESERVE_PRECISION * 10;
        let terminal_quote_asset_reserve = AMM_RESERVE_PRECISION * 9;
        let peg_multiplier = 34000000;
        let base_asset_amount_with_amm = 0;
        let reserve_price = 34562304;
        let total_fee_minus_distributions = 0;
        let net_revenue_since_last_funding = 0;

        let base_asset_reserve = AMM_RESERVE_PRECISION * 10;
        let min_base_asset_reserve = AMM_RESERVE_PRECISION * 7;
        let max_base_asset_reserve = AMM_RESERVE_PRECISION * 13;

        let margin_ratio_initial = 2000; // 5x max leverage
        let max_spread = margin_ratio_initial * 100; //20%

        let mark_std = 34000000 / 50; // 2% of price
        let oracle_std = 34000000 / 150; // .66% of price
        let long_intensity_volume = (QUOTE_PRECISION * 10000) as u64; //10k
        let short_intensity_volume = (QUOTE_PRECISION * 30000) as u64; //30k
        let volume_24h = (QUOTE_PRECISION * 40000) as u64; // 40k

        let (long_vspread, short_vspread) = calculate_long_short_vol_spread(
            last_oracle_conf_pct,
            reserve_price,
            mark_std,
            oracle_std,
            long_intensity_volume,
            short_intensity_volume,
            volume_24h,
        )
        .unwrap();
        assert_eq!(long_vspread, 819);
        assert_eq!(short_vspread, 2459);

        // since short volume ~= 3 * long volume intensity, expect short spread to be larger by this factor
        assert_eq!(short_vspread >= long_vspread * 3, true);

        // inventory scale
        let (max_bids, max_asks) = _calculate_market_open_bids_asks(
            base_asset_reserve,
            min_base_asset_reserve,
            max_base_asset_reserve,
        )
        .unwrap();
        assert_eq!(max_bids, 3000000000);
        assert_eq!(max_asks, -3000000000);

        let min_side_liquidity = max_bids.min(max_asks.abs());
        assert_eq!(min_side_liquidity, 3000000000);

        // inventory scale
        let inventory_scale = base_asset_amount_with_amm
            .safe_mul(DEFAULT_LARGE_BID_ASK_FACTOR.cast::<i128>().unwrap())
            .unwrap()
            .safe_div(min_side_liquidity.max(1))
            .unwrap()
            .unsigned_abs();

        assert_eq!(inventory_scale, 0);

        let inventory_scale_capped = min(
            MAX_BID_ASK_INVENTORY_SKEW_FACTOR,
            BID_ASK_SPREAD_PRECISION
                .safe_add(inventory_scale.cast().unwrap())
                .unwrap(),
        );
        assert_eq!(inventory_scale_capped, BID_ASK_SPREAD_PRECISION);

        let (long_spread, short_spread) = calculate_spread(
            base_spread,
            last_oracle_reserve_price_spread_pct,
            last_oracle_conf_pct,
            max_spread,
            quote_asset_reserve,
            terminal_quote_asset_reserve,
            peg_multiplier,
            base_asset_amount_with_amm,
            reserve_price,
            total_fee_minus_distributions,
            net_revenue_since_last_funding,
            base_asset_reserve,
            min_base_asset_reserve,
            max_base_asset_reserve,
            mark_std,
            oracle_std,
            long_intensity_volume,
            short_intensity_volume,
            volume_24h,
            0,
            0,
            0,
            0,
        )
        .unwrap();

        // since total_fee_minus_distributions <=0, 10 * vol spread
        assert_eq!(long_spread, 8190); // vs 2500
        assert_eq!(
            long_spread
                > (base_spread
                    * ((DEFAULT_LARGE_BID_ASK_FACTOR / BID_ASK_SPREAD_PRECISION) as u32)),
            true
        );

        assert_eq!(short_spread, 74590);
        assert_eq!(
            short_spread
                > (base_spread
                    * ((DEFAULT_LARGE_BID_ASK_FACTOR / BID_ASK_SPREAD_PRECISION) as u32)),
            true
        );

        let (long_spread, short_spread) = calculate_spread(
            base_spread,
            last_oracle_reserve_price_spread_pct,
            last_oracle_conf_pct,
            max_spread,
            quote_asset_reserve,
            terminal_quote_asset_reserve,
            peg_multiplier,
            base_asset_amount_with_amm,
            reserve_price,
            total_fee_minus_distributions + 1000,
            net_revenue_since_last_funding,
            base_asset_reserve,
            min_base_asset_reserve,
            max_base_asset_reserve,
            mark_std,
            oracle_std,
            long_intensity_volume,
            short_intensity_volume,
            volume_24h,
            0,
            0,
            0,
            0,
        )
        .unwrap();

        assert_eq!(long_spread, 819);
        assert_eq!(short_spread, 7459);

        let (long_spread, short_spread) = calculate_spread(
            base_spread,
            last_oracle_reserve_price_spread_pct,
            last_oracle_conf_pct,
            max_spread,
            quote_asset_reserve,
            terminal_quote_asset_reserve,
            peg_multiplier,
            base_asset_amount_with_amm + BASE_PRECISION_I128,
            reserve_price,
            total_fee_minus_distributions + 1000,
            net_revenue_since_last_funding,
            base_asset_reserve,
            min_base_asset_reserve,
            max_base_asset_reserve,
            mark_std,
            oracle_std,
            long_intensity_volume,
            short_intensity_volume,
            volume_24h,
            0,
            0,
            0,
            0,
        )
        .unwrap();
        // The 5bp divergence on short is protected first, then its vol floor,
        // before long-side steering receives the residual ceiling.
        assert_eq!(long_spread, 192541);
        assert_eq!(short_spread, 7459);

        let (long_spread, short_spread) = calculate_spread(
            base_spread,
            last_oracle_reserve_price_spread_pct,
            last_oracle_conf_pct,
            max_spread,
            quote_asset_reserve,
            terminal_quote_asset_reserve,
            peg_multiplier,
            base_asset_amount_with_amm - BASE_PRECISION_I128,
            reserve_price,
            total_fee_minus_distributions + 1000,
            net_revenue_since_last_funding,
            base_asset_reserve,
            min_base_asset_reserve,
            max_base_asset_reserve,
            mark_std,
            oracle_std,
            long_intensity_volume,
            short_intensity_volume,
            volume_24h,
            0,
            0,
            0,
            0,
        )
        .unwrap();
        assert_eq!(long_spread, 819);
        assert_eq!(short_spread, 74125); // big
    }

    #[test]
    fn various_spread_tests() {
        // should match typescript sdk tests in sdk/tests/amm/test.ts

        let (long_spread, short_spread) = calculate_spread(
            300,
            0,
            484,
            47500,
            923807816209694,
            925117623772584,
            13731157,
            -1314027016625,
            13667686,
            115876379475,
            91316628,
            928097825691666,
            907979542352912,
            945977491145601,
            161188,
            1459632439,
            12358265776,
            72230366233,
            432067603632,
            0,
            0,
            0,
            0,
        )
        .unwrap();
        assert_eq!(long_spread, 146096);
        assert_eq!(short_spread, 853904);

        // terms 3
        let (long_spread, short_spread) = calculate_spread(
            300,
            0,
            484,
            47500,
            923807816209694,
            925117623772584,
            13731157,
            -1314027016625,
            13667686,
            115876379475,
            91316628,
            928097825691666,
            907979542352912,
            945977491145601,
            161188,
            1459632439,
            12358265776,
            72230366233,
            432067603632,
            0,
            0,
            0,
            0,
        )
        .unwrap();
        assert_eq!(long_spread, 146096);
        assert_eq!(short_spread, 853904);

        // terms 4
        let (long_spread, short_spread) = calculate_spread(
            300,
            0,
            484,
            47500,
            923807816209694,
            925117623772584,
            13731157,
            -1314027016625,
            13667686,
            115876379475,
            91316628,
            928097825691666,
            907979542352912,
            945977491145601,
            161188,
            1459632439,
            12358265776,
            72230366233,
            432067603632,
            0,
            0,
            0,
            0,
        )
        .unwrap();
        assert_eq!(long_spread, 146096);
        assert_eq!(short_spread, 853904);

        // extra one?

        let (long_spread, short_spread) = calculate_spread(
            300,
            0,
            341,
            47500,
            923813838283625,
            925117620897828,
            13715312,
            -1307974136691,
            13652092,
            115857021791,
            71958944,
            928091775691666,
            907979545174412,
            945977494085178,
            11581,
            54284474,
            9520659647,
            53979922148,
            427588331503,
            0,
            0,
            0,
            0,
        )
        .unwrap();
        assert_eq!(long_spread, 11068);
        assert_eq!(short_spread, 135916);
    }

    #[test]
    fn calculate_spread_funding_bias_scale_tests() {
        use crate::math::constants::PRICE_PRECISION_I64;

        let twap = 100 * PRICE_PRECISION_I64;
        // hourly rate matching the funding offset floor f_ref (~10.95%/yr)
        // on a $100 oracle twap: f_norm = 1_250_000 * 1e6 / 1e8 * 24 = 300_000
        let saturating_rate = 1_250_000_i64;
        let q_amm_long = -BASE_PRECISION_I128; // users net short
        let q_amm_short = BASE_PRECISION_I128; // users net long

        // s = 0 disables
        assert_eq!(
            calculate_spread_funding_bias_scale(q_amm_long, saturating_rate, twap, 0).unwrap(),
            BID_ASK_SPREAD_PRECISION
        );

        // f * q >= 0: vAMM receives (or zero rate/inventory), β = 1
        assert_eq!(
            calculate_spread_funding_bias_scale(q_amm_short, saturating_rate, twap, 50).unwrap(),
            BID_ASK_SPREAD_PRECISION
        );
        assert_eq!(
            calculate_spread_funding_bias_scale(q_amm_long, -saturating_rate, twap, 50).unwrap(),
            BID_ASK_SPREAD_PRECISION
        );
        assert_eq!(
            calculate_spread_funding_bias_scale(0, saturating_rate, twap, 50).unwrap(),
            BID_ASK_SPREAD_PRECISION
        );
        assert_eq!(
            calculate_spread_funding_bias_scale(q_amm_long, 0, twap, 50).unwrap(),
            BID_ASK_SPREAD_PRECISION
        );

        // paying at f_ref: ρ ~= 1, β ~= 1 + s/100 (off by integer rounding
        // of f_ref = 1e9 / 3333)
        assert_eq!(
            calculate_spread_funding_bias_scale(q_amm_long, saturating_rate, twap, 50).unwrap(),
            1_499_950
        );
        assert_eq!(
            calculate_spread_funding_bias_scale(q_amm_short, -saturating_rate, twap, 50).unwrap(),
            1_499_950
        );

        // half ramp: β = 1 + s/100 * 0.5
        assert_eq!(
            calculate_spread_funding_bias_scale(q_amm_long, saturating_rate / 2, twap, 50).unwrap(),
            1_249_975
        );

        // past f_ref the ramp clamps at 1: β = 1 + s/100 exactly
        assert_eq!(
            calculate_spread_funding_bias_scale(q_amm_long, saturating_rate * 10, twap, 100)
                .unwrap(),
            2 * BID_ASK_SPREAD_PRECISION
        );
    }

    #[test]
    fn calculate_spread_funding_bias_tests() {
        use crate::math::constants::PRICE_PRECISION_I64;

        let base_spread = 1000;
        let max_spread = 2000;
        let quote_asset_reserve = AMM_RESERVE_PRECISION * 10;
        let terminal_quote_asset_reserve = AMM_RESERVE_PRECISION * 10;
        let peg_multiplier = 34000000;
        // tiny vAMM-long inventory so σ = λ = 1 and the bias is isolated
        let base_asset_amount_with_amm = -1_000_i128;
        let reserve_price = 34562304;
        let total_fee_minus_distributions = QUOTE_PRECISION_I128 * 10;
        let base_asset_reserve = AMM_RESERVE_PRECISION * 10;
        let min_base_asset_reserve = 0_u128;
        let max_base_asset_reserve = AMM_RESERVE_PRECISION * 100000;
        let twap = 100 * PRICE_PRECISION_I64;
        let saturating_rate = 12_500_000_i64; // well past f_ref

        let calc = |rate: i64, s: u8| {
            calculate_spread(
                base_spread,
                0,
                0,
                max_spread,
                quote_asset_reserve,
                terminal_quote_asset_reserve,
                peg_multiplier,
                base_asset_amount_with_amm,
                reserve_price,
                total_fee_minus_distributions,
                0,
                base_asset_reserve,
                min_base_asset_reserve,
                max_base_asset_reserve,
                0,
                0,
                0,
                0,
                0,
                0,
                rate,
                twap,
                s,
            )
            .unwrap()
        };

        let (long0, short0) = calc(saturating_rate, 0);
        assert_eq!((long0, short0), (500, 500));

        // vAMM long + positive funding: pays, short side doubles at s = 100
        let (long1, short1) = calc(saturating_rate, 100);
        assert_eq!(long1, long0);
        assert_eq!(short1, short0 * 2);

        // negative funding with the same inventory: vAMM receives, no-op
        let (long2, short2) = calc(-saturating_rate, 100);
        assert_eq!((long2, short2), (long0, short0));
    }

    /// Golden tests for the structure-only refactor: pin the exact outputs of
    /// `update_amm_quote_state` (the one public entry whose signature the
    /// refactor keeps) over adversarial inputs. Every assertion below is a
    /// recorded output of the pre-refactor code; the refactor must not move
    /// any of them.
    mod golden {
        use {
            super::*,
            crate::{
                math::oracle::OracleValidity,
                state::{
                    oracle::{MMOraclePriceData, OraclePriceData},
                    perp_market::MarketStats,
                },
                vlp::amm::state::AMM,
            },
        };

        fn base_amm() -> AMM {
            AMM {
                base_spread: 250,
                max_spread: 9750,
                curve_update_intensity: 100,
                base_asset_amount_with_amm: AMM_RESERVE_PRECISION as i128,
                total_fee_minus_distributions: 100 * QUOTE_PRECISION_I128,
                net_revenue_since_last_funding: crate::math::constants::QUOTE_PRECISION_I64,
                last_spread_update_slot: 90,
                ..AMM::default_test()
            }
        }

        fn base_stats() -> MarketStats {
            MarketStats {
                last_oracle_conf_pct: 100,
                mark_std: 500,
                oracle_std: 500,
                long_intensity_volume: 1_000_000,
                short_intensity_volume: 1_000_000,
                volume_24h: 10_000_000,
                ..MarketStats::default()
            }
        }

        /// Refresh `amm` in place against an oracle at `reserve + oracle_delta`
        /// and return (long_spread, short_spread, reference_price_offset,
        /// last_oracle_reserve_price_spread_pct, ask_base, ask_quote, bid_base,
        /// bid_quote).
        fn refresh(
            amm: &mut AMM,
            stats: &MarketStats,
            oracle_delta: i64,
            slot: u64,
        ) -> (u32, u32, i32, i64, u128, u128, u128, u128) {
            let reserve_price = amm.reserve_price().unwrap();
            let oracle_price = reserve_price as i64 + oracle_delta;
            let opd = OraclePriceData {
                price: oracle_price,
                confidence: 100,
                delay: 0,
                has_sufficient_number_of_data_points: true,
                sequence_id: None,
            };
            let mm =
                MMOraclePriceData::new(oracle_price, 0, 0, OracleValidity::Valid, opd).unwrap();
            update_amm_quote_state(amm, stats, &mm, reserve_price, slot).unwrap();
            assert_eq!(amm.last_spread_update_slot, slot);
            (
                amm.long_spread,
                amm.short_spread,
                amm.reference_price_offset,
                amm.last_oracle_reserve_price_spread_pct,
                amm.ask_base_asset_reserve,
                amm.ask_quote_asset_reserve,
                amm.bid_base_asset_reserve,
                amm.bid_quote_asset_reserve,
            )
        }

        #[test]
        fn golden_baseline() {
            let mut amm = base_amm();
            let out = refresh(&mut amm, &base_stats(), 0, 100);
            assert_eq!(
                out,
                (
                    224,
                    125,
                    0,
                    0,
                    99988801254,
                    100011200000,
                    100006250390,
                    99993750000
                )
            );
        }

        #[test]
        fn golden_divergence_wide() {
            // oracle 5% above reserve price: negative spread pct, long retreat.
            let mut amm = base_amm();
            let stats = MarketStats {
                last_oracle_conf_pct: 30_000,
                ..base_stats()
            };
            let reserve_price = amm.reserve_price().unwrap();
            let out = refresh(&mut amm, &stats, (reserve_price / 20) as i64, 100);
            assert_eq!(
                out,
                (
                    55000,
                    5000,
                    0,
                    -50000,
                    97323600973,
                    102750000000,
                    100250626566,
                    99750000000
                )
            );
        }

        #[test]
        fn extreme_divergence_is_clipped_for_quote_math() {
            let amm = base_amm();
            let inputs = SpreadInputs::from_stats(&base_stats());
            let reserve_price = amm.reserve_price().unwrap();

            for (raw, clipped) in [
                (
                    -2 * BID_ASK_SPREAD_PRECISION_I64,
                    -BID_ASK_SPREAD_PRECISION_I64,
                ),
                (
                    2 * BID_ASK_SPREAD_PRECISION_I64,
                    BID_ASK_SPREAD_PRECISION_I64,
                ),
            ] {
                assert_eq!(
                    crate::vlp::amm::math::spread::calculate_spread(
                        &amm,
                        &inputs,
                        reserve_price,
                        raw,
                    )
                    .unwrap(),
                    crate::vlp::amm::math::spread::calculate_spread(
                        &amm,
                        &inputs,
                        reserve_price,
                        clipped,
                    )
                    .unwrap(),
                );
            }
        }

        #[test]
        fn extreme_divergence_refresh_preserves_raw_value() {
            let mut amm = base_amm();
            let reserve_price = amm.reserve_price().unwrap();

            let out = refresh(
                &mut amm,
                &base_stats(),
                reserve_price.safe_mul(2).unwrap().cast().unwrap(),
                100,
            );

            assert_eq!(out.3, -2 * BID_ASK_SPREAD_PRECISION_I64);
            assert_eq!(out.0 as u64 + out.1 as u64, BID_ASK_SPREAD_PRECISION);
        }

        #[test]
        fn golden_tfmd_zero_and_negative() {
            let mut amm = AMM {
                total_fee_minus_distributions: 0,
                ..base_amm()
            };
            let out0 = refresh(&mut amm, &base_stats(), 0, 100);
            assert_eq!(
                out0,
                (
                    2220,
                    1250,
                    0,
                    0,
                    99889123073,
                    100111000000,
                    100062539086,
                    99937500000
                )
            );

            let mut amm = AMM {
                total_fee_minus_distributions: -1000 * QUOTE_PRECISION_I128,
                ..base_amm()
            };
            let outn = refresh(&mut amm, &base_stats(), 0, 100);
            assert_eq!(out0, outn);
        }

        #[test]
        fn golden_inventory_sign_and_retreat() {
            // negative revenue triggers the retreat; tiny inventory picks the
            // side that gets the full amount.
            let stats = base_stats();
            let mut amm_long = AMM {
                base_asset_amount_with_amm: 1,
                net_revenue_since_last_funding: -30 * crate::math::constants::QUOTE_PRECISION_I64,
                ..base_amm()
            };
            let out_long = refresh(&mut amm_long, &stats, 0, 100);
            assert_eq!(
                out_long,
                (
                    425,
                    275,
                    0,
                    0,
                    99978754514,
                    100021250000,
                    100013751890,
                    99986250000
                )
            );

            let mut amm_short = AMM {
                base_asset_amount_with_amm: -1,
                net_revenue_since_last_funding: -30 * crate::math::constants::QUOTE_PRECISION_I64,
                ..base_amm()
            };
            let out_short = refresh(&mut amm_short, &stats, 0, 100);
            assert_eq!(
                out_short,
                (
                    275,
                    425,
                    0,
                    0,
                    99986251890,
                    100013750000,
                    100021254516,
                    99978750000
                )
            );

            let mut amm_zero = AMM {
                base_asset_amount_with_amm: 0,
                net_revenue_since_last_funding: -30 * crate::math::constants::QUOTE_PRECISION_I64,
                ..base_amm()
            };
            let out_zero = refresh(&mut amm_zero, &stats, 0, 100);
            assert_eq!(
                out_zero,
                (
                    275,
                    275,
                    0,
                    0,
                    99986251890,
                    100013750000,
                    100013751890,
                    99986250000
                )
            );
        }

        #[test]
        fn golden_conf_above_lazer_floor() {
            // 25bp confidence: 2500 / 20 + (2500 - 2000) = 625 on each side
            // before the inventory scale widens the long side.
            let mut amm_at = base_amm();
            let stats_at = MarketStats {
                last_oracle_conf_pct: 2500,
                ..base_stats()
            };
            let out_at = refresh(&mut amm_at, &stats_at, 0, 100);
            assert_eq!((out_at.0, out_at.1), (729, 625));

            let mut amm_above = base_amm();
            let stats_above = MarketStats {
                last_oracle_conf_pct: 2501,
                ..base_stats()
            };
            let out_above = refresh(&mut amm_above, &stats_above, 0, 100);
            // One more unit of confidence moves the quote by one unit; there
            // is no threshold and no cliff.
            assert_eq!(out_above.1, out_at.1 + 1);
            assert!(out_above.0 - out_at.0 <= 2);
        }

        #[test]
        fn golden_offset_follows_inventory_without_smoothing() {
            // +1% premium with positive inventory: the offset applies, sized
            // by inventory over the average open liquidity (10 base each
            // side here) and capped at max_offset (intensity 110: 10bp).
            let quote = |base_asset_amount_with_amm: i128, prior_offset: i32| {
                let mut amm = AMM {
                    curve_update_intensity: 110,
                    base_asset_amount_with_amm,
                    min_base_asset_reserve: 90 * AMM_RESERVE_PRECISION,
                    max_base_asset_reserve: 110 * AMM_RESERVE_PRECISION,
                    ..base_amm()
                };
                let reserve_price = amm.reserve_price().unwrap();
                let premium = reserve_price / 100;
                let stats = MarketStats {
                    last_reference_price_offset: prior_offset,
                    last_24h_avg_funding_rate: 100_000,
                    last_funding_oracle_twap: reserve_price as i64,
                    last_mark_price_twap_5min: reserve_price + premium,
                    last_mark_price_twap: reserve_price + premium,
                    historical_oracle_data: crate::state::oracle::HistoricalOracleData {
                        last_oracle_price_twap_5min: reserve_price as i64,
                        last_oracle_price_twap: reserve_price as i64,
                        ..Default::default()
                    },
                    ..base_stats()
                };
                let out = refresh(&mut amm, &stats, 0, 100);
                let bid_price = calculate_price(
                    amm.bid_quote_asset_reserve,
                    amm.bid_base_asset_reserve,
                    amm.peg_multiplier,
                )
                .unwrap();
                (out, bid_price, reserve_price)
            };

            // 1 base of inventory = 10% of liquidity: full 10bp offset
            let (full, bid_price, oracle_price) = quote(AMM_RESERVE_PRECISION as i128, 0);
            assert_eq!(full.2, 1000);
            // the offset lifts the bid, and the guard holds it at the oracle
            assert!(full.1 > 1000);
            assert!(bid_price <= oracle_price);

            // 0.5 base = 5%: half the offset
            let (half, _, _) = quote(AMM_RESERVE_PRECISION as i128 / 2, 0);
            assert_eq!(half.2, 500);

            // the previous offset no longer changes anything, even across a
            // sign flip
            let (after_negative, _, _) = quote(AMM_RESERVE_PRECISION as i128, -1000);
            let (after_positive, _, _) = quote(AMM_RESERVE_PRECISION as i128, 1000);
            assert_eq!(after_negative, full);
            assert_eq!(after_positive, full);
        }

        #[test]
        fn golden_inventory_adjustment_positive() {
            // The positive arm of apply_percent_adjustment (saturating_add +
            // safe_div_ceil + floors) is executed by no other test in the
            // repo.
            let mut amm_50 = AMM {
                amm_inventory_spread_adjustment: 50,
                ..base_amm()
            };
            let out_50 = refresh(&mut amm_50, &base_stats(), 0, 100);
            assert_eq!(
                out_50,
                (
                    336,
                    188,
                    0,
                    0,
                    99983202821,
                    100016800000,
                    100009400883,
                    99990600000
                )
            );

            let mut amm_100 = AMM {
                amm_inventory_spread_adjustment: 100,
                ..base_amm()
            };
            let out_100 = refresh(&mut amm_100, &base_stats(), 0, 100);
            assert_eq!(
                out_100,
                (
                    448,
                    250,
                    0,
                    0,
                    99977605016,
                    100022400000,
                    100012501562,
                    99987500000
                )
            );
        }

        #[test]
        fn golden_asymmetric_intensity_both_divergence_signs() {
            // Asymmetric intensity volumes make long and short vol spreads
            // differ, pinning the from_stats long/short mapping and both
            // arms of the oracle retreat (a vol.0/vol.1 swap in either arm
            // changes these values).
            let stats = MarketStats {
                long_intensity_volume: 1_000_000,
                short_intensity_volume: 3_000_000,
                last_oracle_conf_pct: 3000,
                mark_std: 2000,
                oracle_std: 2000,
                ..base_stats()
            };

            // oracle above reserve: negative pct, long retreat arm
            let mut amm_neg = base_amm();
            let rp = amm_neg.reserve_price().unwrap();
            let out_neg = refresh(&mut amm_neg, &stats, (rp / 50) as i64, 100);
            assert_eq!(
                out_neg,
                (
                    20000,
                    0,
                    0,
                    -20000,
                    99009900990,
                    101000000000,
                    100000000000,
                    100000000000
                )
            );

            // oracle below reserve: positive pct, short retreat arm. A 20000
            // short spread would put the bid at reserve * 0.99^2 = 0.9801,
            // above the 0.98 oracle; the oracle guard widens it to
            // 2 * (1 - sqrt(0.98)) plus its rounding margin.
            let mut amm_pos = base_amm();
            let out_pos = refresh(&mut amm_pos, &stats, -((rp / 50) as i64), 100);
            assert_eq!(
                out_pos,
                (
                    0,
                    20103,
                    0,
                    20000,
                    100000000000,
                    100000000000,
                    101015355849,
                    98994850000
                )
            );
            let bid_price = calculate_price(out_pos.7, out_pos.6, amm_pos.peg_multiplier).unwrap();
            assert!(bid_price as i64 <= rp as i64 - (rp / 50) as i64);
        }

        #[test]
        fn golden_funding_bias_with_adjustment_and_flat_gate() {
            // Funding bias active through the full refresh, stacked with a
            // nonzero inventory adjustment: pins the step 6 -> step 7
            // ordering end to end.
            let mut amm = AMM {
                funding_bias_sensitivity: 50,
                amm_inventory_spread_adjustment: 25,
                base_asset_amount_with_amm: AMM_RESERVE_PRECISION as i128,
                ..base_amm()
            };
            let rp = amm.reserve_price().unwrap();
            // q > 0 with negative normalized funding: vAMM pays.
            let stats = MarketStats {
                last_24h_avg_funding_rate: -1_000_000,
                last_funding_oracle_twap: rp as i64,
                ..base_stats()
            };
            let out = refresh(&mut amm, &stats, 0, 100);
            assert_eq!(
                out,
                (
                    420,
                    157,
                    0,
                    0,
                    99979004409,
                    100021000000,
                    100007850616,
                    99992150000
                )
            );

            // curve_update_intensity == 0: the flat base/2 gate, pinned here
            // instead of depending on a distant fill test.
            let mut amm_flat = AMM {
                curve_update_intensity: 0,
                ..base_amm()
            };
            let out_flat = refresh(&mut amm_flat, &base_stats(), 0, 100);
            assert_eq!(
                out_flat,
                (
                    125,
                    125,
                    0,
                    0,
                    99993750390,
                    100006250000,
                    100006250390,
                    99993750000
                )
            );
        }

        #[test]
        fn golden_spread_adjustment() {
            let mut amm_neg = AMM {
                amm_spread_adjustment: -50,
                ..base_amm()
            };
            let out_neg = refresh(&mut amm_neg, &base_stats(), 0, 100);
            assert_eq!(
                out_neg,
                (
                    112,
                    63,
                    0,
                    0,
                    99994400313,
                    100005600000,
                    100003150099,
                    99996850000
                )
            );

            let mut amm_pos = AMM {
                amm_spread_adjustment: 50,
                ..base_amm()
            };
            let out_pos = refresh(&mut amm_pos, &base_stats(), 0, 100);
            assert_eq!(
                out_pos,
                (
                    336,
                    188,
                    0,
                    0,
                    99983202821,
                    100016800000,
                    100009400883,
                    99990600000
                )
            );

            let mut amm_inv = AMM {
                amm_inventory_spread_adjustment: -50,
                ..base_amm()
            };
            let out_inv = refresh(&mut amm_inv, &base_stats(), 0, 100);
            assert_eq!(
                out_inv,
                (
                    125,
                    125,
                    0,
                    0,
                    99993750390,
                    100006250000,
                    100006250390,
                    99993750000
                )
            );
        }
    }

    /// Tests that pin the intended behavior of each spread mechanism, so a
    /// later change that breaks the intent fails here rather than on chain.
    mod intent {
        use {
            super::*,
            crate::{
                math::{bn::U192, constants::PEG_PRECISION, oracle::OracleValidity},
                state::{
                    oracle::{MMOraclePriceData, OraclePriceData},
                    perp_market::MarketStats,
                },
            },
        };

        /// Deterministic pseudo-random numbers for the property tests.
        struct Lcg(u64);
        impl Lcg {
            fn next(&mut self) -> u64 {
                self.0 = self
                    .0
                    .wrapping_mul(6364136223846793005)
                    .wrapping_add(1442695040888963407);
                self.0 >> 11
            }
            fn range(&mut self, lo: u64, hi: u64) -> u64 {
                lo + self.next() % (hi - lo + 1)
            }
        }

        /// A curve with base/quote reserves and a consistent k.
        fn curve(base: u128, quote: u128, peg: u128) -> AMM {
            let sqrt_k = U192::from(base)
                .safe_mul(U192::from(quote))
                .unwrap()
                .integer_sqrt()
                .try_to_u128()
                .unwrap();
            AMM {
                base_asset_reserve: base,
                quote_asset_reserve: quote,
                sqrt_k,
                peg_multiplier: peg,
                ..AMM::default()
            }
        }

        /// The bid and ask prices the curve actually quotes for the given
        /// spreads and offset, through the same reserve path fills use.
        fn quoted_prices(amm: &AMM, long: u32, short: u32, offset: i32) -> (u64, u64) {
            let (ask_base, ask_quote) =
                compute_spread_reserves_for_direction(amm, long, offset, PositionDirection::Long)
                    .unwrap();
            let (bid_base, bid_quote) =
                compute_spread_reserves_for_direction(amm, short, offset, PositionDirection::Short)
                    .unwrap();
            (
                calculate_price(bid_quote, bid_base, amm.peg_multiplier).unwrap(),
                calculate_price(ask_quote, ask_base, amm.peg_multiplier).unwrap(),
            )
        }

        #[test]
        fn oracle_guard_never_quotes_through_the_oracle() {
            let mut rng = Lcg(7);
            let mut widened = 0;
            for _ in 0..2000 {
                let base = rng.range(1_000_000_000, 1_000_000_000_000_000) as u128;
                let quote = base * rng.range(500, 2000) as u128 / 1000;
                let peg = rng.range(1_000, 100_000_000_000) as u128;
                let amm = curve(base, quote, peg);
                let reserve_price = amm.reserve_price().unwrap();
                if reserve_price < 1_000 {
                    continue;
                }
                // oracle within +-20% of the curve
                let oracle = (reserve_price as u128 * rng.range(800_000, 1_200_000) as u128
                    / 1_000_000) as i64;
                let long = rng.range(0, 30_000) as u32;
                let short = rng.range(0, 30_000) as u32;
                let offset = rng.range(0, 10_000) as i32 - 5_000;

                let (g_long, g_short) =
                    apply_oracle_guard(long, short, offset, reserve_price, oracle).unwrap();

                // only ever widens, and stays a legal pair
                assert!(g_long >= long && g_short >= short);
                assert!(g_long as u64 + g_short as u64 <= BID_ASK_SPREAD_PRECISION);

                // the quotes the curve actually produces sit on the right side
                let (bid, ask) = quoted_prices(&amm, g_long, g_short, offset);
                // and so does the linear reading routing and the mark TWAP use
                let (read_bid, read_ask) = amm
                    .bid_ask_price(reserve_price, g_long, g_short, offset)
                    .unwrap();
                assert!(
                    read_bid as i64 <= oracle,
                    "read bid {read_bid} above oracle {oracle}"
                );
                assert!(
                    read_ask as i64 >= oracle,
                    "read ask {read_ask} below oracle {oracle}"
                );
                assert!(
                    bid as i64 <= oracle,
                    "bid {bid} above oracle {oracle} (reserve {reserve_price}, long {long}, \
                     short {short}, offset {offset})"
                );
                assert!(
                    ask as i64 >= oracle,
                    "ask {ask} below oracle {oracle} (reserve {reserve_price}, long {long}, \
                     short {short}, offset {offset})"
                );

                // and a widened side is widened by no more than a few units of
                // rounding: four units less would cross the oracle again in the
                // executed or the read price
                if g_short > short && g_short - 4 > short {
                    let (bid_less, _) = quoted_prices(&amm, g_long, g_short - 4, offset);
                    let (read_less, _) = amm
                        .bid_ask_price(reserve_price, g_long, g_short - 4, offset)
                        .unwrap();
                    assert!(
                        bid_less as i64 > oracle || read_less as i64 > oracle,
                        "short widened more than needed"
                    );
                }
                if g_long > long && g_long - 4 > long {
                    let (_, ask_less) = quoted_prices(&amm, g_long - 4, g_short, offset);
                    let (_, read_less) = amm
                        .bid_ask_price(reserve_price, g_long - 4, g_short, offset)
                        .unwrap();
                    assert!(
                        (ask_less as i64) < oracle || (read_less as i64) < oracle,
                        "long widened more than needed"
                    );
                }
                if g_long > long || g_short > short {
                    widened += 1;
                }
            }
            // the sample exercises both the binding and the non-binding case
            assert!(widened > 200 && widened < 1900, "widened {widened}");
        }

        #[test]
        fn oracle_guard_leaves_a_safe_quote_alone() {
            // curve at the oracle, no offset: nothing binds, not even zero spreads
            assert_eq!(
                apply_oracle_guard(0, 0, 0, PRICE_PRECISION_U64, PRICE_PRECISION_U64 as i64)
                    .unwrap(),
                (0, 0)
            );
            assert_eq!(
                apply_oracle_guard(250, 400, 0, PRICE_PRECISION_U64, PRICE_PRECISION_U64 as i64)
                    .unwrap(),
                (250, 400)
            );
            // curve 1% above the oracle with a 2% bid spread: already safe
            assert_eq!(
                apply_oracle_guard(100, 20_000, 0, 1_010_000, 1_000_000).unwrap(),
                (100, 20_000)
            );
        }

        #[test]
        fn oracle_guard_widens_the_ask_when_the_curve_is_below_the_oracle() {
            // curve 2% below the oracle: the ask needs 2 * (sqrt(1/0.98) - 1)
            let (long, short) = apply_oracle_guard(100, 100, 0, 980_000, 1_000_000).unwrap();
            assert_eq!(short, 100);
            assert!(long > 20_000, "long {long}");
            let amm = curve(
                100 * AMM_RESERVE_PRECISION,
                98 * AMM_RESERVE_PRECISION,
                PEG_PRECISION,
            );
            let (_, ask) = quoted_prices(&amm, long, short, 0);
            assert!(ask >= 1_000_000, "ask {ask}");
        }

        #[test]
        fn oracle_guard_covers_the_linear_quote_readers() {
            // Curve 2% below the oracle. The executed ask needs 19_903 units,
            // but `bid_ask_price` (routing, mark TWAP) reads the ask linearly and
            // needs 20_000; with only 19_903 it would read 1_019_903, below the
            // 1_020_000 oracle.
            let (long, _) = apply_oracle_guard(100, 100, 0, 1_000_000, 1_020_000).unwrap();
            assert_eq!(long, 20_000);
            let amm = AMM::default();
            let (_, read_ask) = amm.bid_ask_price(1_000_000, long, 100, 0).unwrap();
            assert!(read_ask >= 1_020_000, "read ask {read_ask}");
        }

        #[test]
        fn oracle_guard_holds_the_line_against_the_offset() {
            // an offset lifting the bid by 1bp at the oracle: short moves up to it
            assert_eq!(
                apply_oracle_guard(0, 0, 100, PRICE_PRECISION_U64, PRICE_PRECISION_U64 as i64)
                    .unwrap(),
                (0, 101)
            );
            // and a negative offset lowering the ask
            assert_eq!(
                apply_oracle_guard(0, 0, -100, PRICE_PRECISION_U64, PRICE_PRECISION_U64 as i64)
                    .unwrap(),
                (101, 0)
            );
        }

        #[test]
        fn oracle_guard_keeps_the_pair_within_one_hundred_percent() {
            // an oracle 3x the curve would need more than 100% of ask spread
            let (long, short) = apply_oracle_guard(0, 250, 0, 1_000_000, 3_000_000).unwrap();
            assert_eq!(short, 250);
            assert_eq!(long as u64 + short as u64, BID_ASK_SPREAD_PRECISION);
        }

        /// Whether the guarded quote sits on the correct side of the oracle, read
        /// both linearly and as the marginal price at the spread reserves.
        fn guarded_quote_is_safe(
            long: u32,
            short: u32,
            offset: i32,
            reserve_price: u64,
            oracle: u64,
        ) -> bool {
            let (long, short) =
                apply_oracle_guard(long, short, offset, reserve_price, oracle as i64).unwrap();
            let (bid, ask) = AMM::default()
                .bid_ask_price(reserve_price, long, short, offset)
                .unwrap();
            let p = 2 * BID_ASK_SPREAD_PRECISION_I128;
            let r = reserve_price as i128;
            let bid_factor = p + offset as i128 - short as i128;
            let ask_factor = p + offset as i128 + long as i128;
            let marginal_bid = r * bid_factor * bid_factor / (p * p);
            let marginal_ask = r * ask_factor * ask_factor / (p * p);
            bid <= oracle
                && ask >= oracle
                && marginal_bid <= oracle as i128
                && marginal_ask >= oracle as i128
        }

        #[test]
        fn oracle_guard_saturates_when_the_requirement_exceeds_the_remaining_budget() {
            let r = 1_000_000_u64;
            // zero opposite spread and offset: safe between about 1/4x and 2x
            for oracle in [
                250_100, 300_000, 500_000, 990_000, 1_010_000, 1_500_000, 1_999_000,
            ] {
                assert!(guarded_quote_is_safe(0, 0, 0, r, oracle), "oracle {oracle}");
            }
            assert!(!guarded_quote_is_safe(0, 0, 0, r, 200_000));
            assert!(!guarded_quote_is_safe(0, 0, 0, r, 2_100_000));

            // a wide opposite spread uses up the budget well inside that range
            assert_eq!(
                apply_oracle_guard(100, 800_000, 0, r, 1_500_000).unwrap(),
                (200_000, 800_000)
            );
            assert!(!guarded_quote_is_safe(100, 800_000, 0, r, 1_500_000));
            assert_eq!(
                apply_oracle_guard(800_000, 100, 0, r, 500_000).unwrap(),
                (800_000, 200_000)
            );
            assert!(!guarded_quote_is_safe(800_000, 100, 0, r, 500_000));

            // an offset against the side moves the limit too: a -50% offset
            // leaves the ask unable to reach an oracle at 1.6x
            assert!(guarded_quote_is_safe(0, 0, 0, r, 1_600_000));
            assert!(!guarded_quote_is_safe(0, 0, -500_000, r, 1_600_000));
        }

        fn quote_state_amm(curve_update_intensity: u8) -> AMM {
            AMM {
                base_spread: 500,
                max_spread: 20_000,
                curve_update_intensity,
                amm_spread_adjustment: -25,
                amm_inventory_spread_adjustment: -25,
                total_fee_minus_distributions: 100 * QUOTE_PRECISION_I128,
                ..AMM::default_test()
            }
        }

        fn quote_state_stats() -> MarketStats {
            MarketStats {
                last_oracle_conf_pct: 2000,
                mark_std: 500,
                oracle_std: 500,
                long_intensity_volume: 1_000_000,
                short_intensity_volume: 1_000_000,
                volume_24h: 10_000_000,
                ..MarketStats::default()
            }
        }

        fn refresh_against(amm: &mut AMM, oracle_price: i64) {
            let reserve_price = amm.reserve_price().unwrap();
            let opd = OraclePriceData {
                price: oracle_price,
                confidence: 100,
                delay: 0,
                has_sufficient_number_of_data_points: true,
                sequence_id: None,
            };
            let mm =
                MMOraclePriceData::new(oracle_price, 0, 0, OracleValidity::Valid, opd).unwrap();
            update_amm_quote_state(amm, &quote_state_stats(), &mm, reserve_price, 100).unwrap();
        }

        #[test]
        fn admin_adjustments_cannot_push_the_bid_through_the_oracle() {
            // The mainnet case: the curve 30bp above the oracle and both admin
            // adjustments at -25. The retreat puts the bid below the oracle,
            // the two -25% cuts pull it back through, and the guard holds it.
            let mut amm = quote_state_amm(100);
            let reserve_price = amm.reserve_price().unwrap();
            let oracle = (reserve_price as u128 * 9_970 / 10_000) as i64;
            refresh_against(&mut amm, oracle);

            let bid = calculate_price(
                amm.bid_quote_asset_reserve,
                amm.bid_base_asset_reserve,
                amm.peg_multiplier,
            )
            .unwrap();
            assert!(bid as i64 <= oracle, "bid {bid} above oracle {oracle}");
        }

        #[test]
        fn frozen_curve_markets_quote_off_the_curve() {
            // curve_update_intensity 0 never repegs and runs no dynamic
            // pipeline, so neither the retreat nor the guard applies: the
            // spread stays at half the base spread after the admin cut.
            let mut amm = quote_state_amm(0);
            let reserve_price = amm.reserve_price().unwrap();
            let oracle = (reserve_price as u128 * 9_970 / 10_000) as i64;
            refresh_against(&mut amm, oracle);
            assert_eq!((amm.long_spread, amm.short_spread), (188, 188));
        }

        #[test]
        fn lazer_floor_does_not_leak_through_the_vol_base() {
            // Confidence at the 20bp floor with all flow on the long side
            // (intensity share 1) and no std. If the vol base used the raw
            // confidence the long side would get the full 20bp; it gets the
            // discounted 1bp.
            let (long, short) =
                calculate_long_short_vol_spread(2000, PRICE_PRECISION_U64, 0, 0, 100, 0, 100)
                    .unwrap();
            assert_eq!((long, short), (100, 100));

            // real price movement still reaches the vol spread through std/4
            let (long, _) = calculate_long_short_vol_spread(
                2000,
                PRICE_PRECISION_U64,
                40_000,
                40_000,
                100,
                0,
                100,
            )
            .unwrap();
            assert_eq!(long, 10_000);
        }

        #[test]
        fn offset_is_sized_by_inventory_monotone_capped_and_symmetric() {
            let rev_price = 4216 * 10000;
            let max_offset: i64 = 2000;
            let with_premium = |premium_sign: i64, liquidity_fraction: i128| {
                let mark = (4216 * 10000 + premium_sign * 10000) as u64;
                calculate_reference_price_offset(
                    rev_price,
                    premium_sign * 1_000_000,
                    liquidity_fraction,
                    4216 * 10000,
                    mark,
                    4216 * 10000,
                    mark,
                    max_offset,
                )
                .unwrap() as i64
            };

            let mut previous = 0;
            for step in 0..=300 {
                let fraction = step as i128 * 1_000; // 0% .. 30% of liquidity
                let up = with_premium(1, fraction);
                let down = with_premium(-1, -fraction);
                // monotone in inventory and never past the cap
                assert!(up >= previous && up <= max_offset);
                // the same size either way
                assert_eq!(up, -down);
                // premium and inventory disagreeing gives nothing
                assert_eq!(with_premium(-1, fraction), 0);
                assert_eq!(with_premium(1, -fraction), 0);
                previous = up;
            }
            // linear up to 10% of liquidity, flat after
            assert_eq!(with_premium(1, 50_000), max_offset / 2);
            assert_eq!(with_premium(1, 100_000), max_offset);
            assert_eq!(with_premium(1, 300_000), max_offset);
        }
    }

    /// Shared parity fixtures: the SDK asserts against the same files
    /// (packages/sdk/tests/sdkParity/fixtures), so the two implementations
    /// cannot drift apart without a failing test on at least one side.
    mod parity_fixtures {
        use super::*;

        fn rows(csv: &str) -> impl Iterator<Item = Vec<&str>> {
            csv.lines()
                .skip(1)
                .filter(|l| !l.trim().is_empty())
                .map(|l| l.split(',').collect())
        }

        #[test]
        fn calculate_spread_matches_fixtures() {
            let csv = include_str!(concat!(
                "../../../../../../../packages/sdk/tests/sdkParity/fixtures/",
                "calculate_spread.csv"
            ));
            let mut n = 0;
            for c in rows(csv) {
                let out = calculate_spread(
                    c[0].parse().unwrap(),
                    c[1].parse().unwrap(),
                    c[2].parse().unwrap(),
                    c[3].parse().unwrap(),
                    c[4].parse().unwrap(),
                    c[5].parse().unwrap(),
                    c[6].parse().unwrap(),
                    c[7].parse().unwrap(),
                    c[8].parse().unwrap(),
                    c[9].parse().unwrap(),
                    c[10].parse().unwrap(),
                    c[11].parse().unwrap(),
                    c[12].parse().unwrap(),
                    c[13].parse().unwrap(),
                    c[14].parse().unwrap(),
                    c[15].parse().unwrap(),
                    c[16].parse().unwrap(),
                    c[17].parse().unwrap(),
                    c[18].parse().unwrap(),
                    c[19].parse().unwrap(),
                    c[20].parse().unwrap(),
                    c[21].parse().unwrap(),
                    c[22].parse().unwrap(),
                )
                .unwrap();
                assert_eq!(
                    out,
                    (c[23].parse().unwrap(), c[24].parse().unwrap()),
                    "row {}",
                    n + 1
                );
                n += 1;
            }
            assert!(n > 0);
        }

        #[test]
        fn apply_oracle_guard_matches_fixtures() {
            let csv = include_str!(concat!(
                "../../../../../../../packages/sdk/tests/sdkParity/fixtures/",
                "apply_oracle_guard.csv"
            ));
            let mut n = 0;
            for c in rows(csv) {
                let out = apply_oracle_guard(
                    c[0].parse().unwrap(),
                    c[1].parse().unwrap(),
                    c[2].parse().unwrap(),
                    c[3].parse().unwrap(),
                    c[4].parse().unwrap(),
                )
                .unwrap();
                assert_eq!(
                    out,
                    (c[5].parse().unwrap(), c[6].parse().unwrap()),
                    "row {}",
                    n + 1
                );
                n += 1;
            }
            assert!(n > 0);
        }

        #[test]
        fn reference_price_offset_matches_fixtures() {
            let csv = include_str!(concat!(
                "../../../../../../../packages/sdk/tests/sdkParity/fixtures/",
                "reference_price_offset.csv"
            ));
            let mut n = 0;
            for c in rows(csv) {
                let out = calculate_reference_price_offset(
                    c[0].parse().unwrap(),
                    c[1].parse().unwrap(),
                    c[2].parse().unwrap(),
                    c[3].parse().unwrap(),
                    c[4].parse().unwrap(),
                    c[5].parse().unwrap(),
                    c[6].parse().unwrap(),
                    c[7].parse().unwrap(),
                )
                .unwrap();
                assert_eq!(out, c[8].parse::<i32>().unwrap(), "row {}", n + 1);
                n += 1;
            }
            assert!(n > 0);
        }
    }
}
