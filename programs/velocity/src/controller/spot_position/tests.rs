mod update_spot_position_balance {
    use crate::{
        controller::spot_position::{
            transfer_spot_position_deposit, update_spot_balances_and_cumulative_deposits,
        },
        math::constants::{
            LAMPORTS_PER_SOL_I64, SPOT_BALANCE_PRECISION, SPOT_BALANCE_PRECISION_U64,
        },
        state::{
            spot_market::{SpotBalanceType, SpotMarket},
            user::{SpotPosition, User},
        },
    };

    #[test]
    fn deposit() {
        let mut user = User::default();
        let mut spot_market = SpotMarket::default_quote_market();

        let token_amount = 100_u128;
        update_spot_balances_and_cumulative_deposits(
            token_amount,
            &SpotBalanceType::Deposit,
            &mut spot_market,
            user.get_quote_spot_position_mut(),
            false,
            None,
        )
        .unwrap();

        assert_eq!(user.get_quote_spot_position_mut().cumulative_deposits, 100);
    }

    #[test]
    fn borrow() {
        let mut user = User::default();
        let mut spot_market = SpotMarket {
            deposit_balance: 101 * SPOT_BALANCE_PRECISION,
            ..SpotMarket::default_quote_market()
        };

        let token_amount = 100_u128;
        update_spot_balances_and_cumulative_deposits(
            token_amount,
            &SpotBalanceType::Borrow,
            &mut spot_market,
            user.get_quote_spot_position_mut(),
            false,
            None,
        )
        .unwrap();

        assert_eq!(user.get_quote_spot_position_mut().cumulative_deposits, -100);
    }

    #[test]
    fn transfer() {
        let mut user = User::default();
        let mut user2 = User::default();

        let mut spot_market = SpotMarket {
            deposit_balance: 101 * SPOT_BALANCE_PRECISION,
            ..SpotMarket::default_quote_market()
        };

        let token_amount = 100_i128;
        transfer_spot_position_deposit(
            token_amount,
            &mut spot_market,
            user.get_quote_spot_position_mut(),
            user2.get_quote_spot_position_mut(),
        )
        .unwrap();

        assert_eq!(user.get_quote_spot_position_mut().cumulative_deposits, -100);
        assert_eq!(user2.get_quote_spot_position_mut().cumulative_deposits, 100);

        transfer_spot_position_deposit(
            -token_amount * 2,
            &mut spot_market,
            user.get_quote_spot_position_mut(),
            user2.get_quote_spot_position_mut(),
        )
        .unwrap();

        assert_eq!(user.get_quote_spot_position_mut().cumulative_deposits, 100);
        assert_eq!(
            user2.get_quote_spot_position_mut().cumulative_deposits,
            -100
        );
    }

    #[test]
    fn transfer_fail() {
        let mut user = User::default();

        let mut spot_positions = [SpotPosition::default(); 8];
        spot_positions[0] = SpotPosition {
            market_index: 0,
            balance_type: SpotBalanceType::Deposit,
            scaled_balance: SPOT_BALANCE_PRECISION_U64,
            ..SpotPosition::default()
        };

        spot_positions[1] = SpotPosition {
            market_index: 1,
            open_orders: 1,
            open_bids: LAMPORTS_PER_SOL_I64,
            ..SpotPosition::default()
        };

        let mut user2 = User {
            spot_positions,
            ..User::default()
        };

        let mut spot_market = SpotMarket {
            deposit_balance: 101 * SPOT_BALANCE_PRECISION,
            ..SpotMarket::default_quote_market()
        };

        let mut sol_market = SpotMarket {
            deposit_balance: 101 * SPOT_BALANCE_PRECISION,
            ..SpotMarket::default_base_market()
        };

        let token_amount = 100_i128;
        assert!(transfer_spot_position_deposit(
            token_amount,
            &mut spot_market,
            user.get_quote_spot_position_mut(),
            user2.get_spot_position_mut(1).unwrap(),
        )
        .is_err());

        let token_amount = 100_i128;
        assert!(transfer_spot_position_deposit(
            token_amount,
            &mut sol_market,
            user.get_quote_spot_position_mut(),
            user2.get_spot_position_mut(1).unwrap(),
        )
        .is_err());
    }
}

/// OtterSec #118: the daily deposit cap must throttle deposit *growth*, not lock a market that is
/// already over its cap. `check_deposit_limits` is a market-wide level predicate, so validating it
/// on every trip through the shared credit path made an over-cap market reject withdrawals and
/// repayments too — the very actions that bring the level back down — while liquidation, which does
/// not use this path, stayed live against those same users.
mod deposit_cap_does_not_lock_exits {
    use crate::{
        controller::spot_position::update_spot_balances_and_cumulative_deposits_with_limits,
        error::ErrorCode,
        math::constants::{
            BPS_PRECISION, QUOTE_PRECISION_U64, SPOT_BALANCE_PRECISION, SPOT_BALANCE_PRECISION_U64,
            SPOT_CUMULATIVE_INTEREST_PRECISION,
        },
        state::{
            market_status::MarketStatus,
            oracle::OracleSource,
            spot_market::{SpotBalanceType, SpotMarket},
            user::User,
        },
    };

    /// A market sitting far above its daily deposit cap: the 24h TWAP is $100 with a 20%/day cap
    /// (so the ceiling is $120), while actual deposits are $300.
    fn over_cap_market() -> SpotMarket {
        SpotMarket {
            market_index: 0,
            oracle_source: OracleSource::QuoteAsset,
            cumulative_deposit_interest: SPOT_CUMULATIVE_INTEREST_PRECISION,
            cumulative_borrow_interest: SPOT_CUMULATIVE_INTEREST_PRECISION,
            decimals: 6,
            deposit_balance: 300 * SPOT_BALANCE_PRECISION,
            deposit_token_twap: 100 * QUOTE_PRECISION_U64,
            max_deposit_bps_per_day: (BPS_PRECISION / 5) as u16, // 2000 bps = 20%/day
            withdraw_guard_threshold: u64::MAX,                  // isolate the deposit cap
            status: MarketStatus::Active,
            ..SpotMarket::default_quote_market()
        }
    }

    fn depositor() -> User {
        let mut user = User::default();
        let position = user.get_quote_spot_position_mut();
        position.balance_type = SpotBalanceType::Deposit;
        position.scaled_balance = 200 * SPOT_BALANCE_PRECISION_U64;
        user
    }

    #[test]
    fn withdrawing_from_an_over_cap_market_is_allowed() {
        let mut spot_market = over_cap_market();
        let mut user = depositor();

        // Sanity: the market really is over its cap, so the level predicate is false.
        assert!(!crate::math::spot_withdraw::check_deposit_limits(&spot_market).unwrap());

        // A withdrawal lowers the deposit level and must not be rejected, even though the market
        // stays above its cap afterwards.
        update_spot_balances_and_cumulative_deposits_with_limits(
            (10 * QUOTE_PRECISION_U64) as u128,
            &SpotBalanceType::Borrow,
            &mut spot_market,
            &mut user,
        )
        .expect("withdrawal blocked by the deposit cap");

        assert!(!crate::math::spot_withdraw::check_deposit_limits(&spot_market).unwrap());
    }

    #[test]
    fn depositing_into_an_over_cap_market_is_still_rejected() {
        let mut spot_market = over_cap_market();
        let mut user = depositor();

        let result = update_spot_balances_and_cumulative_deposits_with_limits(
            (10 * QUOTE_PRECISION_U64) as u128,
            &SpotBalanceType::Deposit,
            &mut spot_market,
            &mut user,
        );

        assert_eq!(result, Err(ErrorCode::DailyDepositLimit));
    }

    #[test]
    fn depositing_within_the_cap_is_allowed() {
        let mut spot_market = SpotMarket {
            deposit_balance: 100 * SPOT_BALANCE_PRECISION,
            ..over_cap_market()
        };
        let mut user = depositor();

        // $100 deposits against a $120 ceiling: $10 more stays inside the cap.
        update_spot_balances_and_cumulative_deposits_with_limits(
            (10 * QUOTE_PRECISION_U64) as u128,
            &SpotBalanceType::Deposit,
            &mut spot_market,
            &mut user,
        )
        .expect("deposit within the cap was rejected");

        assert!(crate::math::spot_withdraw::check_deposit_limits(&spot_market).unwrap());
    }
}
