mod get_init_user_fee {
    use crate::State;

    #[test]
    fn it_works() {
        let state = State::default();
        let init_user_fee = state.get_init_user_fee().unwrap();
        assert_eq!(init_user_fee, 0);

        let state = State {
            max_initialize_user_fee: 1,
            max_number_of_sub_accounts: 10,
            number_of_sub_accounts: 800,
            ..State::default()
        };

        let max_number_of_sub_accounts = state.max_number_of_sub_accounts();
        assert_eq!(max_number_of_sub_accounts, 1000);

        let init_user_fee = state.get_init_user_fee().unwrap();
        assert_eq!(init_user_fee, 0);

        let state = State {
            max_initialize_user_fee: 1,
            max_number_of_sub_accounts: 10,
            number_of_sub_accounts: 900,
            ..State::default()
        };

        let init_user_fee = state.get_init_user_fee().unwrap();
        assert_eq!(init_user_fee, 5000000);

        let state = State {
            max_initialize_user_fee: 1,
            max_number_of_sub_accounts: 10,
            number_of_sub_accounts: 1000,
            ..State::default()
        };

        let init_user_fee = state.get_init_user_fee().unwrap();
        assert_eq!(init_user_fee, 10000000);

        let state = State {
            max_initialize_user_fee: 100,
            max_number_of_sub_accounts: 10,
            number_of_sub_accounts: 1000,
            ..State::default()
        };

        let init_user_fee = state.get_init_user_fee().unwrap();
        assert_eq!(init_user_fee, 1000000000);
    }
}

mod escrow_period_before_transfer {
    use crate::{math::constants::TWENTY_FOUR_HOUR, state::state::State};

    /// The delist and `forfeit_revenue_share_order` both read this window, so they must agree on
    /// when an expired market may close. The window is at least a day, so a revenue-share
    /// beneficiary always has a day to create the payout account that keeps their claim.
    #[test]
    fn window_is_at_least_a_day() {
        let state = State {
            settlement_duration: 2,
            ..State::default()
        };
        assert_eq!(
            state.escrow_period_before_transfer().unwrap(),
            TWENTY_FOUR_HOUR + 1
        );

        let long = State {
            settlement_duration: 1_000,
            ..State::default()
        };
        assert_eq!(
            long.escrow_period_before_transfer().unwrap(),
            TWENTY_FOUR_HOUR + 999
        );
    }

    /// A `settlement_duration` of 1 shortens the window for tests.
    #[test]
    fn a_duration_of_one_shortens_the_window() {
        let state = State {
            settlement_duration: 1,
            ..State::default()
        };
        assert_eq!(state.escrow_period_before_transfer().unwrap(), 1);
    }
}

mod conditions_sync_role {
    use {
        crate::state::state::{HotRole, State},
        anchor_lang::prelude::Pubkey,
    };

    #[test]
    fn the_role_key_and_the_admins_may_sync_and_no_one_else() {
        let cold = Pubkey::new_unique();
        let warm = Pubkey::new_unique();
        let keeper = Pubkey::new_unique();
        let mut state = State {
            cold_admin: cold,
            warm_admin: warm,
            ..State::default()
        };

        assert!(!state.is_hot(&keeper, HotRole::ConditionsSync));
        assert!(!state.is_hot(&Pubkey::default(), HotRole::ConditionsSync));

        state.set_hot_key(HotRole::ConditionsSync, keeper);

        assert!(state.is_hot(&keeper, HotRole::ConditionsSync));
        assert!(state.is_hot(&warm, HotRole::ConditionsSync));
        assert!(state.is_hot(&cold, HotRole::ConditionsSync));
        assert!(!state.is_hot(&keeper, HotRole::AccountExtension));
        assert!(!state.is_hot(&Pubkey::new_unique(), HotRole::ConditionsSync));
    }
}
