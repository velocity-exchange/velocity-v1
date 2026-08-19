#[cfg(test)]
mod test {
    use crate::{
        math::{
            constants::PRICE_PRECISION_I64, margin::MarginRequirementType,
            spot_swap::select_margin_type_for_swap,
        },
        state::{oracle::StrictOraclePrice, spot_market::SpotMarket},
    };

    #[test]
    pub fn sell_usdc_buy_sol_decrease_health() {
        let usdc_spot_market = SpotMarket::default_quote_market();

        let sol_spot_market = SpotMarket::default_base_market();

        let usdc_price = PRICE_PRECISION_I64;
        let sol_price = 100 * PRICE_PRECISION_I64;

        let usdc_before = 100 * 10_i128.pow(usdc_spot_market.decimals);
        let sol_before = 0_i128;

        let usdc_after = -100 * 10_i128.pow(usdc_spot_market.decimals);
        let sol_after = 2 * 10_i128.pow(sol_spot_market.decimals);

        let strict_usdc_price = StrictOraclePrice::test(usdc_price);

        let strict_sol_price = StrictOraclePrice::test(sol_price);

        let (margin_type, _) = select_margin_type_for_swap(
            &usdc_spot_market,
            &sol_spot_market,
            &strict_usdc_price,
            &strict_sol_price,
            usdc_before,
            sol_before,
            usdc_after,
            sol_after,
            MarginRequirementType::Initial,
        )
        .unwrap();

        assert_eq!(margin_type, MarginRequirementType::Initial);
    }

    #[test]
    pub fn sell_usdc_buy_sol_increase_health() {
        let usdc_spot_market = SpotMarket::default_quote_market();

        let sol_spot_market = SpotMarket::default_base_market();

        let usdc_price = PRICE_PRECISION_I64;
        let sol_price = 100 * PRICE_PRECISION_I64;

        // close sol borrow by selling usdc
        let usdc_before = 200 * 10_i128.pow(usdc_spot_market.decimals);
        let sol_before = -(10_i128.pow(sol_spot_market.decimals));

        let usdc_after = 100 * 10_i128.pow(usdc_spot_market.decimals);
        let sol_after = 0_i128;

        let strict_usdc_price = StrictOraclePrice::test(usdc_price);

        let strict_sol_price = StrictOraclePrice::test(sol_price);

        let (margin_type, _) = select_margin_type_for_swap(
            &usdc_spot_market,
            &sol_spot_market,
            &strict_usdc_price,
            &strict_sol_price,
            usdc_before,
            sol_before,
            usdc_after,
            sol_after,
            MarginRequirementType::Initial,
        )
        .unwrap();

        assert_eq!(margin_type, MarginRequirementType::Maintenance);
    }

    #[test]
    pub fn buy_usdc_sell_sol_decrease_health() {
        let usdc_spot_market = SpotMarket::default_quote_market();

        let sol_spot_market = SpotMarket::default_base_market();

        let usdc_price = PRICE_PRECISION_I64;
        let sol_price = 100 * PRICE_PRECISION_I64;

        let usdc_before = 0_i128;
        let sol_before = 10_i128.pow(sol_spot_market.decimals);

        let usdc_after = 200 * 10_i128.pow(usdc_spot_market.decimals);
        let sol_after = -(10_i128.pow(sol_spot_market.decimals));

        let strict_usdc_price = StrictOraclePrice::test(usdc_price);

        let strict_sol_price = StrictOraclePrice::test(sol_price);

        let (margin_type, _) = select_margin_type_for_swap(
            &usdc_spot_market,
            &sol_spot_market,
            &strict_usdc_price,
            &strict_sol_price,
            usdc_before,
            sol_before,
            usdc_after,
            sol_after,
            MarginRequirementType::Initial,
        )
        .unwrap();

        assert_eq!(margin_type, MarginRequirementType::Initial);
    }

    #[test]
    pub fn buy_usdc_sell_sol_increase_health() {
        let usdc_spot_market = SpotMarket::default_quote_market();

        let sol_spot_market = SpotMarket::default_base_market();

        let usdc_price = PRICE_PRECISION_I64;
        let sol_price = 100 * PRICE_PRECISION_I64;

        let usdc_before = -100 * 10_i128.pow(usdc_spot_market.decimals);
        let sol_before = 2 * 10_i128.pow(sol_spot_market.decimals);

        let usdc_after = 0_i128;
        let sol_after = 10_i128.pow(sol_spot_market.decimals);

        let strict_usdc_price = StrictOraclePrice::test(usdc_price);

        let strict_sol_price = StrictOraclePrice::test(sol_price);

        let (margin_type, _) = select_margin_type_for_swap(
            &usdc_spot_market,
            &sol_spot_market,
            &strict_usdc_price,
            &strict_sol_price,
            usdc_before,
            sol_before,
            usdc_after,
            sol_after,
            MarginRequirementType::Initial,
        )
        .unwrap();

        assert_eq!(margin_type, MarginRequirementType::Maintenance);
    }
}

#[cfg(test)]
mod validate_price_bands_for_swap {
    use {
        crate::{
            controller::spot_balance::{
                update_spot_market_cumulative_interest, update_spot_market_twap_stats,
            },
            error::ErrorCode,
            math::spot_swap::validate_price_bands_for_swap,
            state::{
                oracle::{HistoricalOracleData, OraclePriceData},
                spot_market::SpotMarket,
            },
            LAMPORTS_PER_SOL_U64, PERCENTAGE_PRECISION_U64, PRICE_PRECISION_I64,
            QUOTE_PRECISION_U64,
        },
        solana_program::native_token::LAMPORTS_PER_SOL,
    };

    #[test]
    fn sol_in_usdc_out() {
        let in_price = 100 * PRICE_PRECISION_I64;
        let in_market = SpotMarket {
            historical_oracle_data: HistoricalOracleData::default_price(in_price),
            ..SpotMarket::default_base_market()
        };

        let out_price = PRICE_PRECISION_I64;
        let out_market = SpotMarket {
            historical_oracle_data: HistoricalOracleData::default_price(out_price),
            ..SpotMarket::default_quote_market()
        };

        let amount_in = LAMPORTS_PER_SOL_U64;
        let amount_out = 100 * QUOTE_PRECISION_U64;

        let max_5min_twap_divergence = PERCENTAGE_PRECISION_U64 / 2;

        let result = validate_price_bands_for_swap(
            &in_market,
            &out_market,
            amount_in,
            amount_out,
            in_price,
            out_price,
            max_5min_twap_divergence,
        );

        assert_eq!(result, Ok(()));

        // breaches oracle price band
        let amount_out = 79 * QUOTE_PRECISION_U64;

        let result = validate_price_bands_for_swap(
            &in_market,
            &out_market,
            amount_in,
            amount_out,
            in_price,
            out_price,
            max_5min_twap_divergence,
        );

        assert_eq!(result, Err(ErrorCode::PriceBandsBreached));

        // breaches twap price band
        let amount_out = 49 * QUOTE_PRECISION_U64;
        let in_price = 49 * PRICE_PRECISION_I64;

        let result = validate_price_bands_for_swap(
            &in_market,
            &out_market,
            amount_in,
            amount_out,
            in_price,
            out_price,
            max_5min_twap_divergence,
        );

        assert_eq!(result, Err(ErrorCode::PriceBandsBreached));
    }

    #[test]
    fn usdc_in_sol_out() {
        let in_price = PRICE_PRECISION_I64;
        let in_market = SpotMarket {
            market_index: 0,
            historical_oracle_data: HistoricalOracleData::default_price(in_price),
            ..SpotMarket::default_quote_market()
        };

        let out_price = 100 * PRICE_PRECISION_I64;
        let out_market = SpotMarket {
            market_index: 1,
            historical_oracle_data: HistoricalOracleData::default_price(out_price),
            ..SpotMarket::default_base_market()
        };

        let amount_in = 100 * QUOTE_PRECISION_U64;
        let amount_out = LAMPORTS_PER_SOL_U64;

        let max_5min_twap_divergence = PERCENTAGE_PRECISION_U64 / 2;

        let result = validate_price_bands_for_swap(
            &in_market,
            &out_market,
            amount_in,
            amount_out,
            in_price,
            out_price,
            max_5min_twap_divergence,
        );

        assert_eq!(result, Ok(()));

        // breaches oracle price band
        let amount_out = 79 * LAMPORTS_PER_SOL / 100;

        let result = validate_price_bands_for_swap(
            &in_market,
            &out_market,
            amount_in,
            amount_out,
            in_price,
            out_price,
            max_5min_twap_divergence,
        );

        assert_eq!(result, Err(ErrorCode::PriceBandsBreached));

        // breaches twap price band
        let amount_out = 49 * LAMPORTS_PER_SOL / 100;
        let out_price = 200 * PRICE_PRECISION_I64;

        let result = validate_price_bands_for_swap(
            &in_market,
            &out_market,
            amount_in,
            amount_out,
            in_price,
            out_price,
            max_5min_twap_divergence,
        );

        assert_eq!(result, Err(ErrorCode::PriceBandsBreached));
    }

    #[test]
    fn sol_in_btc_out() {
        let in_price = 100 * PRICE_PRECISION_I64; // $100 SOL
        let in_market = SpotMarket {
            market_index: 1,
            historical_oracle_data: HistoricalOracleData::default_price(in_price),
            ..SpotMarket::default_base_market()
        };

        let out_price = 20000 * PRICE_PRECISION_I64; // $20k BTC
        let out_market = SpotMarket {
            market_index: 3,
            historical_oracle_data: HistoricalOracleData::default_price(out_price),
            decimals: 6,
            ..SpotMarket::default_base_market()
        };

        let amount_in = LAMPORTS_PER_SOL_U64; // 1 SOL
        let amount_out = QUOTE_PRECISION_U64 / 200; // .005 BTC

        let max_5min_twap_divergence = PERCENTAGE_PRECISION_U64 / 2; // 50%

        let result = validate_price_bands_for_swap(
            &in_market,
            &out_market,
            amount_in,
            amount_out,
            in_price,
            out_price,
            max_5min_twap_divergence,
        );

        assert_eq!(result, Ok(()));

        // breaches oracle price band
        let amount_out = 79 * QUOTE_PRECISION_U64 / 20000;

        let result = validate_price_bands_for_swap(
            &in_market,
            &out_market,
            amount_in,
            amount_out,
            in_price,
            out_price,
            max_5min_twap_divergence,
        );

        assert_eq!(result, Err(ErrorCode::PriceBandsBreached));

        // breaches twap price band
        let amount_out = 49 * QUOTE_PRECISION_U64 / 20000;
        let in_price = 49 * PRICE_PRECISION_I64;

        let result = validate_price_bands_for_swap(
            &in_market,
            &out_market,
            amount_in,
            amount_out,
            in_price,
            out_price,
            max_5min_twap_divergence,
        );

        assert_eq!(result, Err(ErrorCode::PriceBandsBreached));
    }

    /// The swap lane's ordering: `begin_swap` accrues interest without touching
    /// the oracle TWAPs, `end_swap` runs the band check against the pre-swap
    /// anchor, and only then advances the TWAPs (OtterSec #110).
    ///
    /// Asserts both halves. The check rejects the fill against the stored anchor,
    /// and the same check against the post-refresh anchor would have passed — so
    /// the ordering is what carries the fix, not the values in the fixture.
    #[test]
    fn swap_band_check_reads_the_pre_swap_twap_and_the_refresh_follows_it() {
        let twap_ts = 1_700_000_000_i64;
        let anchor = 100 * PRICE_PRECISION_I64;

        let mut in_market = SpotMarket {
            market_index: 1,
            historical_oracle_data: HistoricalOracleData {
                last_oracle_price_twap_ts: twap_ts,
                ..HistoricalOracleData::default_price(anchor)
            },
            ..SpotMarket::default_base_market()
        };

        let out_price = PRICE_PRECISION_I64;
        let out_market = SpotMarket {
            market_index: 0,
            historical_oracle_data: HistoricalOracleData::default_price(out_price),
            ..SpotMarket::default_quote_market()
        };

        // 1 SOL out for $49 against a $100 anchor: 51% divergence on a 50% limit.
        let in_price = 49 * PRICE_PRECISION_I64;
        let amount_in = LAMPORTS_PER_SOL_U64;
        let amount_out = 49 * QUOTE_PRECISION_U64;
        let max_5min_twap_divergence = PERCENTAGE_PRECISION_U64 / 2;

        let oracle_price_data = OraclePriceData {
            price: in_price,
            confidence: 0,
            delay: 0,
            has_sufficient_number_of_data_points: true,
            ..OraclePriceData::default()
        };

        // One full 5-minute window of staleness, so a refresh would move the
        // anchor the whole way to the sanitized sample.
        let now = twap_ts + 300;

        // begin_swap: interest accrues, oracle TWAPs untouched.
        update_spot_market_cumulative_interest(&mut in_market, None, now, false).unwrap();
        assert_eq!(
            in_market.historical_oracle_data.last_oracle_price_twap_5min,
            anchor
        );
        assert_eq!(
            in_market.historical_oracle_data.last_oracle_price_twap_ts, twap_ts,
            "begin_swap must leave the timestamp alone, or the refresh below \
             under-weights the elapsed interval"
        );

        // end_swap: the band check, against the pre-swap anchor.
        let result = validate_price_bands_for_swap(
            &in_market,
            &out_market,
            amount_in,
            amount_out,
            in_price,
            out_price,
            max_5min_twap_divergence,
        );
        assert_eq!(result, Err(ErrorCode::PriceBandsBreached));

        // end_swap, after the check: the TWAPs advance.
        update_spot_market_twap_stats(&mut in_market, Some(&oracle_price_data), now).unwrap();
        assert!(
            in_market.historical_oracle_data.last_oracle_price_twap_5min < anchor,
            "the swap must still contribute to the EMA"
        );
        assert_eq!(
            in_market.historical_oracle_data.last_oracle_price_twap_ts,
            now
        );

        // The same check against the moved anchor passes, which is what running
        // the refresh first would have produced.
        let result = validate_price_bands_for_swap(
            &in_market,
            &out_market,
            amount_in,
            amount_out,
            in_price,
            out_price,
            max_5min_twap_divergence,
        );
        assert_eq!(
            result,
            Ok(()),
            "if this trips, the fixture no longer distinguishes the two orderings"
        );
    }
}
