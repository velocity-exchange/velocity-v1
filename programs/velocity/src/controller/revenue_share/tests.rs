//! Unit tests for the permissionless revenue-share paths:
//! [`sweep_completed_revenue_share_for_market`], [`super::resolve_revenue_share_forfeit_reason`]
//! and [`super::forfeit_revenue_share_order`].
//!
//! The sweep is permissionless and it moves tokens out of a perp market's pnl
//! pool into a beneficiary's quote spot position. Each test asserts balances,
//! counters, and escrow rows before and after the call. Control-flow assertions
//! alone cannot see a missing transfer, so every case pins money.
//!
//! The forfeit is permissionless too, and it destroys a claim. The proof tests
//! at the end of this file pin every branch that can write off a row, and every
//! condition that must keep one alive.

use {
    super::sweep_completed_revenue_share_for_market,
    crate::{
        create_anchor_account_info,
        error::ErrorCode,
        math::{
            constants::{
                BASE_PRECISION_I128, PERCENTAGE_PRECISION_U32, PRICE_PRECISION_I64,
                QUOTE_PRECISION, QUOTE_PRECISION_I128, QUOTE_PRECISION_U64,
                QUOTE_SPOT_MARKET_INDEX, SPOT_BALANCE_PRECISION, SPOT_BALANCE_PRECISION_U64,
                SPOT_CUMULATIVE_INTEREST_PRECISION,
            },
            spot_balance::get_token_amount,
        },
        state::{
            market_status::MarketStatus,
            oracle::{HistoricalOracleData, OracleSource},
            paused_operations::PerpOperation,
            perp_market::{FeeLedger, MarketStats, PerpMarket, PoolBalance},
            perp_market_map::PerpMarketMap,
            revenue_share::{
                BuilderInfo, RevenueShare, RevenueShareEscrow, RevenueShareEscrowFixed,
                RevenueShareEscrowZeroCopyMut, RevenueShareOrder, RevenueShareOrderBitFlag,
            },
            revenue_share_map::{load_revenue_share_map, RevenueShareMap},
            spot_market::{SpotBalance, SpotBalanceType, SpotMarket},
            spot_market_map::SpotMarketMap,
            user::{MarketType, SpotPosition, User, UserStatus},
        },
        test_utils::get_spot_positions,
        vlp::amm::{math::amm::calculate_net_user_pnl, AMM},
    },
    anchor_lang::Discriminator,
    solana_program::pubkey::Pubkey,
    std::cell::{RefCell, RefMut},
};

// ---------------------------------------------------------------------------
// units
// ---------------------------------------------------------------------------

/// One dollar of quote tokens. Fee amounts use QUOTE_PRECISION.
const DOLLAR: u64 = QUOTE_PRECISION_U64;
/// One dollar of quote spot balance. Spot balances use SPOT_BALANCE_PRECISION.
const DOLLAR_BALANCE: u64 = SPOT_BALANCE_PRECISION_U64;
/// Opening deposit of every beneficiary. A payout must add to this value.
const BENEFICIARY_START: u64 = 10 * DOLLAR_BALANCE;

/// Converts dollars to a pnl-pool scaled balance.
fn pool_balance(dollars: u64) -> u128 {
    dollars as u128 * SPOT_BALANCE_PRECISION
}

/// Converts dollars to quote tokens.
fn tokens(dollars: u64) -> u128 {
    dollars as u128 * QUOTE_PRECISION
}

// ---------------------------------------------------------------------------
// harness
// ---------------------------------------------------------------------------

/// Builds the quote spot market. `decimals = 6` makes a scaled balance of
/// `SPOT_BALANCE_PRECISION` worth exactly one dollar of tokens. `deposit_balance`
/// is large so `transfer_spot_balances` never trips its solvency check.
fn quote_spot_market() -> SpotMarket {
    SpotMarket {
        market_index: QUOTE_SPOT_MARKET_INDEX,
        oracle_source: OracleSource::QuoteAsset,
        decimals: 6,
        cumulative_deposit_interest: SPOT_CUMULATIVE_INTEREST_PRECISION,
        cumulative_borrow_interest: SPOT_CUMULATIVE_INTEREST_PRECISION,
        deposit_balance: pool_balance(1_000_000),
        ..SpotMarket::default()
    }
}

/// Builds perp market 0 with `pnl_pool_dollars` in its pnl pool and
/// `pending_revenue_share` dollars of unpaid builder/referrer claims. The AMM is
/// flat, so `net_user_pnl` is zero and nothing is reserved.
fn perp_market(pnl_pool_dollars: u64, pending_revenue_share_dollars: u64) -> PerpMarket {
    PerpMarket {
        market_index: 0,
        pnl_pool: PoolBalance {
            scaled_balance: pool_balance(pnl_pool_dollars),
            market_index: QUOTE_SPOT_MARKET_INDEX,
            ..PoolBalance::default()
        },
        pending_revenue_share: pending_revenue_share_dollars * DOLLAR,
        ..PerpMarket::default()
    }
}

/// Builds a beneficiary User. `sub_account_id` is 0 because the revenue-share
/// map only accepts subaccount 0. Slot 0 of `spot_positions` must be the quote
/// market or `get_quote_spot_position_mut` panics.
fn beneficiary(authority: Pubkey, vault_owned: bool) -> User {
    User {
        authority,
        sub_account_id: 0,
        spot_positions: get_spot_positions(SpotPosition {
            market_index: QUOTE_SPOT_MARKET_INDEX,
            balance_type: SpotBalanceType::Deposit,
            scaled_balance: BENEFICIARY_START,
            ..SpotPosition::default()
        }),
        status: if vault_owned {
            UserStatus::VaultOwned as u8
        } else {
            0
        },
        ..User::default()
    }
}

fn revenue_share(authority: Pubkey) -> RevenueShare {
    RevenueShare {
        authority,
        ..RevenueShare::default()
    }
}

fn builder_info(authority: Pubkey) -> BuilderInfo {
    BuilderInfo {
        authority,
        max_fee_tenth_bps: 100,
        padding: [0; 6],
    }
}

/// A builder row that is ready to sweep: Completed, on a perp market, with fees.
fn completed_builder_row(market_index: u16, fee_dollars: u64) -> RevenueShareOrder {
    let mut order = RevenueShareOrder::new(
        0,
        0,
        1,
        100,
        MarketType::Perp,
        market_index,
        RevenueShareOrderBitFlag::Open as u8,
        0,
    );
    order.add_bit_flag(RevenueShareOrderBitFlag::Completed);
    order.fees_accrued = fee_dollars * DOLLAR;
    order
}

/// A builder row whose order is still live. The sweep must leave it alone.
fn open_builder_row(market_index: u16, fee_dollars: u64) -> RevenueShareOrder {
    let mut order = RevenueShareOrder::new(
        0,
        0,
        2,
        100,
        MarketType::Perp,
        market_index,
        RevenueShareOrderBitFlag::Open as u8,
        0,
    );
    order.fees_accrued = fee_dollars * DOLLAR;
    order
}

/// A referral row. Referral rows carry only the Referral flag.
fn referral_row(market_index: u16, fee_dollars: u64) -> RevenueShareOrder {
    let mut order = RevenueShareOrder::new(
        0,
        0,
        0,
        0,
        MarketType::Perp,
        market_index,
        RevenueShareOrderBitFlag::Referral as u8,
        0,
    );
    order.fees_accrued = fee_dollars * DOLLAR;
    order
}

/// Serializes an escrow account into a 16-byte-aligned buffer. The layout is the
/// one the production loader reads: discriminator, fixed header, `padding0`,
/// orders length, orders, `padding1`, builders length, builders. Returns the
/// backing store and the account data length.
fn escrow_backing(
    orders: &[RevenueShareOrder],
    builders: &[BuilderInfo],
    referrer: Pubkey,
) -> (Vec<u128>, usize) {
    let len = RevenueShareEscrow::space(orders.len(), builders.len());
    let mut backing = vec![0u128; len.div_ceil(16)];
    {
        let full: &mut [u8] = bytemuck::cast_slice_mut(&mut backing);
        let buf = &mut full[..len];
        buf[0..8].copy_from_slice(RevenueShareEscrow::DISCRIMINATOR);
        // fixed header: authority at 8, referrer at 40, reserved at 72
        buf[40..72].copy_from_slice(referrer.as_ref());

        let header = 8 + std::mem::size_of::<RevenueShareEscrowFixed>();
        let order_size = std::mem::size_of::<RevenueShareOrder>();
        buf[header + 4..header + 8].copy_from_slice(&(orders.len() as u32).to_le_bytes());
        for (i, order) in orders.iter().enumerate() {
            let start = header + 8 + i * order_size;
            buf[start..start + order_size].copy_from_slice(bytemuck::bytes_of(order));
        }

        let builders_len_offset = header + 12 + orders.len() * order_size;
        let builder_size = std::mem::size_of::<BuilderInfo>();
        buf[builders_len_offset..builders_len_offset + 4]
            .copy_from_slice(&(builders.len() as u32).to_le_bytes());
        for (i, builder) in builders.iter().enumerate() {
            let start = builders_len_offset + 4 + i * builder_size;
            buf[start..start + builder_size].copy_from_slice(bytemuck::bytes_of(builder));
        }
    }
    (backing, len)
}

/// Loads an escrow zero-copy view over a fresh backing buffer. This mirrors the
/// split `load_zc_mut` performs, without the AccountInfo plumbing.
macro_rules! escrow {
    ($orders:expr, $builders:expr, $referrer:expr, $name:ident) => {
        let (mut escrow_store, escrow_len) = escrow_backing($orders, $builders, $referrer);
        let escrow_bytes: &mut [u8] = bytemuck::cast_slice_mut(&mut escrow_store);
        let escrow_cell = RefCell::new(&mut escrow_bytes[..escrow_len]);
        let escrow_data = RefMut::map(escrow_cell.borrow_mut(), |d| &mut **d);
        let (_disc, escrow_data) = RefMut::map_split(escrow_data, |d| d.split_at_mut(8));
        let (escrow_fixed, escrow_data) = RefMut::map_split(escrow_data, |d| {
            d.split_at_mut(std::mem::size_of::<RevenueShareEscrowFixed>())
        });
        let mut $name = RevenueShareEscrowZeroCopyMut {
            fixed: RefMut::map(escrow_fixed, |b| bytemuck::from_bytes_mut(b)),
            data: escrow_data,
        };
    };
}

/// Reads the pnl pool of market 0, in quote tokens.
fn pnl_pool_tokens(perp_market_map: &PerpMarketMap, spot_market_map: &SpotMarketMap) -> u128 {
    let market = perp_market_map.get_ref(&0).unwrap();
    let spot_market = spot_market_map.get_ref(&QUOTE_SPOT_MARKET_INDEX).unwrap();
    get_token_amount(
        market.pnl_pool.scaled_balance,
        &spot_market,
        market.pnl_pool.balance_type(),
    )
    .unwrap()
}

/// Reads a beneficiary's quote spot balance, in SPOT_BALANCE_PRECISION.
fn quote_balance(revenue_share_map: &RevenueShareMap, authority: &Pubkey) -> u64 {
    revenue_share_map
        .get_user_ref_mut(authority)
        .unwrap()
        .spot_positions[0]
        .scaled_balance
}

/// Reads `(total_referrer_rewards, total_builder_rewards)` for an authority.
fn reward_totals(revenue_share_map: &RevenueShareMap, authority: &Pubkey) -> (u64, u64) {
    let account = revenue_share_map
        .get_revenue_share_account_mut(authority)
        .unwrap();
    (
        account.total_referrer_rewards,
        account.total_builder_rewards,
    )
}

// ---------------------------------------------------------------------------
// happy paths and row lifecycle
// ---------------------------------------------------------------------------

/// A completed builder row pays the builder out of the pnl pool. This pins the
/// whole money path: the pool falls, the builder's spot balance rises by the
/// same amount, the market's liability counter falls, and the reward total
/// rises. It also pins that a builder row is fully cleared, which frees the
/// escrow slot for reuse.
#[test]
fn builder_row_pays_builder_and_frees_the_slot() {
    let builder_authority = Pubkey::new_unique();

    let mut market = perp_market(100, 5);
    create_anchor_account_info!(market, PerpMarket, market_info);
    let perp_market_map = PerpMarketMap::load_one(&market_info, true).unwrap();

    let mut spot_market = quote_spot_market();
    create_anchor_account_info!(spot_market, SpotMarket, spot_market_info);
    let spot_market_map = SpotMarketMap::load_one(&spot_market_info, true).unwrap();

    let mut builder_user = beneficiary(builder_authority, false);
    create_anchor_account_info!(builder_user, User, builder_user_info);
    let mut builder_rev_share = revenue_share(builder_authority);
    create_anchor_account_info!(builder_rev_share, RevenueShare, builder_rev_share_info);
    let accounts = vec![builder_user_info, builder_rev_share_info];
    let mut account_iter = accounts.iter().peekable();
    let revenue_share_map = load_revenue_share_map(&mut account_iter).unwrap();

    escrow!(
        &[completed_builder_row(0, 5)],
        &[builder_info(builder_authority)],
        Pubkey::default(),
        escrow
    );

    sweep_completed_revenue_share_for_market(
        0,
        &mut escrow,
        &perp_market_map,
        &spot_market_map,
        &revenue_share_map,
        0,
        100 * PRICE_PRECISION_I64,
        true,
        false,
    )
    .unwrap();

    // the builder received exactly five dollars on top of the opening deposit
    assert_eq!(
        quote_balance(&revenue_share_map, &builder_authority),
        BENEFICIARY_START + 5 * DOLLAR_BALANCE
    );
    // the pnl pool paid exactly five dollars
    assert_eq!(
        pnl_pool_tokens(&perp_market_map, &spot_market_map),
        tokens(95)
    );
    // the market's unpaid revenue-share liability is discharged
    assert_eq!(
        perp_market_map.get_ref(&0).unwrap().pending_revenue_share,
        0
    );
    assert_eq!(
        reward_totals(&revenue_share_map, &builder_authority),
        (0, 5 * DOLLAR)
    );
    // a builder row is reset to default, so the slot is available again
    assert_eq!(escrow.get_order(0).unwrap(), &RevenueShareOrder::default());
    assert!(escrow.get_order(0).unwrap().is_available());
}

/// A referral row pays the referrer out of the pnl pool and keeps the row with
/// zero fees. The retained row is the difference from the builder path. It
/// permanently occupies an escrow slot, so escrow capacity depends on this
/// behaviour staying as it is.
#[test]
fn referral_row_pays_referrer_and_keeps_the_row() {
    let referrer_authority = Pubkey::new_unique();

    let mut market = perp_market(100, 4);
    create_anchor_account_info!(market, PerpMarket, market_info);
    let perp_market_map = PerpMarketMap::load_one(&market_info, true).unwrap();

    let mut spot_market = quote_spot_market();
    create_anchor_account_info!(spot_market, SpotMarket, spot_market_info);
    let spot_market_map = SpotMarketMap::load_one(&spot_market_info, true).unwrap();

    let mut referrer_user = beneficiary(referrer_authority, false);
    create_anchor_account_info!(referrer_user, User, referrer_user_info);
    let mut referrer_rev_share = revenue_share(referrer_authority);
    create_anchor_account_info!(referrer_rev_share, RevenueShare, referrer_rev_share_info);
    let accounts = vec![referrer_user_info, referrer_rev_share_info];
    let mut account_iter = accounts.iter().peekable();
    let revenue_share_map = load_revenue_share_map(&mut account_iter).unwrap();

    escrow!(&[referral_row(0, 4)], &[], referrer_authority, escrow);

    sweep_completed_revenue_share_for_market(
        0,
        &mut escrow,
        &perp_market_map,
        &spot_market_map,
        &revenue_share_map,
        0,
        100 * PRICE_PRECISION_I64,
        true,
        false,
    )
    .unwrap();

    assert_eq!(
        quote_balance(&revenue_share_map, &referrer_authority),
        BENEFICIARY_START + 4 * DOLLAR_BALANCE
    );
    assert_eq!(
        pnl_pool_tokens(&perp_market_map, &spot_market_map),
        tokens(96)
    );
    assert_eq!(
        perp_market_map.get_ref(&0).unwrap().pending_revenue_share,
        0
    );
    assert_eq!(
        reward_totals(&revenue_share_map, &referrer_authority),
        (4 * DOLLAR, 0)
    );

    // the row survives with zero fees, so the slot stays occupied
    let row = escrow.get_order(0).unwrap();
    assert_eq!(row.fees_accrued, 0);
    assert!(row.is_referral_order());
    assert!(!row.is_available());
}

/// The sweep only touches rows for this market that are ready to pay. This test
/// puts four rows the sweep must skip next to one it must pay. The paid row is
/// the control that proves the loop ran over all of them.
#[test]
fn sweep_skips_rows_that_are_not_payable_for_this_market() {
    let builder_authority = Pubkey::new_unique();
    let referrer_authority = Pubkey::new_unique();

    let mut market = perp_market(100, 11);
    create_anchor_account_info!(market, PerpMarket, market_info);
    let perp_market_map = PerpMarketMap::load_one(&market_info, true).unwrap();

    let mut spot_market = quote_spot_market();
    create_anchor_account_info!(spot_market, SpotMarket, spot_market_info);
    let spot_market_map = SpotMarketMap::load_one(&spot_market_info, true).unwrap();

    let mut builder_user = beneficiary(builder_authority, false);
    create_anchor_account_info!(builder_user, User, builder_user_info);
    let mut builder_rev_share = revenue_share(builder_authority);
    create_anchor_account_info!(builder_rev_share, RevenueShare, builder_rev_share_info);
    let mut referrer_user = beneficiary(referrer_authority, false);
    create_anchor_account_info!(referrer_user, User, referrer_user_info);
    let mut referrer_rev_share = revenue_share(referrer_authority);
    create_anchor_account_info!(referrer_rev_share, RevenueShare, referrer_rev_share_info);
    let accounts = vec![
        builder_user_info,
        builder_rev_share_info,
        referrer_user_info,
        referrer_rev_share_info,
    ];
    let mut account_iter = accounts.iter().peekable();
    let revenue_share_map = load_revenue_share_map(&mut account_iter).unwrap();

    let rows = [
        open_builder_row(0, 2),      // still live, not Completed
        completed_builder_row(1, 2), // another market
        completed_builder_row(0, 0), // nothing accrued
        referral_row(1, 2),          // referral for another market
        completed_builder_row(0, 3), // payable
    ];
    escrow!(
        &rows,
        &[builder_info(builder_authority)],
        referrer_authority,
        escrow
    );

    sweep_completed_revenue_share_for_market(
        0,
        &mut escrow,
        &perp_market_map,
        &spot_market_map,
        &revenue_share_map,
        0,
        100 * PRICE_PRECISION_I64,
        true,
        false,
    )
    .unwrap();

    // only the payable row moved money
    assert_eq!(
        quote_balance(&revenue_share_map, &builder_authority),
        BENEFICIARY_START + 3 * DOLLAR_BALANCE
    );
    assert_eq!(
        pnl_pool_tokens(&perp_market_map, &spot_market_map),
        tokens(97)
    );
    // the referral row for market 1 was not paid
    assert_eq!(
        quote_balance(&revenue_share_map, &referrer_authority),
        BENEFICIARY_START
    );
    // eleven dollars were owed and three were paid
    assert_eq!(
        perp_market_map.get_ref(&0).unwrap().pending_revenue_share,
        8 * DOLLAR
    );
    // the skipped rows keep their fees and their flags
    for (i, expected) in rows.iter().enumerate().take(4) {
        assert_eq!(escrow.get_order(i as u32).unwrap(), expected, "row {}", i);
    }
    assert_eq!(escrow.get_order(4).unwrap(), &RevenueShareOrder::default());
}

// ---------------------------------------------------------------------------
// vault-owned beneficiaries forfeit (OtterSec #91/#92/#93)
// ---------------------------------------------------------------------------

/// A vault-owned referrer must never be credited. Its User prices vault
/// depositor shares, so a reward that lands at this attacker-chosen sweep time
/// mis-splits depositor value. The reward is forfeited to the pnl pool: the
/// liability counter drains and the row clears with no transfer.
#[test]
fn vault_owned_referrer_forfeits_the_reward() {
    let referrer_authority = Pubkey::new_unique();

    let mut market = perp_market(100, 4);
    create_anchor_account_info!(market, PerpMarket, market_info);
    let perp_market_map = PerpMarketMap::load_one(&market_info, true).unwrap();

    let mut spot_market = quote_spot_market();
    create_anchor_account_info!(spot_market, SpotMarket, spot_market_info);
    let spot_market_map = SpotMarketMap::load_one(&spot_market_info, true).unwrap();

    let mut referrer_user = beneficiary(referrer_authority, true);
    create_anchor_account_info!(referrer_user, User, referrer_user_info);
    let mut referrer_rev_share = revenue_share(referrer_authority);
    create_anchor_account_info!(referrer_rev_share, RevenueShare, referrer_rev_share_info);
    let accounts = vec![referrer_user_info, referrer_rev_share_info];
    let mut account_iter = accounts.iter().peekable();
    let revenue_share_map = load_revenue_share_map(&mut account_iter).unwrap();

    // control: the vault flag is the only thing that stops the payout. The
    // identical non-vault case in `referral_row_pays_referrer_and_keeps_the_row`
    // credits four dollars from the same pool and the same row.
    assert!(revenue_share_map
        .get_user_ref_mut(&referrer_authority)
        .unwrap()
        .is_vault_owned());

    escrow!(&[referral_row(0, 4)], &[], referrer_authority, escrow);

    sweep_completed_revenue_share_for_market(
        0,
        &mut escrow,
        &perp_market_map,
        &spot_market_map,
        &revenue_share_map,
        0,
        100 * PRICE_PRECISION_I64,
        true,
        false,
    )
    .unwrap();

    // nothing was credited and the pnl pool keeps the tokens
    assert_eq!(
        quote_balance(&revenue_share_map, &referrer_authority),
        BENEFICIARY_START
    );
    assert_eq!(
        pnl_pool_tokens(&perp_market_map, &spot_market_map),
        tokens(100)
    );
    assert_eq!(
        reward_totals(&revenue_share_map, &referrer_authority),
        (0, 0)
    );
    // the claim is discharged and the row is cleared, so it cannot be replayed
    assert_eq!(
        perp_market_map.get_ref(&0).unwrap().pending_revenue_share,
        0
    );
    assert_eq!(escrow.get_order(0).unwrap().fees_accrued, 0);
    assert!(escrow.get_order(0).unwrap().is_referral_order());
}

/// A vault-owned builder must never be credited, for the same reason as a
/// vault-owned referrer. The builder branch also resets the row to default, so
/// the escrow slot is freed.
#[test]
fn vault_owned_builder_forfeits_the_reward_and_frees_the_slot() {
    let builder_authority = Pubkey::new_unique();

    let mut market = perp_market(100, 5);
    create_anchor_account_info!(market, PerpMarket, market_info);
    let perp_market_map = PerpMarketMap::load_one(&market_info, true).unwrap();

    let mut spot_market = quote_spot_market();
    create_anchor_account_info!(spot_market, SpotMarket, spot_market_info);
    let spot_market_map = SpotMarketMap::load_one(&spot_market_info, true).unwrap();

    let mut builder_user = beneficiary(builder_authority, true);
    create_anchor_account_info!(builder_user, User, builder_user_info);
    let mut builder_rev_share = revenue_share(builder_authority);
    create_anchor_account_info!(builder_rev_share, RevenueShare, builder_rev_share_info);
    let accounts = vec![builder_user_info, builder_rev_share_info];
    let mut account_iter = accounts.iter().peekable();
    let revenue_share_map = load_revenue_share_map(&mut account_iter).unwrap();

    // control: the vault flag is the only difference from
    // `builder_row_pays_builder_and_frees_the_slot`, which pays five dollars.
    assert!(revenue_share_map
        .get_user_ref_mut(&builder_authority)
        .unwrap()
        .is_vault_owned());

    escrow!(
        &[completed_builder_row(0, 5)],
        &[builder_info(builder_authority)],
        Pubkey::default(),
        escrow
    );

    sweep_completed_revenue_share_for_market(
        0,
        &mut escrow,
        &perp_market_map,
        &spot_market_map,
        &revenue_share_map,
        0,
        100 * PRICE_PRECISION_I64,
        true,
        false,
    )
    .unwrap();

    assert_eq!(
        quote_balance(&revenue_share_map, &builder_authority),
        BENEFICIARY_START
    );
    assert_eq!(
        pnl_pool_tokens(&perp_market_map, &spot_market_map),
        tokens(100)
    );
    assert_eq!(
        reward_totals(&revenue_share_map, &builder_authority),
        (0, 0)
    );
    assert_eq!(
        perp_market_map.get_ref(&0).unwrap().pending_revenue_share,
        0
    );
    assert_eq!(escrow.get_order(0).unwrap(), &RevenueShareOrder::default());
}

// ---------------------------------------------------------------------------
// reserved_user_claims (OtterSec #48 and the #245/#53 bankruptcy-floor class)
// ---------------------------------------------------------------------------

/// The sweep may only pay out of the pnl pool's excess over live positive user
/// PnL. The reservation is priced with the oracle, so the same pool and the same
/// row pay or do not pay depending only on the price. Without this the sweep
/// would spend tokens that back a third party's settlement.
#[test]
fn sweep_reserves_positive_net_user_pnl() {
    let builder_authority = Pubkey::new_unique();

    // One base unit is long against the AMM at a cost basis of 92 dollars.
    // At an oracle price of 100 the users hold 8 dollars of positive PnL.
    let mut market = PerpMarket {
        amm: AMM {
            base_asset_amount_with_amm: BASE_PRECISION_I128,
            ..AMM::default()
        },
        quote_asset_amount: -92 * QUOTE_PRECISION_I128,
        ..perp_market(10, 5)
    };
    create_anchor_account_info!(market, PerpMarket, market_info);
    let perp_market_map = PerpMarketMap::load_one(&market_info, true).unwrap();

    let mut spot_market = quote_spot_market();
    create_anchor_account_info!(spot_market, SpotMarket, spot_market_info);
    let spot_market_map = SpotMarketMap::load_one(&spot_market_info, true).unwrap();

    let mut builder_user = beneficiary(builder_authority, false);
    create_anchor_account_info!(builder_user, User, builder_user_info);
    let mut builder_rev_share = revenue_share(builder_authority);
    create_anchor_account_info!(builder_rev_share, RevenueShare, builder_rev_share_info);
    let accounts = vec![builder_user_info, builder_rev_share_info];
    let mut account_iter = accounts.iter().peekable();
    let revenue_share_map = load_revenue_share_map(&mut account_iter).unwrap();

    escrow!(
        &[completed_builder_row(0, 5)],
        &[builder_info(builder_authority)],
        Pubkey::default(),
        escrow
    );

    // control: the reservation really is 8 dollars at this price, so a 5 dollar
    // fee against a 10 dollar pool leaves only 2 dollars available.
    {
        let market = perp_market_map.get_ref(&0).unwrap();
        assert_eq!(
            calculate_net_user_pnl(
                &market.amm,
                100 * PRICE_PRECISION_I64,
                market.quote_asset_amount,
                market.net_unsettled_funding_pnl,
            )
            .unwrap(),
            8 * QUOTE_PRECISION_I128
        );
    }

    sweep_completed_revenue_share_for_market(
        0,
        &mut escrow,
        &perp_market_map,
        &spot_market_map,
        &revenue_share_map,
        0,
        100 * PRICE_PRECISION_I64,
        true,
        false,
    )
    .unwrap();

    // nothing moved and the row still owes its fee
    assert_eq!(
        quote_balance(&revenue_share_map, &builder_authority),
        BENEFICIARY_START
    );
    assert_eq!(
        pnl_pool_tokens(&perp_market_map, &spot_market_map),
        tokens(10)
    );
    assert_eq!(
        perp_market_map.get_ref(&0).unwrap().pending_revenue_share,
        5 * DOLLAR
    );
    assert_eq!(escrow.get_order(0).unwrap().fees_accrued, 5 * DOLLAR);

    // control: at a price of 92 the users hold no positive PnL, nothing is
    // reserved, and the identical row pays. This proves the block above came
    // from the reservation and not from some other guard.
    sweep_completed_revenue_share_for_market(
        0,
        &mut escrow,
        &perp_market_map,
        &spot_market_map,
        &revenue_share_map,
        0,
        92 * PRICE_PRECISION_I64,
        true,
        false,
    )
    .unwrap();

    assert_eq!(
        quote_balance(&revenue_share_map, &builder_authority),
        BENEFICIARY_START + 5 * DOLLAR_BALANCE
    );
    assert_eq!(
        pnl_pool_tokens(&perp_market_map, &spot_market_map),
        tokens(5)
    );
    assert_eq!(
        perp_market_map.get_ref(&0).unwrap().pending_revenue_share,
        0
    );
}

/// The sweep must also leave the floored insurance-fund bankruptcy tranche
/// backed. `resolve_perp_bankruptcy` cancels a forgiven loss against
/// `pending_if_fee` without moving tokens, so it needs that value to still sit
/// in the pnl pool.
#[test]
fn sweep_reserves_the_bankruptcy_if_tranche() {
    let builder_authority = Pubkey::new_unique();

    // Open interest is 5 base at a TWAP of 100 dollars, so the notional is 500
    // dollars. A floor of 1.2 percent is 6 dollars, and `pending_if_fee` is 6
    // dollars, so 6 dollars are reserved out of a 10 dollar pool.
    let mut market = PerpMarket {
        base_asset_amount_long: 5 * BASE_PRECISION_I128,
        base_asset_amount_short: -5 * BASE_PRECISION_I128,
        bankruptcy_if_floor_pct: PERCENTAGE_PRECISION_U32 * 12 / 1000,
        market_stats: MarketStats {
            historical_oracle_data: HistoricalOracleData {
                last_oracle_price_twap: 100 * PRICE_PRECISION_I64,
                ..HistoricalOracleData::default()
            },
            ..MarketStats::default()
        },
        fee_ledger: FeeLedger {
            pending_if_fee: tokens(6),
            ..FeeLedger::default()
        },
        ..perp_market(10, 5)
    };
    create_anchor_account_info!(market, PerpMarket, market_info);
    let perp_market_map = PerpMarketMap::load_one(&market_info, true).unwrap();

    let mut spot_market = quote_spot_market();
    create_anchor_account_info!(spot_market, SpotMarket, spot_market_info);
    let spot_market_map = SpotMarketMap::load_one(&spot_market_info, true).unwrap();

    let mut builder_user = beneficiary(builder_authority, false);
    create_anchor_account_info!(builder_user, User, builder_user_info);
    let mut builder_rev_share = revenue_share(builder_authority);
    create_anchor_account_info!(builder_rev_share, RevenueShare, builder_rev_share_info);
    let accounts = vec![builder_user_info, builder_rev_share_info];
    let mut account_iter = accounts.iter().peekable();
    let revenue_share_map = load_revenue_share_map(&mut account_iter).unwrap();

    escrow!(
        &[completed_builder_row(0, 5)],
        &[builder_info(builder_authority)],
        Pubkey::default(),
        escrow
    );

    // control: the reservation is 6 dollars, and net user PnL is zero, so the
    // tranche is the only thing withholding tokens.
    {
        let market = perp_market_map.get_ref(&0).unwrap();
        assert_eq!(
            market.get_bankruptcy_if_tranche_reservation(false).unwrap(),
            tokens(6)
        );
        assert_eq!(
            calculate_net_user_pnl(
                &market.amm,
                100 * PRICE_PRECISION_I64,
                market.quote_asset_amount,
                market.net_unsettled_funding_pnl,
            )
            .unwrap(),
            0
        );
    }

    sweep_completed_revenue_share_for_market(
        0,
        &mut escrow,
        &perp_market_map,
        &spot_market_map,
        &revenue_share_map,
        0,
        100 * PRICE_PRECISION_I64,
        true,
        false,
    )
    .unwrap();

    // only 4 dollars were available, so the 5 dollar fee did not pay
    assert_eq!(
        quote_balance(&revenue_share_map, &builder_authority),
        BENEFICIARY_START
    );
    assert_eq!(
        pnl_pool_tokens(&perp_market_map, &spot_market_map),
        tokens(10)
    );
    assert_eq!(escrow.get_order(0).unwrap().fees_accrued, 5 * DOLLAR);

    // control: with the floor disabled the reservation goes to zero and the same
    // row pays out of the same pool.
    perp_market_map
        .get_ref_mut(&0)
        .unwrap()
        .bankruptcy_if_floor_pct = 0;
    sweep_completed_revenue_share_for_market(
        0,
        &mut escrow,
        &perp_market_map,
        &spot_market_map,
        &revenue_share_map,
        0,
        100 * PRICE_PRECISION_I64,
        true,
        false,
    )
    .unwrap();

    assert_eq!(
        quote_balance(&revenue_share_map, &builder_authority),
        BENEFICIARY_START + 5 * DOLLAR_BALANCE
    );
    assert_eq!(
        pnl_pool_tokens(&perp_market_map, &spot_market_map),
        tokens(5)
    );
}

/// The reservation is computed once before the loop and the pnl pool is read
/// again for every row. Three rows drain the pool down to the reservation and
/// then stop. If the pool amount were ever hoisted out of the loop the third row
/// would also pay and the pool would fall below the reserved floor.
#[test]
fn reservation_is_a_floor_across_every_row() {
    let builder_authority = Pubkey::new_unique();

    // Pool 10 dollars, reservation 4 dollars of positive user PnL.
    let mut market = PerpMarket {
        quote_asset_amount: 4 * QUOTE_PRECISION_I128,
        ..perp_market(10, 7)
    };
    create_anchor_account_info!(market, PerpMarket, market_info);
    let perp_market_map = PerpMarketMap::load_one(&market_info, true).unwrap();

    let mut spot_market = quote_spot_market();
    create_anchor_account_info!(spot_market, SpotMarket, spot_market_info);
    let spot_market_map = SpotMarketMap::load_one(&spot_market_info, true).unwrap();

    let mut builder_user = beneficiary(builder_authority, false);
    create_anchor_account_info!(builder_user, User, builder_user_info);
    let mut builder_rev_share = revenue_share(builder_authority);
    create_anchor_account_info!(builder_rev_share, RevenueShare, builder_rev_share_info);
    let accounts = vec![builder_user_info, builder_rev_share_info];
    let mut account_iter = accounts.iter().peekable();
    let revenue_share_map = load_revenue_share_map(&mut account_iter).unwrap();

    let rows = [
        completed_builder_row(0, 3),
        completed_builder_row(0, 3),
        completed_builder_row(0, 1),
    ];
    escrow!(
        &rows,
        &[builder_info(builder_authority)],
        Pubkey::default(),
        escrow
    );

    // control: the reservation is 4 dollars for every row in this call.
    {
        let market = perp_market_map.get_ref(&0).unwrap();
        assert_eq!(
            calculate_net_user_pnl(
                &market.amm,
                100 * PRICE_PRECISION_I64,
                market.quote_asset_amount,
                market.net_unsettled_funding_pnl,
            )
            .unwrap(),
            4 * QUOTE_PRECISION_I128
        );
    }

    sweep_completed_revenue_share_for_market(
        0,
        &mut escrow,
        &perp_market_map,
        &spot_market_map,
        &revenue_share_map,
        0,
        100 * PRICE_PRECISION_I64,
        true,
        false,
    )
    .unwrap();

    // the first two rows paid 6 dollars in total
    assert_eq!(
        quote_balance(&revenue_share_map, &builder_authority),
        BENEFICIARY_START + 6 * DOLLAR_BALANCE
    );
    assert_eq!(
        perp_market_map.get_ref(&0).unwrap().pending_revenue_share,
        DOLLAR
    );
    // the pool holds exactly the reserved amount and no less
    assert_eq!(
        pnl_pool_tokens(&perp_market_map, &spot_market_map),
        tokens(4)
    );
    assert_eq!(escrow.get_order(0).unwrap(), &RevenueShareOrder::default());
    assert_eq!(escrow.get_order(1).unwrap(), &RevenueShareOrder::default());
    // the third row is untouched because the pool was re-read after each payout
    assert_eq!(escrow.get_order(2).unwrap(), &rows[2]);
}

// ---------------------------------------------------------------------------
// pause
// ---------------------------------------------------------------------------

/// The `SettleRevPool` pause stops this sweep. It is the same pnl-pool conduit
/// the protocol fee sweep drains, so a paused market must not have its pool
/// routed to builders while the direct fee sweep is halted.
#[test]
fn settle_rev_pool_pause_stops_the_sweep() {
    let builder_authority = Pubkey::new_unique();

    let mut market = PerpMarket {
        paused_operations: PerpOperation::SettleRevPool as u8,
        ..perp_market(100, 5)
    };
    create_anchor_account_info!(market, PerpMarket, market_info);
    let perp_market_map = PerpMarketMap::load_one(&market_info, true).unwrap();

    let mut spot_market = quote_spot_market();
    create_anchor_account_info!(spot_market, SpotMarket, spot_market_info);
    let spot_market_map = SpotMarketMap::load_one(&spot_market_info, true).unwrap();

    let mut builder_user = beneficiary(builder_authority, false);
    create_anchor_account_info!(builder_user, User, builder_user_info);
    let mut builder_rev_share = revenue_share(builder_authority);
    create_anchor_account_info!(builder_rev_share, RevenueShare, builder_rev_share_info);
    let accounts = vec![builder_user_info, builder_rev_share_info];
    let mut account_iter = accounts.iter().peekable();
    let revenue_share_map = load_revenue_share_map(&mut account_iter).unwrap();

    let row = completed_builder_row(0, 5);
    escrow!(
        &[row],
        &[builder_info(builder_authority)],
        Pubkey::default(),
        escrow
    );

    sweep_completed_revenue_share_for_market(
        0,
        &mut escrow,
        &perp_market_map,
        &spot_market_map,
        &revenue_share_map,
        0,
        100 * PRICE_PRECISION_I64,
        true,
        false,
    )
    .unwrap();

    // nothing changed at all
    assert_eq!(
        quote_balance(&revenue_share_map, &builder_authority),
        BENEFICIARY_START
    );
    assert_eq!(
        pnl_pool_tokens(&perp_market_map, &spot_market_map),
        tokens(100)
    );
    assert_eq!(
        perp_market_map.get_ref(&0).unwrap().pending_revenue_share,
        5 * DOLLAR
    );
    assert_eq!(escrow.get_order(0).unwrap(), &row);

    // control: the pause was the only thing stopping the payout.
    perp_market_map.get_ref_mut(&0).unwrap().paused_operations = 0;
    sweep_completed_revenue_share_for_market(
        0,
        &mut escrow,
        &perp_market_map,
        &spot_market_map,
        &revenue_share_map,
        0,
        100 * PRICE_PRECISION_I64,
        true,
        false,
    )
    .unwrap();

    assert_eq!(
        quote_balance(&revenue_share_map, &builder_authority),
        BENEFICIARY_START + 5 * DOLLAR_BALANCE
    );
    assert_eq!(
        pnl_pool_tokens(&perp_market_map, &spot_market_map),
        tokens(95)
    );
}

// ---------------------------------------------------------------------------
// beneficiary resolution
// ---------------------------------------------------------------------------

/// The builder feature flag gates only the builder branch. Referral rows still
/// pay. A disabled flag must not clear a builder row or discharge its claim,
/// because the fee is still owed once the flag is on again.
#[test]
fn disabled_builder_codes_leaves_builder_rows_but_pays_referrals() {
    let builder_authority = Pubkey::new_unique();
    let referrer_authority = Pubkey::new_unique();

    let mut market = perp_market(100, 5);
    create_anchor_account_info!(market, PerpMarket, market_info);
    let perp_market_map = PerpMarketMap::load_one(&market_info, true).unwrap();

    let mut spot_market = quote_spot_market();
    create_anchor_account_info!(spot_market, SpotMarket, spot_market_info);
    let spot_market_map = SpotMarketMap::load_one(&spot_market_info, true).unwrap();

    let mut builder_user = beneficiary(builder_authority, false);
    create_anchor_account_info!(builder_user, User, builder_user_info);
    let mut builder_rev_share = revenue_share(builder_authority);
    create_anchor_account_info!(builder_rev_share, RevenueShare, builder_rev_share_info);
    let mut referrer_user = beneficiary(referrer_authority, false);
    create_anchor_account_info!(referrer_user, User, referrer_user_info);
    let mut referrer_rev_share = revenue_share(referrer_authority);
    create_anchor_account_info!(referrer_rev_share, RevenueShare, referrer_rev_share_info);
    let accounts = vec![
        builder_user_info,
        builder_rev_share_info,
        referrer_user_info,
        referrer_rev_share_info,
    ];
    let mut account_iter = accounts.iter().peekable();
    let revenue_share_map = load_revenue_share_map(&mut account_iter).unwrap();

    let builder_row = completed_builder_row(0, 3);
    escrow!(
        &[builder_row, referral_row(0, 2)],
        &[builder_info(builder_authority)],
        referrer_authority,
        escrow
    );

    sweep_completed_revenue_share_for_market(
        0,
        &mut escrow,
        &perp_market_map,
        &spot_market_map,
        &revenue_share_map,
        0,
        100 * PRICE_PRECISION_I64,
        false,
        false,
    )
    .unwrap();

    // the builder row is intact and unpaid
    assert_eq!(
        quote_balance(&revenue_share_map, &builder_authority),
        BENEFICIARY_START
    );
    assert_eq!(escrow.get_order(0).unwrap(), &builder_row);
    // the referral row paid, which proves the loop reached both rows
    assert_eq!(
        quote_balance(&revenue_share_map, &referrer_authority),
        BENEFICIARY_START + 2 * DOLLAR_BALANCE
    );
    assert_eq!(
        pnl_pool_tokens(&perp_market_map, &spot_market_map),
        tokens(98)
    );
    assert_eq!(
        perp_market_map.get_ref(&0).unwrap().pending_revenue_share,
        3 * DOLLAR
    );
}

/// A caller who leaves out a beneficiary account cannot make the claim vanish.
/// The sweep is permissionless, so the row and the liability counter must both
/// survive a missing `User` or a missing `RevenueShare`.
#[test]
fn missing_beneficiary_accounts_leave_the_claim_intact() {
    let builder_authority = Pubkey::new_unique();

    let mut market = perp_market(100, 5);
    create_anchor_account_info!(market, PerpMarket, market_info);
    let perp_market_map = PerpMarketMap::load_one(&market_info, true).unwrap();

    let mut spot_market = quote_spot_market();
    create_anchor_account_info!(spot_market, SpotMarket, spot_market_info);
    let spot_market_map = SpotMarketMap::load_one(&spot_market_info, true).unwrap();

    // the builder's User is supplied but its RevenueShare is not
    let mut builder_user = beneficiary(builder_authority, false);
    create_anchor_account_info!(builder_user, User, builder_user_info);
    let accounts = vec![builder_user_info];
    let mut account_iter = accounts.iter().peekable();
    let revenue_share_map = load_revenue_share_map(&mut account_iter).unwrap();
    assert!(revenue_share_map
        .get_revenue_share_account_mut(&builder_authority)
        .is_err());

    let row = completed_builder_row(0, 5);
    escrow!(
        &[row],
        &[builder_info(builder_authority)],
        Pubkey::default(),
        escrow
    );

    sweep_completed_revenue_share_for_market(
        0,
        &mut escrow,
        &perp_market_map,
        &spot_market_map,
        &revenue_share_map,
        0,
        100 * PRICE_PRECISION_I64,
        true,
        false,
    )
    .unwrap();

    assert_eq!(
        quote_balance(&revenue_share_map, &builder_authority),
        BENEFICIARY_START
    );
    assert_eq!(
        pnl_pool_tokens(&perp_market_map, &spot_market_map),
        tokens(100)
    );
    assert_eq!(
        perp_market_map.get_ref(&0).unwrap().pending_revenue_share,
        5 * DOLLAR
    );
    assert_eq!(escrow.get_order(0).unwrap(), &row);
}

/// A referral row with no referrer on the escrow pays nobody and keeps its fees.
/// The row would otherwise route tokens with no recipient recorded.
#[test]
fn referral_row_without_a_referrer_pays_nobody() {
    let mut market = perp_market(100, 4);
    create_anchor_account_info!(market, PerpMarket, market_info);
    let perp_market_map = PerpMarketMap::load_one(&market_info, true).unwrap();

    let mut spot_market = quote_spot_market();
    create_anchor_account_info!(spot_market, SpotMarket, spot_market_info);
    let spot_market_map = SpotMarketMap::load_one(&spot_market_info, true).unwrap();

    let revenue_share_map = RevenueShareMap::empty();

    let row = referral_row(0, 4);
    escrow!(&[row], &[], Pubkey::default(), escrow);
    assert!(escrow.get_referrer().is_none());

    sweep_completed_revenue_share_for_market(
        0,
        &mut escrow,
        &perp_market_map,
        &spot_market_map,
        &revenue_share_map,
        0,
        100 * PRICE_PRECISION_I64,
        true,
        false,
    )
    .unwrap();

    assert_eq!(
        pnl_pool_tokens(&perp_market_map, &spot_market_map),
        tokens(100)
    );
    assert_eq!(
        perp_market_map.get_ref(&0).unwrap().pending_revenue_share,
        4 * DOLLAR
    );
    assert_eq!(escrow.get_order(0).unwrap(), &row);
}

/// The escrow records only an authority, so the loader pins the recipient to
/// subaccount 0. Without this a permissionless caller could pass any sibling
/// subaccount of that authority and redirect the rewards.
#[test]
fn revenue_share_map_only_accepts_subaccount_zero() {
    let authority = Pubkey::new_unique();

    let mut sibling = beneficiary(authority, false);
    sibling.sub_account_id = 1;
    create_anchor_account_info!(sibling, User, sibling_info);
    let accounts = vec![sibling_info];
    let mut account_iter = accounts.iter().peekable();
    assert_eq!(
        load_revenue_share_map(&mut account_iter).err(),
        Some(ErrorCode::InvalidRevenueShareRecipient)
    );

    // control: the same account at subaccount 0 loads and resolves.
    let mut canonical = beneficiary(authority, false);
    create_anchor_account_info!(canonical, User, canonical_info);
    let accounts = vec![canonical_info];
    let mut account_iter = accounts.iter().peekable();
    let revenue_share_map = load_revenue_share_map(&mut account_iter).unwrap();
    assert_eq!(
        revenue_share_map
            .get_user_ref_mut(&authority)
            .unwrap()
            .sub_account_id,
        0
    );
}

// ---------------------------------------------------------------------------
// head-of-line blocking
// ---------------------------------------------------------------------------

/// The sweep skips a row that the pool cannot pay. It does not stop. The loop reads the pool again
/// for each row, and the reserve is constant for the call, so a smaller later row still pays. Row
/// order does not change between calls, so a stop would block the later rows forever.
#[test]
fn an_unaffordable_row_does_not_block_later_rows() {
    let builder_authority = Pubkey::new_unique();

    // Pool 5 dollars, no reservation. The first row wants 10 and can never be paid from it.
    let mut market = perp_market(5, 13);
    create_anchor_account_info!(market, PerpMarket, market_info);
    let perp_market_map = PerpMarketMap::load_one(&market_info, true).unwrap();

    let mut spot_market = quote_spot_market();
    create_anchor_account_info!(spot_market, SpotMarket, spot_market_info);
    let spot_market_map = SpotMarketMap::load_one(&spot_market_info, true).unwrap();

    let mut builder_user = beneficiary(builder_authority, false);
    create_anchor_account_info!(builder_user, User, builder_user_info);
    let mut builder_rev_share = revenue_share(builder_authority);
    create_anchor_account_info!(builder_rev_share, RevenueShare, builder_rev_share_info);
    let accounts = vec![builder_user_info, builder_rev_share_info];
    let mut account_iter = accounts.iter().peekable();
    let revenue_share_map = load_revenue_share_map(&mut account_iter).unwrap();

    let rows = [completed_builder_row(0, 10), completed_builder_row(0, 3)];
    escrow!(
        &rows,
        &[builder_info(builder_authority)],
        Pubkey::default(),
        escrow
    );

    let discharged = sweep_completed_revenue_share_for_market(
        0,
        &mut escrow,
        &perp_market_map,
        &spot_market_map,
        &revenue_share_map,
        0,
        100 * PRICE_PRECISION_I64,
        true,
        false,
    )
    .unwrap();

    // the second row paid despite the first being unaffordable
    assert_eq!(discharged, 3 * DOLLAR);
    assert_eq!(
        quote_balance(&revenue_share_map, &builder_authority),
        BENEFICIARY_START + 3 * DOLLAR_BALANCE
    );
    assert_eq!(
        pnl_pool_tokens(&perp_market_map, &spot_market_map),
        tokens(2)
    );
    // exactly the paid row's fee left the counter; the skipped row is still owed
    assert_eq!(
        perp_market_map.get_ref(&0).unwrap().pending_revenue_share,
        10 * DOLLAR
    );
    assert_eq!(escrow.get_order(0).unwrap(), &rows[0]);
    assert_eq!(escrow.get_order(1).unwrap(), &RevenueShareOrder::default());
}

/// The referral branch skips in the same way. It keeps the row and clears only the fee.
#[test]
fn an_unaffordable_row_does_not_block_a_later_referral_row() {
    let referrer_authority = Pubkey::new_unique();
    let builder_authority = Pubkey::new_unique();

    let mut market = perp_market(5, 13);
    create_anchor_account_info!(market, PerpMarket, market_info);
    let perp_market_map = PerpMarketMap::load_one(&market_info, true).unwrap();

    let mut spot_market = quote_spot_market();
    create_anchor_account_info!(spot_market, SpotMarket, spot_market_info);
    let spot_market_map = SpotMarketMap::load_one(&spot_market_info, true).unwrap();

    let mut referrer_user = beneficiary(referrer_authority, false);
    create_anchor_account_info!(referrer_user, User, referrer_user_info);
    let mut referrer_rev_share = revenue_share(referrer_authority);
    create_anchor_account_info!(referrer_rev_share, RevenueShare, referrer_rev_share_info);
    let accounts = vec![referrer_user_info, referrer_rev_share_info];
    let mut account_iter = accounts.iter().peekable();
    let revenue_share_map = load_revenue_share_map(&mut account_iter).unwrap();

    let rows = [completed_builder_row(0, 10), referral_row(0, 3)];
    escrow!(
        &rows,
        &[builder_info(builder_authority)],
        referrer_authority,
        escrow
    );

    let discharged = sweep_completed_revenue_share_for_market(
        0,
        &mut escrow,
        &perp_market_map,
        &spot_market_map,
        &revenue_share_map,
        0,
        100 * PRICE_PRECISION_I64,
        true,
        false,
    )
    .unwrap();

    assert_eq!(discharged, 3 * DOLLAR);
    assert_eq!(
        quote_balance(&revenue_share_map, &referrer_authority),
        BENEFICIARY_START + 3 * DOLLAR_BALANCE
    );
    assert_eq!(
        perp_market_map.get_ref(&0).unwrap().pending_revenue_share,
        10 * DOLLAR
    );
    assert_eq!(escrow.get_order(0).unwrap(), &rows[0]);
    // a referral row keeps its slot and only loses its fee
    assert_eq!(escrow.get_order(1).unwrap().fees_accrued, 0);
    assert!(escrow.get_order(1).unwrap().is_referral_order());
}

/// The loop holds no stopped state. A skip in the middle leaves every later row payable. Every
/// payment still keeps the reserve.
#[test]
fn every_row_is_reconsidered_after_a_skip() {
    let builder_authority = Pubkey::new_unique();

    // Pool 10 dollars, 4 dollars of positive user PnL reserved, so 6 are available.
    let mut market = PerpMarket {
        quote_asset_amount: 4 * QUOTE_PRECISION_I128,
        ..perp_market(10, 22)
    };
    create_anchor_account_info!(market, PerpMarket, market_info);
    let perp_market_map = PerpMarketMap::load_one(&market_info, true).unwrap();

    let mut spot_market = quote_spot_market();
    create_anchor_account_info!(spot_market, SpotMarket, spot_market_info);
    let spot_market_map = SpotMarketMap::load_one(&spot_market_info, true).unwrap();

    let mut builder_user = beneficiary(builder_authority, false);
    create_anchor_account_info!(builder_user, User, builder_user_info);
    let mut builder_rev_share = revenue_share(builder_authority);
    create_anchor_account_info!(builder_rev_share, RevenueShare, builder_rev_share_info);
    let accounts = vec![builder_user_info, builder_rev_share_info];
    let mut account_iter = accounts.iter().peekable();
    let revenue_share_map = load_revenue_share_map(&mut account_iter).unwrap();

    let rows = [
        completed_builder_row(0, 9),
        completed_builder_row(0, 2),
        completed_builder_row(0, 11),
    ];
    escrow!(
        &rows,
        &[builder_info(builder_authority)],
        Pubkey::default(),
        escrow
    );

    let discharged = sweep_completed_revenue_share_for_market(
        0,
        &mut escrow,
        &perp_market_map,
        &spot_market_map,
        &revenue_share_map,
        0,
        100 * PRICE_PRECISION_I64,
        true,
        false,
    )
    .unwrap();

    // only the middle row was affordable
    assert_eq!(discharged, 2 * DOLLAR);
    assert_eq!(
        quote_balance(&revenue_share_map, &builder_authority),
        BENEFICIARY_START + 2 * DOLLAR_BALANCE
    );
    assert_eq!(escrow.get_order(0).unwrap(), &rows[0]);
    assert_eq!(escrow.get_order(1).unwrap(), &RevenueShareOrder::default());
    assert_eq!(escrow.get_order(2).unwrap(), &rows[2]);
    // and the pool never fell below the reserved floor
    assert_eq!(
        pnl_pool_tokens(&perp_market_map, &spot_market_map),
        tokens(8)
    );
    assert!(pnl_pool_tokens(&perp_market_map, &spot_market_map) >= tokens(4));
}

// ---------------------------------------------------------------------------
// settlement pricing
// ---------------------------------------------------------------------------

/// In Settlement, expired positions settle at `expiry_price`, not at the live price. The reserve
/// must use the same price. A live price below `expiry_price` on a net-long market makes the
/// reserve too small. The sweep then pays value that the expiry claims need, and those claims
/// later fail with `InsufficientPerpPnlPool`.
#[test]
fn settlement_status_values_the_reserve_at_expiry_price() {
    let builder_authority = Pubkey::new_unique();

    // One base long against the AMM at a cost basis of 92 dollars. At the live oracle of 92 the
    // users hold nothing; at the expiry price of 100 they hold 8 dollars.
    let mut market = PerpMarket {
        status: MarketStatus::Settlement,
        expiry_price: 100 * PRICE_PRECISION_I64,
        amm: AMM {
            base_asset_amount_with_amm: BASE_PRECISION_I128,
            ..AMM::default()
        },
        quote_asset_amount: -92 * QUOTE_PRECISION_I128,
        ..perp_market(10, 5)
    };
    create_anchor_account_info!(market, PerpMarket, market_info);
    let perp_market_map = PerpMarketMap::load_one(&market_info, true).unwrap();

    let mut spot_market = quote_spot_market();
    create_anchor_account_info!(spot_market, SpotMarket, spot_market_info);
    let spot_market_map = SpotMarketMap::load_one(&spot_market_info, true).unwrap();

    let mut builder_user = beneficiary(builder_authority, false);
    create_anchor_account_info!(builder_user, User, builder_user_info);
    let mut builder_rev_share = revenue_share(builder_authority);
    create_anchor_account_info!(builder_rev_share, RevenueShare, builder_rev_share_info);
    let accounts = vec![builder_user_info, builder_rev_share_info];
    let mut account_iter = accounts.iter().peekable();
    let revenue_share_map = load_revenue_share_map(&mut account_iter).unwrap();

    escrow!(
        &[completed_builder_row(0, 5)],
        &[builder_info(builder_authority)],
        Pubkey::default(),
        escrow
    );

    // control: at the live price nothing would be reserved, so the row would pay out of the
    // 10 dollar pool. Any payout below proves the live price was used.
    {
        let market = perp_market_map.get_ref(&0).unwrap();
        assert_eq!(
            calculate_net_user_pnl(
                &market.amm,
                92 * PRICE_PRECISION_I64,
                market.quote_asset_amount,
                market.net_unsettled_funding_pnl,
            )
            .unwrap(),
            0
        );
        assert_eq!(
            calculate_net_user_pnl(
                &market.amm,
                market.expiry_price,
                market.quote_asset_amount,
                market.net_unsettled_funding_pnl,
            )
            .unwrap(),
            8 * QUOTE_PRECISION_I128
        );
    }

    let discharged = sweep_completed_revenue_share_for_market(
        0,
        &mut escrow,
        &perp_market_map,
        &spot_market_map,
        &revenue_share_map,
        0,
        // a live oracle below the expiry price, which must be ignored
        92 * PRICE_PRECISION_I64,
        true,
        false,
    )
    .unwrap();

    // 8 of the 10 dollars are reserved for the expiring longs, so the 5 dollar fee cannot be paid
    assert_eq!(discharged, 0);
    assert_eq!(
        quote_balance(&revenue_share_map, &builder_authority),
        BENEFICIARY_START
    );
    assert_eq!(
        pnl_pool_tokens(&perp_market_map, &spot_market_map),
        tokens(10)
    );
    assert_eq!(
        perp_market_map.get_ref(&0).unwrap().pending_revenue_share,
        5 * DOLLAR
    );
    assert_eq!(escrow.get_order(0).unwrap().fees_accrued, 5 * DOLLAR);
}

// ---------------------------------------------------------------------------
// the payability the delist check depends on
// ---------------------------------------------------------------------------

/// The delist check depends on this: in Settlement the program can pay the whole outstanding
/// revenue share. The check therefore cannot stop a market from closing.
///
/// The expiry solver values winner claims against `pnl_pool - pending_revenue_share` (OtterSec
/// #147). At `expiry_price` the reserve equals the pool minus the amount owed, so the amount owed
/// stays available. This test uses a 10 dollar pool and 4 dollars owed, so net user pnl is 6.
#[test]
fn settlement_leaves_exactly_the_owed_amount_available() {
    let builder_authority = Pubkey::new_unique();

    // One base long against the AMM at a cost basis of 92. net_user_pnl(price) = price - 92. An
    // expiry price of 98 therefore values winner claims at 6, which is pool(10) - owed(4).
    let mut market = PerpMarket {
        status: MarketStatus::Settlement,
        expiry_price: 98 * PRICE_PRECISION_I64,
        amm: AMM {
            base_asset_amount_with_amm: BASE_PRECISION_I128,
            ..AMM::default()
        },
        quote_asset_amount: -92 * QUOTE_PRECISION_I128,
        ..perp_market(10, 4)
    };
    create_anchor_account_info!(market, PerpMarket, market_info);
    let perp_market_map = PerpMarketMap::load_one(&market_info, true).unwrap();

    let mut spot_market = quote_spot_market();
    create_anchor_account_info!(spot_market, SpotMarket, spot_market_info);
    let spot_market_map = SpotMarketMap::load_one(&spot_market_info, true).unwrap();

    let mut builder_user = beneficiary(builder_authority, false);
    create_anchor_account_info!(builder_user, User, builder_user_info);
    let mut builder_rev_share = revenue_share(builder_authority);
    create_anchor_account_info!(builder_rev_share, RevenueShare, builder_rev_share_info);
    let accounts = vec![builder_user_info, builder_rev_share_info];
    let mut account_iter = accounts.iter().peekable();
    let revenue_share_map = load_revenue_share_map(&mut account_iter).unwrap();

    // The 4 dollars owed sit in two rows. The whole liability must clear, not one row.
    escrow!(
        &[completed_builder_row(0, 3), completed_builder_row(0, 1)],
        &[builder_info(builder_authority)],
        Pubkey::default(),
        escrow
    );

    // Control: the reserve of the solver equals the pool minus the amount owed.
    {
        let market = perp_market_map.get_ref(&0).unwrap();
        assert_eq!(
            calculate_net_user_pnl(
                &market.amm,
                market.expiry_price,
                market.quote_asset_amount,
                market.net_unsettled_funding_pnl,
            )
            .unwrap(),
            6 * QUOTE_PRECISION_I128
        );
    }

    let discharged = sweep_completed_revenue_share_for_market(
        0,
        &mut escrow,
        &perp_market_map,
        &spot_market_map,
        &revenue_share_map,
        0,
        // Settlement ignores the live price. This value proves that.
        1_000 * PRICE_PRECISION_I64,
        true,
        false,
    )
    .unwrap();

    // Every dollar owed is paid and the counter is clear. The delist check then passes.
    assert_eq!(discharged, 4 * DOLLAR);
    assert_eq!(
        perp_market_map.get_ref(&0).unwrap().pending_revenue_share,
        0
    );
    assert_eq!(
        quote_balance(&revenue_share_map, &builder_authority),
        BENEFICIARY_START + 4 * DOLLAR_BALANCE
    );
    // The pool holds exactly the winner claims and no less.
    assert_eq!(
        pnl_pool_tokens(&perp_market_map, &spot_market_map),
        tokens(6)
    );
}

/// The delist checks set `net_user_pnl` to zero. The whole pool is then available, and even a
/// large liability pays in full. The delist check is therefore safe and no market can fail it.
#[test]
fn a_wound_down_market_can_pay_its_whole_liability() {
    let builder_authority = Pubkey::new_unique();

    // A flat AMM and a zero cost basis give net_user_pnl == 0, as the delist checks require.
    let mut market = PerpMarket {
        status: MarketStatus::Settlement,
        expiry_price: 100 * PRICE_PRECISION_I64,
        ..perp_market(9, 9)
    };
    create_anchor_account_info!(market, PerpMarket, market_info);
    let perp_market_map = PerpMarketMap::load_one(&market_info, true).unwrap();

    let mut spot_market = quote_spot_market();
    create_anchor_account_info!(spot_market, SpotMarket, spot_market_info);
    let spot_market_map = SpotMarketMap::load_one(&spot_market_info, true).unwrap();

    let mut builder_user = beneficiary(builder_authority, false);
    create_anchor_account_info!(builder_user, User, builder_user_info);
    let mut builder_rev_share = revenue_share(builder_authority);
    create_anchor_account_info!(builder_rev_share, RevenueShare, builder_rev_share_info);
    let accounts = vec![builder_user_info, builder_rev_share_info];
    let mut account_iter = accounts.iter().peekable();
    let revenue_share_map = load_revenue_share_map(&mut account_iter).unwrap();

    escrow!(
        &[
            completed_builder_row(0, 5),
            completed_builder_row(0, 3),
            completed_builder_row(0, 1),
        ],
        &[builder_info(builder_authority)],
        Pubkey::default(),
        escrow
    );

    let discharged = sweep_completed_revenue_share_for_market(
        0,
        &mut escrow,
        &perp_market_map,
        &spot_market_map,
        &revenue_share_map,
        0,
        100 * PRICE_PRECISION_I64,
        true,
        false,
    )
    .unwrap();

    // The pool goes to the beneficiaries, not to the revenue pool at the delist.
    assert_eq!(discharged, 9 * DOLLAR);
    assert_eq!(
        perp_market_map.get_ref(&0).unwrap().pending_revenue_share,
        0
    );
    assert_eq!(pnl_pool_tokens(&perp_market_map, &spot_market_map), 0);
}

// ---------------------------------------------------------------------------
// forfeiting rows that provably cannot be paid
// ---------------------------------------------------------------------------

/// A builder row normally needs `Completed` before payment. Payment clears the row, and a live
/// order would lose the link that it needs to accrue. In Settlement no fill can happen, so an
/// `Open` row pays. The program can then clear the liability without the sub-accounts of the
/// escrow owner. An owner can delete a sub-account, and its rows could never reach `Completed`.
#[test]
fn settlement_pays_an_open_builder_row() {
    let builder_authority = Pubkey::new_unique();

    let mut market = PerpMarket {
        status: MarketStatus::Settlement,
        expiry_price: 100 * PRICE_PRECISION_I64,
        ..perp_market(10, 4)
    };
    create_anchor_account_info!(market, PerpMarket, market_info);
    let perp_market_map = PerpMarketMap::load_one(&market_info, true).unwrap();

    let mut spot_market = quote_spot_market();
    create_anchor_account_info!(spot_market, SpotMarket, spot_market_info);
    let spot_market_map = SpotMarketMap::load_one(&spot_market_info, true).unwrap();

    let mut builder_user = beneficiary(builder_authority, false);
    create_anchor_account_info!(builder_user, User, builder_user_info);
    let mut builder_rev_share = revenue_share(builder_authority);
    create_anchor_account_info!(builder_rev_share, RevenueShare, builder_rev_share_info);
    let accounts = vec![builder_user_info, builder_rev_share_info];
    let mut account_iter = accounts.iter().peekable();
    let revenue_share_map = load_revenue_share_map(&mut account_iter).unwrap();

    escrow!(
        &[open_builder_row(0, 4)],
        &[builder_info(builder_authority)],
        Pubkey::default(),
        escrow
    );

    let discharged = sweep_completed_revenue_share_for_market(
        0,
        &mut escrow,
        &perp_market_map,
        &spot_market_map,
        &revenue_share_map,
        0,
        100 * PRICE_PRECISION_I64,
        true,
        false,
    )
    .unwrap();

    assert_eq!(discharged, 4 * DOLLAR);
    assert_eq!(
        perp_market_map.get_ref(&0).unwrap().pending_revenue_share,
        0
    );
    assert_eq!(
        quote_balance(&revenue_share_map, &builder_authority),
        BENEFICIARY_START + 4 * DOLLAR_BALANCE
    );
}

/// The sweep does not pay an `Open` row on a live market. A fill can still accrue to it, and
/// payment would clear the `order_id` that the fill path matches. Only Settlement removes this
/// rule.
#[test]
fn a_live_market_still_requires_a_completed_builder_row() {
    let builder_authority = Pubkey::new_unique();

    let mut market = perp_market(10, 4);
    create_anchor_account_info!(market, PerpMarket, market_info);
    let perp_market_map = PerpMarketMap::load_one(&market_info, true).unwrap();

    let mut spot_market = quote_spot_market();
    create_anchor_account_info!(spot_market, SpotMarket, spot_market_info);
    let spot_market_map = SpotMarketMap::load_one(&spot_market_info, true).unwrap();

    let mut builder_user = beneficiary(builder_authority, false);
    create_anchor_account_info!(builder_user, User, builder_user_info);
    let mut builder_rev_share = revenue_share(builder_authority);
    create_anchor_account_info!(builder_rev_share, RevenueShare, builder_rev_share_info);
    let accounts = vec![builder_user_info, builder_rev_share_info];
    let mut account_iter = accounts.iter().peekable();
    let revenue_share_map = load_revenue_share_map(&mut account_iter).unwrap();

    let rows = [open_builder_row(0, 4)];
    escrow!(
        &rows,
        &[builder_info(builder_authority)],
        Pubkey::default(),
        escrow
    );

    let discharged = sweep_completed_revenue_share_for_market(
        0,
        &mut escrow,
        &perp_market_map,
        &spot_market_map,
        &revenue_share_map,
        0,
        100 * PRICE_PRECISION_I64,
        true,
        false,
    )
    .unwrap();

    assert_eq!(discharged, 0);
    assert_eq!(escrow.get_order(0).unwrap(), &rows[0]);
    assert_eq!(
        perp_market_map.get_ref(&0).unwrap().pending_revenue_share,
        4 * DOLLAR
    );
}

/// A forfeit removes the exact amount of the row from the counter and moves no tokens. The quote
/// stays in the pnl pool. It also clears the row before it changes the counter. A failure
/// therefore cannot leave a low counter and an unpaid row, which a later sweep would subtract a
/// second time.
#[test]
fn forfeit_discharges_the_row_without_moving_tokens() {
    let builder_authority = Pubkey::new_unique();

    let mut market = PerpMarket {
        status: MarketStatus::Settlement,
        ..perp_market(10, 7)
    };
    create_anchor_account_info!(market, PerpMarket, market_info);
    let perp_market_map = PerpMarketMap::load_one(&market_info, true).unwrap();

    let mut spot_market = quote_spot_market();
    create_anchor_account_info!(spot_market, SpotMarket, spot_market_info);
    let spot_market_map = SpotMarketMap::load_one(&spot_market_info, true).unwrap();

    escrow!(
        &[completed_builder_row(0, 4), referral_row(0, 3)],
        &[builder_info(builder_authority)],
        Pubkey::new_unique(),
        escrow
    );

    {
        let mut market = perp_market_map.get_ref_mut(&0).unwrap();
        let forfeited = super::forfeit_revenue_share_order(
            &mut market,
            &mut escrow,
            0,
            super::RevenueShareForfeitReason::NoBeneficiaryAccount,
        )
        .unwrap();
        assert_eq!(forfeited, 4 * DOLLAR);
        // counter falls by exactly the row's fee, not to zero
        assert_eq!(market.pending_revenue_share, 3 * DOLLAR);
    }

    // a builder row is fully cleared, freeing the slot
    assert_eq!(escrow.get_order(0).unwrap(), &RevenueShareOrder::default());

    {
        let mut market = perp_market_map.get_ref_mut(&0).unwrap();
        let forfeited = super::forfeit_revenue_share_order(
            &mut market,
            &mut escrow,
            1,
            super::RevenueShareForfeitReason::UnresolvableBeneficiary,
        )
        .unwrap();
        assert_eq!(forfeited, 3 * DOLLAR);
        // the counter now reaches zero, which is what unblocks delisting
        assert_eq!(market.pending_revenue_share, 0);
    }

    // a referral row keeps its slot and only loses the fee, matching how the sweep settles one
    assert_eq!(escrow.get_order(1).unwrap().fees_accrued, 0);
    assert!(escrow.get_order(1).unwrap().is_referral_order());

    // no tokens moved anywhere
    assert_eq!(
        pnl_pool_tokens(&perp_market_map, &spot_market_map),
        tokens(10)
    );
}

/// Two forfeits of the same row must subtract the amount once. The first call clears the row, so
/// the second finds nothing. Without this the counter would fall below the amount that the other
/// rows still owe, and the pool would reserve too little for them.
#[test]
fn forfeit_is_not_double_counted() {
    let builder_authority = Pubkey::new_unique();

    let mut market = PerpMarket {
        status: MarketStatus::Settlement,
        ..perp_market(10, 9)
    };
    create_anchor_account_info!(market, PerpMarket, market_info);
    let perp_market_map = PerpMarketMap::load_one(&market_info, true).unwrap();

    escrow!(
        &[completed_builder_row(0, 4), completed_builder_row(0, 5)],
        &[builder_info(builder_authority)],
        Pubkey::default(),
        escrow
    );

    let mut market = perp_market_map.get_ref_mut(&0).unwrap();
    super::forfeit_revenue_share_order(
        &mut market,
        &mut escrow,
        0,
        super::RevenueShareForfeitReason::NoBeneficiaryAccount,
    )
    .unwrap();
    let second = super::forfeit_revenue_share_order(
        &mut market,
        &mut escrow,
        0,
        super::RevenueShareForfeitReason::NoBeneficiaryAccount,
    )
    .unwrap();

    assert_eq!(second, 0);
    // the other row's claim is untouched and still reserved
    assert_eq!(market.pending_revenue_share, 5 * DOLLAR);
}

// ---------------------------------------------------------------------------
// forfeit proof
// ---------------------------------------------------------------------------

/// The address of the payout account of a beneficiary. `resolve_revenue_share_forfeit_reason`
/// derives the same address and accepts no other.
fn payout_user(authority: Pubkey) -> Pubkey {
    Pubkey::find_program_address(
        &[b"user", authority.as_ref(), 0_u16.to_le_bytes().as_ref()],
        &crate::ID,
    )
    .0
}

/// The `User` address of another sub-account of the same authority. The revenue-share map pays
/// only sub-account 0, so this address can never receive the row.
fn sibling_user(authority: Pubkey, sub_account_id: u16) -> Pubkey {
    Pubkey::find_program_address(
        &[
            b"user",
            authority.as_ref(),
            sub_account_id.to_le_bytes().as_ref(),
        ],
        &crate::ID,
    )
    .0
}

/// The escrow window of the state in these tests. The deadline for a missing payout account is
/// `expiry_ts` plus this value.
const ESCROW_PERIOD: i64 = 24 * 60 * 60;
/// The expiry of the settlement market in these tests.
const EXPIRY_TS: i64 = 1_700_000_000;
/// The first moment that a missing payout account is a missed deadline.
const FORFEIT_AFTER: i64 = EXPIRY_TS + ESCROW_PERIOD;

/// A market that is closed and wound down, with `pnl_pool_dollars` left in the pnl pool. The
/// default AMM holds no base and no quote, so the market owes its users nothing.
fn settlement_market(pnl_pool_dollars: u64, pending_revenue_share_dollars: u64) -> PerpMarket {
    PerpMarket {
        status: MarketStatus::Settlement,
        expiry_ts: EXPIRY_TS,
        ..perp_market(pnl_pool_dollars, pending_revenue_share_dollars)
    }
}

/// Runs the proof for order 0 of an escrow that holds one builder row.
fn resolve_builder_row(
    market: &PerpMarket,
    builder_authority: Pubkey,
    beneficiary_user: &Pubkey,
    beneficiary_user_is_empty: bool,
    now: i64,
) -> Result<super::RevenueShareForfeitReason, ErrorCode> {
    let spot_market = quote_spot_market();
    escrow!(
        &[completed_builder_row(0, 4)],
        &[builder_info(builder_authority)],
        Pubkey::default(),
        escrow
    );
    super::resolve_revenue_share_forfeit_reason(
        market,
        &spot_market,
        &mut escrow,
        0,
        0,
        beneficiary_user,
        beneficiary_user_is_empty,
        now,
        ESCROW_PERIOD,
    )
}

/// A live market has no forfeit. The row blocks no delist, and the beneficiary can still create
/// the account that they need.
#[test]
fn a_live_market_cannot_forfeit_a_row() {
    let builder_authority = Pubkey::new_unique();
    let market = PerpMarket {
        status: MarketStatus::Active,
        expiry_ts: EXPIRY_TS,
        ..perp_market(0, 4)
    };

    assert_eq!(
        resolve_builder_row(
            &market,
            builder_authority,
            &payout_user(builder_authority),
            true,
            FORFEIT_AFTER + 1,
        ),
        Err(ErrorCode::PerpMarketNotInSettlement)
    );
}

/// A missing payout account is proof only after the escrow window ends. The beneficiary can create
/// the account until then. The deadline is exclusive: the last second of the window still belongs
/// to the beneficiary.
#[test]
fn a_missing_payout_account_waits_for_the_escrow_window() {
    let builder_authority = Pubkey::new_unique();
    let market = settlement_market(0, 4);
    let payout = payout_user(builder_authority);

    // inside the window
    assert_eq!(
        resolve_builder_row(&market, builder_authority, &payout, true, EXPIRY_TS),
        Err(ErrorCode::RevenueShareOrderNotForfeitable)
    );
    // the deadline itself is still the beneficiary's
    assert_eq!(
        resolve_builder_row(&market, builder_authority, &payout, true, FORFEIT_AFTER),
        Err(ErrorCode::RevenueShareOrderNotForfeitable)
    );
    // one second later the account is late
    assert_eq!(
        resolve_builder_row(&market, builder_authority, &payout, true, FORFEIT_AFTER + 1),
        Ok(super::RevenueShareForfeitReason::NoBeneficiaryAccount)
    );
}

/// The proof derives the payout address of the row. A caller cannot pass some other empty account
/// and claim that the beneficiary has none. A sibling sub-account of the same beneficiary is also
/// refused, because only sub-account 0 can receive the row.
#[test]
fn an_empty_account_of_another_address_is_not_proof() {
    let builder_authority = Pubkey::new_unique();
    let market = settlement_market(0, 4);
    let now = FORFEIT_AFTER + 1;

    // an unrelated address
    assert_eq!(
        resolve_builder_row(&market, builder_authority, &Pubkey::new_unique(), true, now),
        Err(ErrorCode::InvalidRevenueShareRecipient)
    );
    // sub-account 1 of the same beneficiary
    assert_eq!(
        resolve_builder_row(
            &market,
            builder_authority,
            &sibling_user(builder_authority, 1),
            true,
            now,
        ),
        Err(ErrorCode::InvalidRevenueShareRecipient)
    );
    // the payout account of a different authority
    assert_eq!(
        resolve_builder_row(
            &market,
            builder_authority,
            &payout_user(Pubkey::new_unique()),
            true,
            now,
        ),
        Err(ErrorCode::InvalidRevenueShareRecipient)
    );
    // only the derived address passes
    assert_eq!(
        resolve_builder_row(
            &market,
            builder_authority,
            &payout_user(builder_authority),
            true,
            now,
        ),
        Ok(super::RevenueShareForfeitReason::NoBeneficiaryAccount)
    );
}

/// A row that the pool can pay is not forfeitable. The beneficiary has an account, the market is
/// wound down, and the pool holds the fee. `settle_revenue_share` must pay it.
#[test]
fn a_payable_row_is_not_forfeitable() {
    let builder_authority = Pubkey::new_unique();
    let payout = payout_user(builder_authority);
    let now = FORFEIT_AFTER + 1;

    // the pool holds more than the row
    assert_eq!(
        resolve_builder_row(
            &settlement_market(10, 4),
            builder_authority,
            &payout,
            false,
            now
        ),
        Err(ErrorCode::RevenueShareOrderNotForfeitable)
    );
    // the pool holds exactly the row
    assert_eq!(
        resolve_builder_row(
            &settlement_market(4, 4),
            builder_authority,
            &payout,
            false,
            now
        ),
        Err(ErrorCode::RevenueShareOrderNotForfeitable)
    );
    // one dollar short, so the pool can never pay it
    assert_eq!(
        resolve_builder_row(
            &settlement_market(3, 4),
            builder_authority,
            &payout,
            false,
            now
        ),
        Ok(super::RevenueShareForfeitReason::PoolExhausted)
    );
}

/// The pool is final only after the market winds down. While the AMM holds base, or while users
/// still hold quote, the pool can still grow, so a short pool is no proof.
#[test]
fn an_open_market_is_not_proof_of_an_exhausted_pool() {
    let builder_authority = Pubkey::new_unique();
    let payout = payout_user(builder_authority);
    let now = FORFEIT_AFTER + 1;

    let mut market = settlement_market(0, 4);
    market.amm.base_asset_amount_with_amm = BASE_PRECISION_I128;
    assert_eq!(
        resolve_builder_row(&market, builder_authority, &payout, false, now),
        Err(ErrorCode::RevenueShareOrderNotForfeitable)
    );

    let mut market = settlement_market(0, 4);
    market.quote_asset_amount = -QUOTE_PRECISION_I128;
    assert_eq!(
        resolve_builder_row(&market, builder_authority, &payout, false, now),
        Err(ErrorCode::RevenueShareOrderNotForfeitable)
    );
}

/// A row that names no beneficiary is unresolvable. Nobody can pay it, so it needs no deadline and
/// no pool check. The passed account is not read.
#[test]
fn a_row_that_names_nobody_is_unresolvable() {
    let market = settlement_market(10, 7);
    let spot_market = quote_spot_market();

    // a builder row whose index is past the end of the approved builders
    escrow!(
        &[completed_builder_row(0, 4)],
        &[],
        Pubkey::default(),
        no_builder
    );
    assert_eq!(
        super::resolve_revenue_share_forfeit_reason(
            &market,
            &spot_market,
            &mut no_builder,
            0,
            0,
            &Pubkey::new_unique(),
            false,
            EXPIRY_TS,
            ESCROW_PERIOD,
        ),
        Ok(super::RevenueShareForfeitReason::UnresolvableBeneficiary)
    );

    // a referral row in an escrow that has no referrer
    escrow!(&[referral_row(0, 3)], &[], Pubkey::default(), no_referrer);
    assert_eq!(
        super::resolve_revenue_share_forfeit_reason(
            &market,
            &spot_market,
            &mut no_referrer,
            0,
            0,
            &Pubkey::new_unique(),
            false,
            EXPIRY_TS,
            ESCROW_PERIOD,
        ),
        Ok(super::RevenueShareForfeitReason::UnresolvableBeneficiary)
    );
}

/// A referral row credits the referrer of the escrow, not a builder. The proof derives the payout
/// address of that referrer.
#[test]
fn a_referral_row_credits_the_referrer() {
    let referrer = Pubkey::new_unique();
    let builder_authority = Pubkey::new_unique();
    let market = settlement_market(0, 3);
    let spot_market = quote_spot_market();

    escrow!(
        &[referral_row(0, 3)],
        &[builder_info(builder_authority)],
        referrer,
        escrow
    );

    // the builder of the escrow is not the beneficiary of a referral row
    assert_eq!(
        super::resolve_revenue_share_forfeit_reason(
            &market,
            &spot_market,
            &mut escrow,
            0,
            0,
            &payout_user(builder_authority),
            true,
            FORFEIT_AFTER + 1,
            ESCROW_PERIOD,
        ),
        Err(ErrorCode::InvalidRevenueShareRecipient)
    );
    assert_eq!(
        super::resolve_revenue_share_forfeit_reason(
            &market,
            &spot_market,
            &mut escrow,
            0,
            0,
            &payout_user(referrer),
            true,
            FORFEIT_AFTER + 1,
            ESCROW_PERIOD,
        ),
        Ok(super::RevenueShareForfeitReason::NoBeneficiaryAccount)
    );
}

/// The proof reads the row that the caller names. A row of another market, or a row that owes
/// nothing, is refused.
#[test]
fn the_row_must_belong_to_the_market_and_owe_something() {
    let builder_authority = Pubkey::new_unique();
    let market = settlement_market(0, 4);
    let spot_market = quote_spot_market();

    escrow!(
        &[completed_builder_row(1, 4), completed_builder_row(0, 0)],
        &[builder_info(builder_authority)],
        Pubkey::default(),
        escrow
    );

    // order 0 belongs to market 1
    assert_eq!(
        super::resolve_revenue_share_forfeit_reason(
            &market,
            &spot_market,
            &mut escrow,
            0,
            0,
            &payout_user(builder_authority),
            true,
            FORFEIT_AFTER + 1,
            ESCROW_PERIOD,
        ),
        Err(ErrorCode::RevenueShareOrderMarketMismatch)
    );
    // order 1 owes nothing, so there is nothing to write off
    assert_eq!(
        super::resolve_revenue_share_forfeit_reason(
            &market,
            &spot_market,
            &mut escrow,
            0,
            1,
            &payout_user(builder_authority),
            true,
            FORFEIT_AFTER + 1,
            ESCROW_PERIOD,
        ),
        Err(ErrorCode::RevenueShareOrderHasNoFeesAccrued)
    );
}
