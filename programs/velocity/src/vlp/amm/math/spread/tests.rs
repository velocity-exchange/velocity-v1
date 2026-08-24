#[cfg(test)]
mod test {
    use crate::{
        error::VelocityResult,
        math::constants::{
            AMM_RESERVE_PRECISION, BASE_PRECISION_I128, BID_ASK_SPREAD_PRECISION,
            BID_ASK_SPREAD_PRECISION_I64, QUOTE_PRECISION, QUOTE_PRECISION_I128,
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
    fn ordered_cap_preserves_priority() {
        // raw = divergence (20, 0) + steering (80, 0) + padding (60, 60)
        let components = SpreadComponents::from_raw(
            SpreadPair {
                long: 160,
                short: 60,
            },
            SpreadPair { long: 20, short: 0 },
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

        // Padding is gone before steering is touched.
        assert_eq!(
            components.cap_total_ordered(90).unwrap(),
            SpreadPair { long: 90, short: 0 }
        );

        // Divergence is the last layer compressed when the ceiling cannot
        // hold even the known oracle gap.
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
            SpreadPair { long: 0, short: 50 },
        )
        .unwrap();

        let capped = components.cap_total_ordered(130).unwrap();
        assert_eq!(capped.total().unwrap(), 130);
        assert_eq!(capped.long, 65);
        assert_eq!(capped.short, 65);
        assert!(capped.long >= 60); // full divergence requirement survives
        assert!(capped.short >= 50); // full steering requirement survives
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

        let res =
            calculate_reference_price_offset(rev_price, 0, 0, 0, 0, 0, 0, 0, max_offset).unwrap();
        assert_eq!(res, 0);

        let res = calculate_reference_price_offset(
            rev_price,
            1,
            10,
            1,
            4216 * 10000,
            4217 * 10000,
            4216 * 10000,
            4217 * 10000,
            max_offset,
        )
        .unwrap();
        assert_eq!(res, 290); // 1 penny divergence
        let res = calculate_reference_price_offset(
            rev_price,
            1,
            10,
            1,
            4216 * 10000,
            4219 * 10000,
            4216 * 10000,
            4219 * 10000,
            max_offset,
        )
        .unwrap();
        assert_eq!(res, 1870);

        let res = calculate_reference_price_offset(
            rev_price,
            -43_000_000,
            10,
            1,
            4216 * 10000,
            4218 * 10000,
            4216 * 10000,
            4218 * 10000,
            max_offset,
        )
        .unwrap();
        assert_eq!(res, 0); // disregard 24h_avg sign

        let res = calculate_reference_price_offset(
            rev_price,
            -43_000_000,
            -10000,
            1,
            4216 * 10000,
            4218 * 10000,
            4216 * 10000,
            4218 * 10000,
            max_offset,
        )
        .unwrap();
        assert_eq!(res, -2500); // counteracting 24h_avg / base inventory sign

        let res = calculate_reference_price_offset(
            rev_price,
            -43_000_000,
            -10,
            1,
            4216 * 10000,
            4214 * 10000,
            4216 * 10000,
            4214 * 10000,
            max_offset,
        )
        .unwrap();
        assert_eq!(res, -2500); // flipped

        let res = calculate_reference_price_offset(
            rev_price,
            1,
            10,
            1,
            4216 * 10000,
            4223 * 10000,
            4216 * 10000,
            4223 * 10000,
            max_offset,
        )
        .unwrap();
        assert_eq!(res, 2500); // 7 penny divergence

        let res = calculate_reference_price_offset(
            rev_price,
            10_000_000,
            10,
            1,
            4216 * 10000,
            4233 * 10000,
            4216 * 10000,
            4233 * 10000,
            max_offset,
        )
        .unwrap();
        assert_eq!(res, 2500); // upper bound

        let res = calculate_reference_price_offset(
            rev_price,
            -10_000_000,
            -10,
            1,
            4216 * 10000,
            4123 * 10000,
            4216 * 10000,
            4123 * 10000,
            max_offset,
        )
        .unwrap();
        assert_eq!(res, -2500); // lower bound

        // max offset = 0
        let res = calculate_reference_price_offset(
            rev_price,
            -10_000_000,
            -10,
            1,
            4216 * 10000,
            4123 * 10000,
            6 * 10000,
            4123 * 10000,
            0,
        )
        .unwrap();
        assert_eq!(res, 0); // zero bound

        // counteracting fast/slow twaps to 0
        let res = calculate_reference_price_offset(
            rev_price,
            -1,
            1,
            1,
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

        // 1000/2 * (1+(34562000-34000000)/QUOTE_PRECISION) -> 781
        // assert_eq!(long_spread3, 31246);
        assert_eq!(long_spread3, 46869);

        // last_oracle_reserve_price_spread_pct + conf retreat
        // assert_eq!(short_spread3, 1010000);
        assert_eq!(short_spread3, 60000);
        assert!(short_spread3 > long_spread3);
        assert_eq!(short_spread3 + long_spread3, 106869);

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
        assert_eq!(bar_l, 1972972973);
        assert_eq!(qar_l, 2027397260);
        assert_eq!(qar_s, 1999500000);

        assert!(qar_l > amm.quote_asset_reserve);
        assert!(bar_l < amm.base_asset_reserve);
        assert!(qar_s < amm.quote_asset_reserve);
        assert!(bar_s > amm.base_asset_reserve);

        let l_price = calculate_price(qar_l, bar_l, amm.peg_multiplier).unwrap();
        let s_price = calculate_price(qar_s, bar_s, amm.peg_multiplier).unwrap();
        assert_eq!(l_price, 1027584);
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
        assert_eq!(bar_l, 1971830986); // down
        assert_eq!(qar_l, 2028571428); // up

        let l_price = calculate_price(qar_l, bar_l, amm.peg_multiplier).unwrap();
        let s_price = calculate_price(qar_s, bar_s, amm.peg_multiplier).unwrap();
        assert_eq!(l_price, 1028775);
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
        assert_eq!(bar_s, 2001501501); // up
        assert_eq!(bar_l, 1974025974); // up
        assert_eq!(qar_l, 2026315789); // down
        assert_eq!(qar_s, 1998499625); // down

        let l_price = calculate_price(qar_l, bar_l, amm.peg_multiplier).unwrap();
        let s_price = calculate_price(qar_s, bar_s, amm.peg_multiplier).unwrap();
        assert_eq!(l_price, 1026488);
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

        // Steering alone consumes the ceiling, so the base/vol padding on
        // the healing side yields completely.
        assert_eq!(long_spread_btc1, 0);
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
        assert_eq!(long_spread1, 0);
        assert_eq!(short_spread1, 200000);

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
        assert_eq!(long_spread1, 0);
        assert_eq!(short_spread1, 200000);

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
        assert_eq!(long_spread1, 200000);
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
        assert_eq!(long_spread1, 200000);
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
        assert_eq!(long_spread1, 200000);
        assert_eq!(short_spread1, 0);
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
        assert_eq!(long_spread, 200000);
        assert_eq!(short_spread, 0);

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
        // The 5bp divergence on short is protected before long-side
        // steering; padding receives no room at this saturation point.
        assert_eq!(long_spread, 195000);
        assert_eq!(short_spread, 5000);

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
        assert_eq!(long_spread, 0);
        assert_eq!(short_spread, 1000000);

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
        assert_eq!(long_spread, 0);
        assert_eq!(short_spread, 1000000);

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
        assert_eq!(long_spread, 0);
        assert_eq!(short_spread, 1000000);

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
            update_amm_quote_state(
                amm,
                stats,
                &mm,
                reserve_price,
                slot,
                crate::math::time::SlotDuration::BASELINE,
            )
            .unwrap();
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
                    99988800538,
                    100011200716,
                    100006200396,
                    99993799988
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
                    59440,
                    560,
                    0,
                    -50000,
                    97058823529,
                    103030303030,
                    100028011204,
                    99971996640
                )
            );
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
                    99889012208,
                    100111111111,
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
                    99978800085,
                    100021204410,
                    100013702383,
                    99986299494
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
                    99986301370,
                    100013700506,
                    100021208907,
                    99978795590
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
                    99986301370,
                    100013700506,
                    100013702383,
                    99986299494
                )
            );
        }

        #[test]
        fn golden_conf_threshold() {
            // 25 bp threshold: PERCENTAGE_PRECISION_U64 / 400 == 2500.
            let mut amm_at = base_amm();
            let stats_at = MarketStats {
                last_oracle_conf_pct: 2500,
                ..base_stats()
            };
            let out_at = refresh(&mut amm_at, &stats_at, 0, 100);
            assert_eq!(
                out_at,
                (
                    350,
                    250,
                    0,
                    0,
                    99982502187,
                    100017500875,
                    100012501562,
                    99987500000
                )
            );

            let mut amm_above = base_amm();
            let stats_above = MarketStats {
                last_oracle_conf_pct: 2501,
                ..base_stats()
            };
            let out_above = refresh(&mut amm_above, &stats_above, 0, 100);
            // One unit of confidence input above the 25 bp threshold moves the
            // quoted spread ~8x. This pins the cliff itself (design issue 5).
            assert_eq!(
                out_above,
                (
                    2778,
                    2501,
                    0,
                    0,
                    99861111111,
                    100139082058,
                    100125156445,
                    99875000000
                )
            );
        }

        #[test]
        fn golden_offset_sign_transition_smoothing() {
            // prior offset negative, fresh offset positive, intensity > 100:
            // the smoothing branch runs and widens both sides asymmetrically.
            let mut amm = AMM {
                curve_update_intensity: 110,
                ..base_amm()
            };
            let reserve_price = amm.reserve_price().unwrap();
            let premium = (reserve_price / 100) as u64;
            let stats = MarketStats {
                last_reference_price_offset: -500,
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
            assert_eq!(
                out,
                (
                    674,
                    175,
                    -450,
                    0,
                    99988800538,
                    100011200716,
                    100031210986,
                    99968798752
                )
            );
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
                    99983201747,
                    100016801075,
                    100009401146,
                    99990599737
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
                    99977603584,
                    100022401433,
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

            // oracle below reserve: positive pct, short retreat arm
            let mut amm_pos = base_amm();
            let out_pos = refresh(&mut amm_pos, &stats, -((rp / 50) as i64), 100);
            assert_eq!(
                out_pos,
                (
                    0,
                    20000,
                    0,
                    20000,
                    100000000000,
                    100000000000,
                    101010101010,
                    99000000000
                )
            );
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
                    99979000420,
                    100021003990,
                    100007800920,
                    99992199688
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
                    99993800372,
                    100006200012,
                    100006200396,
                    99993799988
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
                    99994400269,
                    100005600044,
                    100003100102,
                    99996899994
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
                    99983201747,
                    100016801075,
                    100009401146,
                    99990599737
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
                    99993800372,
                    100006200012,
                    100006200396,
                    99993799988
                )
            );
        }
    }
}
