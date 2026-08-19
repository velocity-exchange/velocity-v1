use {
    crate::{
        math::{
            constants::{QUOTE_PRECISION, SPOT_CUMULATIVE_INTEREST_PRECISION},
            helpers::log10,
            insurance::*,
        },
        state::spot_market::InsuranceFund,
    },
    anchor_lang::prelude::Pubkey,
};

#[test]
pub fn basic_stake_if_test() {
    let (expo_diff, rebase_div) = calculate_rebase_info(10000, 10000).unwrap();
    assert_eq!(rebase_div, 1);
    assert_eq!(expo_diff, 0);

    let (expo_diff, rebase_div) = calculate_rebase_info(20_000, 10000).unwrap();
    assert_eq!(rebase_div, 1);
    assert_eq!(expo_diff, 0);

    let (expo_diff, rebase_div) = calculate_rebase_info(60_078, 10000).unwrap();
    assert_eq!(rebase_div, 1);
    assert_eq!(expo_diff, 0);

    let (expo_diff, rebase_div) = calculate_rebase_info(60_078, 9999).unwrap();
    assert_eq!(rebase_div, 1);
    assert_eq!(expo_diff, 0);

    let (expo_diff, rebase_div) = calculate_rebase_info(60_078, 6008).unwrap();
    assert_eq!(rebase_div, 1);
    assert_eq!(expo_diff, 0);

    let (expo_diff, rebase_div) = calculate_rebase_info(60_078, 6007).unwrap();
    assert_eq!(rebase_div, 1);
    assert_eq!(expo_diff, 0);

    let (expo_diff, rebase_div) = calculate_rebase_info(60_078, 6006).unwrap();
    assert_eq!(rebase_div, 1);
    assert_eq!(expo_diff, 0);

    let (expo_diff, rebase_div) = calculate_rebase_info(60_078, 606).unwrap();
    assert_eq!(rebase_div, 1);
    assert_eq!(expo_diff, 0);

    let (expo_diff, rebase_div) = calculate_rebase_info(60_078, 600).unwrap();
    assert_eq!(rebase_div, 10);
    assert_eq!(expo_diff, 1);

    let (expo_diff, rebase_div) = calculate_rebase_info(
        60_078 * QUOTE_PRECISION,
        ((600 * QUOTE_PRECISION) + 19234) as u64,
    )
    .unwrap();
    assert_eq!(rebase_div, 10);
    assert_eq!(expo_diff, 1);

    let (expo_diff, rebase_div) = calculate_rebase_info(
        60_078 * QUOTE_PRECISION,
        ((601 * QUOTE_PRECISION) + 19234) as u64,
    )
    .unwrap();
    assert_eq!(rebase_div, 1);
    assert_eq!(expo_diff, 0);

    // $800M goes to 1e-6 of dollar
    let (expo_diff, rebase_div) =
        calculate_rebase_info(800_000_078 * QUOTE_PRECISION, 1_u64).unwrap();

    assert_eq!(rebase_div, 10000000000000);
    assert_eq!(expo_diff, 13);

    let (expo_diff, rebase_div) = calculate_rebase_info(99_999, 100).unwrap();
    assert_eq!(log10(100), 2);
    assert_eq!(log10_iter(100), 2);
    assert_eq!(99_999 / 10 / 100, 99);
    assert_eq!(rebase_div, 10);
    assert_eq!(expo_diff, 1);

    let (expo_diff, rebase_div) = calculate_rebase_info(100_000, 100).unwrap();
    assert_eq!(log10(100), 2);
    assert_eq!(100_000 / 10 / 100, 100);
    assert_eq!(rebase_div, 100);
    assert_eq!(expo_diff, 2);

    let (expo_diff, rebase_div) = calculate_rebase_info(100_001, 100).unwrap();
    assert_eq!(log10(100), 2);
    assert_eq!(100_001 / 10 / 100, 100);
    assert_eq!(rebase_div, 100);
    assert_eq!(expo_diff, 2);

    let (expo_diff, rebase_div) = calculate_rebase_info(1_242_418_900_000, 1).unwrap();

    assert_eq!(rebase_div, 100000000000);
    assert_eq!(expo_diff, 11);

    // todo?: does not rebase the other direction (perhaps unnecessary)
    let (expo_diff, rebase_div) = calculate_rebase_info(12412, 83295723895729080).unwrap();

    assert_eq!(rebase_div, 1);
    assert_eq!(expo_diff, 0);
}

#[test]
pub fn if_shares_lost_test() {
    let _amount = QUOTE_PRECISION as u64; // $1
    let mut spot_market = SpotMarket {
        deposit_balance: 0,
        cumulative_deposit_interest: 1111 * SPOT_CUMULATIVE_INTEREST_PRECISION / 1000,
        insurance_fund: InsuranceFund {
            unstaking_period: 0,
            total_shares: 1000 * QUOTE_PRECISION,
            user_shares: 1000 * QUOTE_PRECISION,
            ..InsuranceFund::default()
        },
        ..SpotMarket::default()
    };

    let mut if_stake = InsuranceFundStake::new(Pubkey::default(), 0, 0);
    if_stake
        .update_if_shares(100 * QUOTE_PRECISION, &spot_market)
        .unwrap();
    if_stake.last_withdraw_request_shares = 100 * QUOTE_PRECISION;
    if_stake.last_withdraw_request_value = ((100 * QUOTE_PRECISION) - 1) as u64;

    let if_balance = (1000 * QUOTE_PRECISION) as u64;

    // unchanged balance
    let lost_shares = calculate_if_shares_lost(&if_stake, &spot_market, if_balance).unwrap();
    assert_eq!(lost_shares, 2);

    let if_balance = if_balance + (100 * QUOTE_PRECISION) as u64;
    spot_market.insurance_fund.total_shares += 100 * QUOTE_PRECISION;
    spot_market.insurance_fund.user_shares += 100 * QUOTE_PRECISION;
    let lost_shares = calculate_if_shares_lost(&if_stake, &spot_market, if_balance).unwrap();
    assert_eq!(lost_shares, 2); // giving up $5 of gains

    let if_balance = if_balance - (100 * QUOTE_PRECISION) as u64;
    spot_market.insurance_fund.total_shares -= 100 * QUOTE_PRECISION;
    spot_market.insurance_fund.user_shares -= 100 * QUOTE_PRECISION;
    let lost_shares = calculate_if_shares_lost(&if_stake, &spot_market, if_balance).unwrap();
    assert_eq!(lost_shares, 2); // giving up $5 of gains

    // take back gain
    let if_balance = (1100 * QUOTE_PRECISION) as u64;
    let lost_shares = calculate_if_shares_lost(&if_stake, &spot_market, if_balance).unwrap();
    assert_eq!(lost_shares, 10_000_001); // giving up $10 of gains

    // doesnt matter if theres a loss
    if_stake.last_withdraw_request_value = (200 * QUOTE_PRECISION) as u64;
    let lost_shares = calculate_if_shares_lost(&if_stake, &spot_market, if_balance).unwrap();
    assert_eq!(lost_shares, 0);
    if_stake.last_withdraw_request_value = (100 * QUOTE_PRECISION - 1) as u64;

    // take back gain and total_if_shares alter w/o user alter
    let if_balance = (2100 * QUOTE_PRECISION) as u64;
    spot_market.insurance_fund.total_shares *= 2;
    let lost_shares = calculate_if_shares_lost(&if_stake, &spot_market, if_balance).unwrap();
    assert_eq!(lost_shares, 5_000_001); // giving up $5 of gains

    let if_balance = (2100 * QUOTE_PRECISION * 10) as u64;

    let expected_gain_if_no_loss = if_balance * 100 / 2000;
    assert_eq!(expected_gain_if_no_loss, 1_050_000_000);
    let lost_shares = calculate_if_shares_lost(&if_stake, &spot_market, if_balance).unwrap();
    assert_eq!(lost_shares, 90_909_092); // giving up $5 of gains
    assert_eq!(
        (9090908 * if_balance / ((spot_market.insurance_fund.total_shares - lost_shares) as u64))
            < if_stake.last_withdraw_request_value,
        true
    );
}

#[test]
pub fn if_shares_lost_sole_staker_full_request_test() {
    // finding #108: a staker whose pending request covers the entire fund must keep their
    // position on cancel. The withdraw-and-restake forfeiture accrues to the *remaining*
    // stakers, and a sole staker has none, so nothing is forfeited. Before the guard the
    // restake leg divided into a zero-share pool, returned 0 new shares, and the cancel path
    // burned every share (stake, user_shares and total_shares) while the vault kept the tokens.
    let spot_market = SpotMarket {
        insurance_fund: InsuranceFund {
            unstaking_period: 0,
            total_shares: 100 * QUOTE_PRECISION,
            user_shares: 100 * QUOTE_PRECISION,
            ..InsuranceFund::default()
        },
        ..SpotMarket::default()
    };

    let mut if_stake = InsuranceFundStake::new(Pubkey::default(), 0, 0);
    if_stake
        .update_if_shares(100 * QUOTE_PRECISION, &spot_market)
        .unwrap();
    if_stake.last_withdraw_request_shares = 100 * QUOTE_PRECISION;
    if_stake.last_withdraw_request_value = (100 * QUOTE_PRECISION) as u64;

    // revenue settled into the vault during the escrow window (the fund appreciated 10%), so
    // `amount > last_withdraw_request_value` and the forfeiture branch is reached.
    let if_balance = (110 * QUOTE_PRECISION) as u64;

    let lost_shares = calculate_if_shares_lost(&if_stake, &spot_market, if_balance).unwrap();
    assert_eq!(lost_shares, 0);

    // a staker who is merely large (but not sole) still forfeits the escrow-window gain:
    // the guard is scoped to the degenerate zero-remainder case, not to big positions.
    let mut spot_market_with_others = spot_market;
    spot_market_with_others.insurance_fund.total_shares = 200 * QUOTE_PRECISION;
    spot_market_with_others.insurance_fund.user_shares = 200 * QUOTE_PRECISION;
    let if_balance = (220 * QUOTE_PRECISION) as u64;
    let lost_shares =
        calculate_if_shares_lost(&if_stake, &spot_market_with_others, if_balance).unwrap();
    assert!(lost_shares > 0);
}

#[test]
pub fn deposit_amount_and_shares_charges_only_whole_shares() {
    // share price 1_000_000 (one share against a 1_000_000 vault): a request worth 1.5
    // shares buys one share and is charged one share price, not the full request.
    let (amount_to_deposit, n_shares) =
        deposit_amount_and_shares_for_if_stake(1_500_000, 1, 1_000_000).unwrap();
    assert_eq!(n_shares, 1);
    assert_eq!(amount_to_deposit, 1_000_000);

    // a request below the price of a single share buys nothing (the caller rejects it)
    let (amount_to_deposit, n_shares) =
        deposit_amount_and_shares_for_if_stake(999_999, 1, 1_000_000).unwrap();
    assert_eq!(n_shares, 0);
    assert_eq!(amount_to_deposit, 0);

    // an empty fund mints 1:1, so the whole request is charged
    let (amount_to_deposit, n_shares) =
        deposit_amount_and_shares_for_if_stake(100 * QUOTE_PRECISION as u64, 0, 0).unwrap();
    assert_eq!(n_shares, 100 * QUOTE_PRECISION);
    assert_eq!(amount_to_deposit, 100 * QUOTE_PRECISION as u64);

    // share price below one (post-rebase regime): shares are finer than a token unit, so
    // every unit of the request is spendable
    let (amount_to_deposit, n_shares) =
        deposit_amount_and_shares_for_if_stake(7, 1000, 100).unwrap();
    assert_eq!(n_shares, 70);
    assert_eq!(amount_to_deposit, 7);
}

#[test]
pub fn deposit_amount_and_shares_never_overcharges() {
    // awkward, non-round share price so both roundings bite
    let total_shares = 7_u128;
    let vault_balance = 1_000_003_u64;

    for amount in [
        142_858,
        142_857 * 2,
        1_000_003,
        1_000_004,
        2_000_005,
        7_000_021,
        12_345_678,
    ] {
        let (amount_to_deposit, n_shares) =
            deposit_amount_and_shares_for_if_stake(amount, total_shares, vault_balance).unwrap();

        // never charge more than was requested
        assert!(amount_to_deposit <= amount);

        // leave behind less than the price of one share, i.e. everything that could be
        // converted was: remainder < vault_balance / total_shares
        assert!((amount - amount_to_deposit) as u128 * total_shares < vault_balance as u128);

        // the minted shares are never worth more than what was charged for them:
        // n_shares * (vault + deposit) / (total_shares + n_shares) <= deposit, so a
        // deposit followed by an immediate withdraw cannot turn a profit
        let vault_after = vault_balance as u128 + amount_to_deposit as u128;
        let shares_after = total_shares + n_shares;
        assert!(n_shares * vault_after <= amount_to_deposit as u128 * shares_after);
    }
}
