//! Moving a deposit and a borrow between two lending pools.
//!
//! A pool transfer touches four spot markets at once: the deposit leaves one
//! pool and enters the other, and the borrow does the same in reverse. Each
//! pair shares a mint, so the tokens move between the two vaults of one asset.
//! Both accounts release value against their own debt valuation, so both are
//! margin checked.

use super::*;

/// The four spot markets a pool transfer moves value between.
struct PoolLegs {
    deposit_from: u16,
    deposit_to: u16,
    borrow_from: u16,
    borrow_to: u16,
}

/// What the caller asked to move. `None` means the whole position.
struct RequestedPoolAmounts {
    deposit: Option<u64>,
    borrow: Option<u64>,
}

/// What the transfer moved on each leg.
struct PoolAmountsMoved {
    deposit: u64,
    borrow: u64,
}

/// The oracle price each of the four markets is valued at.
struct PoolPrices {
    deposit_from: i64,
    deposit_to: i64,
    borrow_from: i64,
    borrow_to: i64,
}

/// One credit or debit of a pool transfer, as the record sees it.
struct PoolSideMove {
    amount: u64,
    oracle_price: i64,
    ts: i64,
}

/// The program-owned vaults a pool transfer moves tokens between, and the
/// signer that authorizes it.
struct PoolVaults<'a, 'info> {
    deposit_from: &'a mut InterfaceAccount<'info, TokenAccount>,
    deposit_to: &'a mut InterfaceAccount<'info, TokenAccount>,
    borrow_from: &'a mut InterfaceAccount<'info, TokenAccount>,
    borrow_to: &'a mut InterfaceAccount<'info, TokenAccount>,
    velocity_signer: &'a UncheckedAccount<'info>,
    signer_nonce: u8,
}

/// Mirror the direct `deposit()` admission checks on a `transfer_pools`
/// deposit-side credit. `transfer_pools` credits every leg through
/// `update_spot_balances_and_cumulative_deposits_with_limits`, which only
/// enforces the *withdraw*-side admission (source debit). For a credit into a
/// destination market that is a deposit, the deposit-side gates must still
/// hold: the market-scoped `SpotOperation::Deposit` pause, the active-status
/// requirement for a positive resulting deposit balance, and the aggregate
/// `max_token_deposits` cap after crediting.
fn enforce_transfer_pools_deposit_admission(
    spot_market: &SpotMarket,
    user: &User,
    market_index: u16,
) -> anchor_lang::Result<()> {
    validate!(
        !spot_market.is_operation_paused(SpotOperation::Deposit),
        ErrorCode::MarketActionPaused,
        "transfer_pools deposit into spot market {} paused",
        market_index
    )?;

    let spot_position = user.get_spot_position(market_index)?;
    if spot_position.balance_type == SpotBalanceType::Deposit && spot_position.scaled_balance > 0 {
        validate!(
            matches!(spot_market.status, MarketStatus::Active),
            ErrorCode::MarketActionPaused,
            "transfer_pools deposit spot market {} not active",
            market_index
        )?;
    }

    spot_market.validate_max_token_deposits_and_borrows(false)?;

    Ok(())
}

/// Prove the two accounts may transfer between pools. They belong to one
/// authority, neither is bankrupt, and they sit in different pools.
fn admit_pool_transfer(parties: &TransferParties<'_>, user_stats: &UserStats) -> Result<()> {
    validate!(
        !user_stats.is_equity_breaker_tripped(),
        ErrorCode::EquityBelowFloor,
        "equity floor breaker is tripped for this authority"
    )?;

    validate!(
        !parties.to_user.is_bankrupt(),
        ErrorCode::UserBankrupt,
        "to_user bankrupt"
    )?;
    validate!(
        !parties.from_user.is_bankrupt(),
        ErrorCode::UserBankrupt,
        "from_user bankrupt"
    )?;

    validate!(
        parties.from_user_key != parties.to_user_key,
        ErrorCode::CantTransferBetweenSameUserAccount,
        "cant transfer between the same user account"
    )?;

    validate!(
        parties.from_user.pool_id != parties.to_user.pool_id,
        ErrorCode::InvalidPoolId,
        "cant transfer between the same pool"
    )?;

    Ok(())
}

/// Prove the four markets form two same-mint pairs across two pools.
fn validate_pool_legs(
    deposit_from: &SpotMarket,
    deposit_to: &SpotMarket,
    borrow_from: &SpotMarket,
    borrow_to: &SpotMarket,
) -> Result<()> {
    validate!(
        deposit_from.mint == deposit_to.mint,
        ErrorCode::InvalidPoolId,
        "deposit from and to spot markets must have the same mint"
    )?;

    validate!(
        borrow_from.mint == borrow_to.mint,
        ErrorCode::InvalidPoolId,
        "borrow from and to spot markets must have the same mint"
    )?;

    validate!(
        deposit_from.pool_id == borrow_from.pool_id,
        ErrorCode::InvalidPoolId,
        "deposit from and borrow from spot markets must have the same pool id"
    )?;

    validate!(
        deposit_to.pool_id == borrow_to.pool_id,
        ErrorCode::InvalidPoolId,
        "deposit to and borrow to spot markets must have the same pool id"
    )?;

    validate!(
        deposit_from.pool_id != deposit_to.pool_id,
        ErrorCode::InvalidPoolId,
        "deposit from and to spot markets must have different pool ids"
    )?;

    Ok(())
}

/// Accrue interest on all four markets, but do not advance their oracle TWAPs (OtterSec #134, the
/// same shape as #110 and #111).
///
/// The margin checks at the end of this transfer run with `Initial`, which enables strict pricing.
/// `StrictOraclePrice` bounds are the min and max of the live price and each market's
/// `last_oracle_price_twap_5min`, and a liability is priced at the *upper* bound. A refresh here,
/// from an instruction any user can call, drags that TWAP toward a temporarily depressed live
/// price, under-values the borrow, and admits a transfer the pre-refresh TWAP rejects. That leaves
/// socialized debt once the oracle recovers. Only pause flags gate this instruction, so the caller
/// controls when it runs. Four markets are refreshed and both accounts are then margin checked, so
/// every one of them is a lever.
///
/// Interest accrual and the deposit/borrow/utilization TWAPs still advance. Only the oracle TWAPs
/// and their timestamps are left alone, so the next real refresh still weights the full elapsed
/// interval. Those TWAPs keep advancing on every other spot path and through the permissionless
/// `update_spot_market_cumulative_interest` crank.
///
/// allow-verbose: the strict-pricing lever this guards is not stated at any callsite.
fn accrue_pool_legs(
    deposit_from: &mut SpotMarket,
    deposit_to: &mut SpotMarket,
    borrow_from: &mut SpotMarket,
    borrow_to: &mut SpotMarket,
    now: i64,
    funding_paused: bool,
) -> Result<()> {
    for spot_market in [deposit_from, deposit_to, borrow_from, borrow_to] {
        controller::spot_balance::update_spot_market_cumulative_interest(
            spot_market,
            None,
            now,
            funding_paused,
        )?;
    }

    Ok(())
}

/// The size of one leg: the amount the caller asked for, or the whole position
/// when they asked for none. `label` names the leg in the error messages.
fn pool_leg_amount(
    user: &mut User,
    spot_market: &SpotMarket,
    requested: Option<u64>,
    balance_type: SpotBalanceType,
    label: &str,
) -> Result<u64> {
    if let Some(0) = requested {
        return Ok(0);
    }

    let spot_position = user.force_get_spot_position_mut(spot_market.market_index)?;

    validate!(
        spot_position.balance_type == balance_type,
        ErrorCode::InvalidSpotPosition,
        "{} from market must be a {} spot position",
        label,
        label
    )?;

    let token_amount = spot_position.get_token_amount(spot_market)?.cast::<u64>()?;

    let amount = requested.unwrap_or(token_amount);

    validate!(
        amount <= token_amount,
        ErrorCode::InvalidSpotPosition,
        "{} amount is greater than the spot position token amount",
        label
    )?;

    Ok(amount)
}

/// Debit one side of a pool transfer and record it.
fn debit_pool_side(
    user: &mut User,
    user_key: Pubkey,
    counterparty_key: Pubkey,
    spot_market: &mut SpotMarket,
    moved: &PoolSideMove,
) -> Result<()> {
    user.increment_total_withdraws(
        moved.amount,
        moved.oracle_price,
        spot_market.get_precision().cast()?,
    )?;

    controller::spot_position::update_spot_balances_and_cumulative_deposits_with_limits(
        moved.amount as u128,
        &SpotBalanceType::Borrow,
        spot_market,
        user,
    )?;

    emit_spot_balance_move(
        user,
        user_key,
        spot_market,
        SpotBalanceMove {
            ts: moved.ts,
            direction: DepositDirection::Withdraw,
            amount: moved.amount,
            oracle_price: moved.oracle_price,
            explanation: DepositExplanation::Transfer,
            transfer_user: Some(counterparty_key),
            signer: None,
            total_deposits_after: user.total_deposits,
            total_withdraws_after: user.total_withdraws,
        },
    )
}

/// Credit one side of a pool transfer and record it. The credit is a deposit,
/// so it carries the admission the shared withdraw-oriented helper skips.
fn credit_pool_side(
    user: &mut User,
    user_key: Pubkey,
    counterparty_key: Pubkey,
    spot_market: &mut SpotMarket,
    moved: &PoolSideMove,
) -> Result<()> {
    user.increment_total_deposits(
        moved.amount,
        moved.oracle_price,
        spot_market.get_precision().cast()?,
    )?;

    controller::spot_position::update_spot_balances_and_cumulative_deposits_with_limits(
        moved.amount as u128,
        &SpotBalanceType::Deposit,
        spot_market,
        user,
    )?;

    enforce_transfer_pools_deposit_admission(spot_market, user, spot_market.market_index)?;

    emit_spot_balance_move(
        user,
        user_key,
        spot_market,
        SpotBalanceMove {
            ts: moved.ts,
            direction: DepositDirection::Deposit,
            amount: moved.amount,
            oracle_price: moved.oracle_price,
            explanation: DepositExplanation::Transfer,
            transfer_user: Some(counterparty_key),
            signer: None,
            total_deposits_after: user.total_deposits,
            total_withdraws_after: user.total_withdraws,
        },
    )
}

/// Read the oracle price of each of the four markets.
fn read_pool_prices(
    oracle_map: &mut OracleMap,
    deposit_from: &SpotMarket,
    deposit_to: &SpotMarket,
    borrow_from: &SpotMarket,
    borrow_to: &SpotMarket,
) -> Result<PoolPrices> {
    Ok(PoolPrices {
        deposit_from: oracle_map.get_price_data(&deposit_from.oracle_id())?.price,
        deposit_to: oracle_map.get_price_data(&deposit_to.oracle_id())?.price,
        borrow_from: oracle_map.get_price_data(&borrow_from.oracle_id())?.price,
        borrow_to: oracle_map.get_price_data(&borrow_to.oracle_id())?.price,
    })
}

/// The price each side of one leg is valued at.
struct LegPrices {
    from: i64,
    to: i64,
}

/// Move the deposit leg: debit the source pool's market and credit the
/// destination pool's market.
fn move_deposit_leg(
    parties: &mut TransferParties<'_>,
    from_market: &mut SpotMarket,
    to_market: &mut SpotMarket,
    amount: u64,
    prices: &LegPrices,
    ts: i64,
) -> Result<()> {
    let (from_key, to_key) = (parties.from_user_key, parties.to_user_key);

    debit_pool_side(
        parties.from_user,
        from_key,
        to_key,
        from_market,
        &PoolSideMove {
            amount,
            oracle_price: prices.from,
            ts,
        },
    )?;

    credit_pool_side(
        parties.to_user,
        to_key,
        from_key,
        to_market,
        &PoolSideMove {
            amount,
            oracle_price: prices.to,
            ts,
        },
    )
}

/// Move the borrow leg: repay the source subaccount's borrow and open the same
/// borrow on the destination.
fn move_borrow_leg(
    parties: &mut TransferParties<'_>,
    from_market: &mut SpotMarket,
    to_market: &mut SpotMarket,
    amount: u64,
    prices: &LegPrices,
    ts: i64,
) -> Result<()> {
    let (from_key, to_key) = (parties.from_user_key, parties.to_user_key);

    credit_pool_side(
        parties.from_user,
        from_key,
        to_key,
        from_market,
        &PoolSideMove {
            amount,
            oracle_price: prices.from,
            ts,
        },
    )?;

    debit_pool_side(
        parties.to_user,
        to_key,
        from_key,
        to_market,
        &PoolSideMove {
            amount,
            oracle_price: prices.to,
            ts,
        },
    )
}

/// Move both legs' balances. The four market borrows are held together for the
/// whole move, so a caller that names one market twice is refused.
fn move_pool_balances(
    parties: &mut TransferParties<'_>,
    maps: &mut AccountMaps,
    legs: &PoolLegs,
    requested: &RequestedPoolAmounts,
    clock: &Clock,
    funding_paused: bool,
) -> Result<PoolAmountsMoved> {
    let mut deposit_from = maps.spot_market_map.get_ref_mut(&legs.deposit_from)?;
    let mut deposit_to = maps.spot_market_map.get_ref_mut(&legs.deposit_to)?;
    let mut borrow_from = maps.spot_market_map.get_ref_mut(&legs.borrow_from)?;
    let mut borrow_to = maps.spot_market_map.get_ref_mut(&legs.borrow_to)?;

    validate_pool_legs(&deposit_from, &deposit_to, &borrow_from, &borrow_to)?;

    let prices = read_pool_prices(
        &mut maps.oracle_map,
        &deposit_from,
        &deposit_to,
        &borrow_from,
        &borrow_to,
    )?;

    let ts = clock.unix_timestamp;
    accrue_pool_legs(
        &mut deposit_from,
        &mut deposit_to,
        &mut borrow_from,
        &mut borrow_to,
        ts,
        funding_paused,
    )?;

    let deposit = pool_leg_amount(
        parties.from_user,
        &deposit_from,
        requested.deposit,
        SpotBalanceType::Deposit,
        "deposit",
    )?;

    if deposit > 0 {
        move_deposit_leg(
            parties,
            &mut deposit_from,
            &mut deposit_to,
            deposit,
            &LegPrices {
                from: prices.deposit_from,
                to: prices.deposit_to,
            },
            ts,
        )?;
    }

    let borrow = pool_leg_amount(
        parties.from_user,
        &borrow_from,
        requested.borrow,
        SpotBalanceType::Borrow,
        "borrow",
    )?;

    if borrow > 0 {
        move_borrow_leg(
            parties,
            &mut borrow_from,
            &mut borrow_to,
            borrow,
            &LegPrices {
                from: prices.borrow_from,
                to: prices.borrow_to,
            },
            ts,
        )?;
    }

    Ok(PoolAmountsMoved { deposit, borrow })
}

/// Prove both accounts still stand after the move, and stamp them active.
fn settle_pool_transfer(
    parties: &mut TransferParties<'_>,
    maps: &mut AccountMaps,
    clock: &Clock,
) -> Result<()> {
    // OtterSec #135: same shape as `handle_withdraw`. This handler cranks only the
    // four markets it moves balances between, and every other borrow market of
    // either account arrives read-only, so their un-booked interest is missing from
    // the checks below. Both accounts are gated: the transfer moves debt onto
    // `to_user`, so each one releases value against its own debt valuation.
    math::margin::validate_spot_borrow_interest_fresh_for_margin(
        parties.from_user,
        &maps.spot_market_map,
        clock.unix_timestamp,
    )?;
    math::margin::validate_spot_borrow_interest_fresh_for_margin(
        parties.to_user,
        &maps.spot_market_map,
        clock.unix_timestamp,
    )?;

    parties.from_user.meets_withdraw_margin_requirement_swap(
        maps,
        MarginRequirementType::Initial,
        false,
    )?;

    parties.to_user.meets_withdraw_margin_requirement_swap(
        maps,
        MarginRequirementType::Initial,
        false,
    )?;

    validate_spot_margin_trading(parties.from_user, maps)?;

    parties.from_user.update_last_active_slot(clock.slot);

    validate_spot_margin_trading(parties.to_user, maps)?;

    parties.to_user.update_last_active_slot(clock.slot);

    if parties.from_user.is_cross_margin_being_liquidated() {
        parties.from_user.exit_cross_margin_liquidation();
    }

    if parties.to_user.is_cross_margin_being_liquidated() {
        parties.to_user.exit_cross_margin_liquidation();
    }

    Ok(())
}

/// The program signer that authorizes a move between two program-owned vaults.
struct VaultSigner<'a, 'info> {
    account: &'a UncheckedAccount<'info>,
    nonce: u8,
}

/// Send one leg's tokens between the two pools' vaults. The token program and
/// the mint come out of `remaining_accounts`, which already carries both.
fn send_between_pool_vaults<'info>(
    spot_market: &SpotMarket,
    from_vault: &InterfaceAccount<'info, TokenAccount>,
    to_vault: &InterfaceAccount<'info, TokenAccount>,
    signer: VaultSigner<'_, 'info>,
    amount: u64,
    remaining_accounts: &'info [AccountInfo<'info>],
) -> Result<()> {
    let token_program_pubkey = spot_market.get_token_program();
    let token_program = &remaining_accounts
        .iter()
        .find(|acc| acc.key() == token_program_pubkey)
        .map(Interface::try_from)
        .unwrap()
        .unwrap();

    let spot_market_mint = &spot_market.mint;
    let mint_account_info = remaining_accounts
        .iter()
        .find(|acc| acc.key() == spot_market_mint.key())
        .map(|acc| InterfaceAccount::try_from(acc).unwrap());

    // TODO: support transfer hook tokens
    controller::token::send_from_program_vault(
        token_program,
        from_vault,
        to_vault,
        signer.account,
        signer.nonce,
        amount,
        &mint_account_info,
        None,
    )
}

/// Re-read a vault and prove it still backs its market.
fn revalidate_pool_vault(
    vault: &mut InterfaceAccount<'_, TokenAccount>,
    spot_market: &SpotMarket,
) -> Result<()> {
    vault.reload()?;
    math::spot_withdraw::validate_spot_market_vault_amount(spot_market, vault.amount)?;

    Ok(())
}

/// Move the tokens the balance changes promised, then prove every vault still
/// backs its market.
fn settle_pool_vaults<'info>(
    vaults: PoolVaults<'_, 'info>,
    maps: &AccountMaps,
    legs: &PoolLegs,
    moved: &PoolAmountsMoved,
    remaining_accounts: &'info [AccountInfo<'info>],
) -> Result<()> {
    let deposit_from = maps.spot_market_map.get_ref(&legs.deposit_from)?;
    let deposit_to = maps.spot_market_map.get_ref(&legs.deposit_to)?;
    let borrow_from = maps.spot_market_map.get_ref(&legs.borrow_from)?;
    let borrow_to = maps.spot_market_map.get_ref(&legs.borrow_to)?;

    if moved.deposit > 0 {
        send_between_pool_vaults(
            &deposit_from,
            vaults.deposit_from,
            vaults.deposit_to,
            VaultSigner {
                account: vaults.velocity_signer,
                nonce: vaults.signer_nonce,
            },
            moved.deposit,
            remaining_accounts,
        )?;
    }

    if moved.borrow > 0 {
        send_between_pool_vaults(
            &borrow_to,
            vaults.borrow_to,
            vaults.borrow_from,
            VaultSigner {
                account: vaults.velocity_signer,
                nonce: vaults.signer_nonce,
            },
            moved.borrow,
            remaining_accounts,
        )?;
    }

    revalidate_pool_vault(vaults.deposit_from, &deposit_from)?;
    revalidate_pool_vault(vaults.deposit_to, &deposit_to)?;
    revalidate_pool_vault(vaults.borrow_from, &borrow_from)?;
    revalidate_pool_vault(vaults.borrow_to, &borrow_to)?;

    Ok(())
}

#[access_control(
    deposit_not_paused(&ctx.accounts.state)
    withdraw_not_paused(&ctx.accounts.state)
)]
pub fn handle_transfer_pools<'c: 'info, 'info>(
    ctx: Context<'info, TransferPools<'info>>,
    deposit_from_market_index: u16,
    deposit_to_market_index: u16,
    borrow_from_market_index: u16,
    borrow_to_market_index: u16,
    deposit_amount: Option<u64>,
    borrow_amount: Option<u64>,
) -> anchor_lang::Result<()> {
    let legs = PoolLegs {
        deposit_from: deposit_from_market_index,
        deposit_to: deposit_to_market_index,
        borrow_from: borrow_from_market_index,
        borrow_to: borrow_to_market_index,
    };
    let requested = RequestedPoolAmounts {
        deposit: deposit_amount,
        borrow: borrow_amount,
    };

    let state = ctx.accounts.state.load()?;
    let clock = Clock::get()?;

    let mut to_user = load_mut!(ctx.accounts.to_user)?;
    let mut from_user = load_mut!(ctx.accounts.from_user)?;
    let user_stats = load_mut!(ctx.accounts.user_stats)?;

    let parties = &mut TransferParties {
        from_user_key: ctx.accounts.from_user.key(),
        to_user_key: ctx.accounts.to_user.key(),
        from_user: &mut from_user,
        to_user: &mut to_user,
        signer: None,
    };

    admit_pool_transfer(parties, &user_stats)?;

    let mut maps = load_maps(
        &mut ctx.remaining_accounts.iter().peekable(),
        &MarketSet::new(),
        &get_writable_spot_market_set_from_many(vec![
            legs.deposit_from,
            legs.deposit_to,
            legs.borrow_from,
            legs.borrow_to,
        ]),
        clock.slot,
        state.slot_clock(),
        Some(state.oracle_guard_rails),
    )?;

    let funding_paused = state.funding_paused()?;
    let moved = move_pool_balances(
        parties,
        &mut maps,
        &legs,
        &requested,
        &clock,
        funding_paused,
    )?;

    settle_pool_transfer(parties, &mut maps, &clock)?;

    settle_pool_vaults(
        PoolVaults {
            deposit_from: &mut ctx.accounts.deposit_from_spot_market_vault,
            deposit_to: &mut ctx.accounts.deposit_to_spot_market_vault,
            borrow_from: &mut ctx.accounts.borrow_from_spot_market_vault,
            borrow_to: &mut ctx.accounts.borrow_to_spot_market_vault,
            velocity_signer: &ctx.accounts.velocity_signer,
            signer_nonce: state.signer_nonce,
        },
        &maps,
        &legs,
        &moved,
        ctx.remaining_accounts,
    )
}

#[derive(Accounts)]
#[instruction(
    deposit_from_market_index: u16,
    deposit_to_market_index: u16,
    borrow_from_market_index: u16,
    borrow_to_market_index: u16,
)]
pub struct TransferPools<'info> {
    #[account(
        mut,
        has_one = authority,
    )]
    pub from_user: AccountLoader<'info, User>,
    #[account(
        mut,
        has_one = authority,
    )]
    pub to_user: AccountLoader<'info, User>,
    #[account(
        mut,
        has_one = authority
    )]
    pub user_stats: AccountLoader<'info, UserStats>,
    pub authority: Signer<'info>,
    pub state: AccountLoader<'info, State>,
    #[account(
        mut,
        seeds = [b"spot_market_vault".as_ref(), deposit_from_market_index.to_le_bytes().as_ref()],
        bump,
    )]
    pub deposit_from_spot_market_vault: Box<InterfaceAccount<'info, TokenAccount>>,
    #[account(
        mut,
        seeds = [b"spot_market_vault".as_ref(), deposit_to_market_index.to_le_bytes().as_ref()],
        bump,
    )]
    pub deposit_to_spot_market_vault: Box<InterfaceAccount<'info, TokenAccount>>,
    #[account(
        mut,
        seeds = [b"spot_market_vault".as_ref(), borrow_from_market_index.to_le_bytes().as_ref()],
        bump,
    )]
    pub borrow_from_spot_market_vault: Box<InterfaceAccount<'info, TokenAccount>>,
    #[account(
        mut,
        seeds = [b"spot_market_vault".as_ref(), borrow_to_market_index.to_le_bytes().as_ref()],
        bump,
    )]
    pub borrow_to_spot_market_vault: Box<InterfaceAccount<'info, TokenAccount>>,
    #[account(
        constraint = state.load()?.signer.eq(&velocity_signer.key())
    )]
    /// CHECK: forced velocity_signer
    pub velocity_signer: UncheckedAccount<'info>,
}
