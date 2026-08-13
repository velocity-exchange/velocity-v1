#[cfg(test)]
mod test {
    use crate::{
        math::{
            constants::IF_FACTOR_PRECISION,
            spot_balance::{
                get_interest_token_amount_with_dust, get_spot_balance, get_token_amount,
                split_deposit_interest,
            },
        },
        state::spot_market::{SpotBalanceType, SpotMarket},
        SPOT_CUMULATIVE_INTEREST_PRECISION,
    };

    fn carveout_market(if_fee_factor: u32, protocol_fee_factor: u32) -> SpotMarket {
        let mut market = SpotMarket {
            decimals: 6,
            ..SpotMarket::default_quote_market()
        };
        market.insurance_fund.if_fee_factor = if_fee_factor;
        market.protocol_fee_factor = protocol_fee_factor;
        market
    }

    /// Run `intervals` splits of the same `deposit_interest`, feeding each result's remainders
    /// back into the market. Return the totals and the remainders that stay in flight.
    fn run_splits(
        market: &mut SpotMarket,
        deposit_interest: u128,
        intervals: u128,
    ) -> (u128, u128, u128) {
        let (mut lenders, mut insurance_fund, mut protocol) = (0_u128, 0_u128, 0_u128);

        for _ in 0..intervals {
            let split = split_deposit_interest(market, deposit_interest).unwrap();

            // Every interval divides the whole gain and nothing else. This is the property that
            // keeps the accrual able to commit: the two cuts never exceed the gain, so the
            // lender share never goes below zero.
            assert_eq!(
                split
                    .for_lenders
                    .checked_add(split.for_insurance_fund)
                    .unwrap()
                    .checked_add(split.for_protocol)
                    .unwrap(),
                deposit_interest
            );

            lenders += split.for_lenders;
            insurance_fund += split.for_insurance_fund;
            protocol += split.for_protocol;

            market.revenue_pool.pending_interest_split_dust = split.carveout_dust;
            market.protocol_fee_pool.pending_interest_split_dust = split.insurance_fund_dust;
        }

        (lenders, insurance_fund, protocol)
    }

    #[test]
    fn bonk() {
        let spot_market = SpotMarket {
            cumulative_deposit_interest: SPOT_CUMULATIVE_INTEREST_PRECISION,
            decimals: 5,
            ..SpotMarket::default_quote_market()
        };

        let one_bonk = 10_u128.pow(spot_market.decimals);

        let balance =
            get_spot_balance(one_bonk, &spot_market, &SpotBalanceType::Deposit, false).unwrap();

        let token_amount =
            get_token_amount(balance, &spot_market, &SpotBalanceType::Deposit).unwrap();
        assert_eq!(token_amount, one_bonk);
    }

    #[test]
    fn split_carries_the_whole_carveout_across_intervals() {
        // A 0.1% cut of 9 index units rounds to zero on its own. The carried remainder must make
        // the total exact over many intervals.
        let combined_factor = 1000_u128;
        let deposit_interest = 9_u128;
        let intervals = 5000_u128;

        let mut market = carveout_market(1000, 0);
        let (lenders, insurance_fund, protocol) =
            run_splits(&mut market, deposit_interest, intervals);

        assert_eq!(protocol, 0);

        // Exact conservation of the lenders-vs-carveouts split. Every interval adds
        // `deposit_interest * combined_factor` to the numerator, and the division either pays it
        // out or leaves it in the remainder.
        let carried = market.revenue_pool.pending_interest_split_dust as u128;
        assert_eq!(
            insurance_fund * IF_FACTOR_PRECISION + carried,
            intervals * deposit_interest * combined_factor
        );

        // The remainder never reaches a whole unit of its divisor.
        assert!(carried < IF_FACTOR_PRECISION);

        // Lenders receive the rest, so no index unit is created or destroyed.
        assert_eq!(lenders + insurance_fund, intervals * deposit_interest);

        // A single interval of this size still rounds the cut to zero, which is the bug the
        // carry exists to fix.
        let mut single = carveout_market(1000, 0);
        let (_, single_if, _) = run_splits(&mut single, deposit_interest, 1);
        assert_eq!(single_if, 0);
        assert!(insurance_fund > 0);
    }

    #[test]
    fn split_divides_between_the_two_pools_without_loss() {
        let (if_factor, protocol_factor) = (400_000_u128, 500_000_u128);
        let combined_factor = if_factor + protocol_factor;
        let deposit_interest = 7_u128;
        let intervals = 5000_u128;

        let mut market = carveout_market(if_factor as u32, protocol_factor as u32);
        let (lenders, insurance_fund, protocol) =
            run_splits(&mut market, deposit_interest, intervals);

        let withheld = insurance_fund + protocol;
        let carried = market.revenue_pool.pending_interest_split_dust as u128;
        let carried_if = market.protocol_fee_pool.pending_interest_split_dust as u128;

        // First split: lenders against the two cuts together.
        assert_eq!(
            withheld * IF_FACTOR_PRECISION + carried,
            intervals * deposit_interest * combined_factor
        );
        assert_eq!(lenders + withheld, intervals * deposit_interest);

        // Second split: the insurance fund against the protocol, inside what was withheld.
        assert_eq!(
            insurance_fund * combined_factor + carried_if,
            withheld * if_factor
        );
        assert!(carried_if < combined_factor);

        // Each pool ends within one unit of its exact share of the withheld amount.
        let exact_if = withheld * if_factor / combined_factor;
        assert!(insurance_fund.abs_diff(exact_if) <= 1);
    }

    #[test]
    fn token_conversion_carries_dust_into_whole_tokens() {
        // A $1 market. One index unit of withheld interest is worth far less than one token, so
        // every single conversion floors to zero and only the carried remainder ever pays.
        let market = carveout_market(1000, 0);
        let balance = 1_000_000_000_u128;
        let interest = 3_u128;
        let intervals = 20_000_u128;
        let precision_decrease = 10_u128.pow(19 - market.decimals);

        let mut carried = 0_u64;
        let mut paid = 0_u128;
        let mut first_payout = None;

        for _ in 0..intervals {
            let (tokens, dust) =
                get_interest_token_amount_with_dust(balance, &market, interest, carried).unwrap();

            // The remainder is always below one token, which is what keeps it inside a u64.
            assert!((dust as u128) < precision_decrease);

            if tokens > 0 && first_payout.is_none() {
                first_payout = Some(tokens);
            }
            paid += tokens;
            carried = dust;
        }

        // Exact conservation. Every interval adds `balance * interest` to the numerator, and the
        // division either pays it out in whole tokens or leaves it in the remainder.
        assert_eq!(
            paid * precision_decrease + carried as u128,
            intervals * balance * interest
        );

        // A single interval pays nothing, so the payments come only from the carry.
        let (single, _) =
            get_interest_token_amount_with_dust(balance, &market, interest, 0).unwrap();
        assert_eq!(single, 0);
        assert!(paid > 0);

        // The dust crosses one token at a time, so the first payment is exactly one token.
        assert_eq!(first_payout, Some(1));
    }
}
