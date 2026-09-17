//! The two halves of a spot swap.
//!
//! `begin_swap` lends the in leg's tokens out of the market vault and proves
//! the rest of the transaction is a shape it can trust. `end_swap` takes back
//! what the route returned, books both legs, and proves the account still
//! stands. A swap is therefore a flash loan whose repayment is a different
//! asset.

use super::*;

#[derive(Clone, Copy, AnchorSerialize, AnchorDeserialize, PartialEq, Debug, Eq)]
pub enum SwapReduceOnly {
    In,
    Out,
}

/// The markets one swap moves value between.
struct SwapMarkets {
    in_index: u16,
    out_index: u16,
}

/// What the caller asked the swap to do.
struct SwapRequest {
    limit_price: Option<u64>,
    reduce_only: Option<SwapReduceOnly>,
}

/// The two prices `end_swap` values the swap at, and the validity of each.
struct SwapOracles {
    in_data: OraclePriceData,
    in_validity: OracleValidity,
    out_data: OraclePriceData,
    out_validity: OracleValidity,
}

/// The token programs, mints, and transfer-hook accounts a swap moves value
/// through, in the order `remaining_accounts` carries them.
struct SwapTokenRoute<'a, 'info> {
    out_token_program: Option<Interface<'info, TokenInterface>>,
    in_mint: Option<InterfaceAccount<'info, Mint>>,
    out_mint: Option<InterfaceAccount<'info, Mint>>,
    hooks: &'a mut Peekable<Iter<'info, AccountInfo<'info>>>,
}

/// What the in leg did. It holds how much the route spent, and what the
/// account held before and after.
struct InLeg {
    amount_in: u64,
    token_amount_before: i128,
    token_amount_after: i128,
    /// Whether the debit only consumed a deposit the account already held.
    is_reduced: bool,
}

/// What the out leg did.
struct OutLeg {
    amount_out: u64,
    fee: u64,
    token_amount_before: i128,
    token_amount_after: i128,
    /// Whether the credit only repaid a borrow the account already held.
    is_reduced: bool,
}

/// Checks if an instruction is a SPL Token CloseAccount targeting
/// one of the swap's token accounts.
fn is_token_close_account_for_swap_ix(
    ix: &solana_program::instruction::Instruction,
    in_token_account: &Pubkey,
    out_token_account: &Pubkey,
) -> bool {
    let is_token_program = ix.program_id == Token::id() || ix.program_id == Token2022::id();
    if !is_token_program {
        return false;
    }

    // SPL Token CloseAccount discriminator is byte 9
    // (TokenInstruction enum variant index)
    const CLOSE_ACCOUNT_DISCRIMINATOR: u8 = 9;
    if ix.data.is_empty() || ix.data[0] != CLOSE_ACCOUNT_DISCRIMINATOR {
        return false;
    }

    // The first account in CloseAccount is the account being closed
    if ix.accounts.is_empty() {
        return false;
    }

    let account_to_close = &ix.accounts[0].pubkey;
    account_to_close == in_token_account || account_to_close == out_token_account
}

/// The transaction shape `begin_swap` demands of the instructions after it.
///
/// The swap must be top level, the transaction must end with exactly one
/// `end_swap`, and that `end_swap` must name the same accounts. Instructions
/// between the two may only belong to the allowlisted routing programs, and
/// instructions after it may not write anything.
struct SwapTransactionShape<'a, 'info> {
    program_id: &'a Pubkey,
    accounts: &'a Swap<'info>,
    remaining_accounts: &'a [AccountInfo<'info>],
    /// A delegate signer is held to the allowlist alone. An owner may also
    /// route through the token and staking programs.
    delegate_is_signer: bool,
}

impl SwapTransactionShape<'_, '_> {
    /// Walk the rest of the transaction and prove every instruction in it is
    /// one this swap may be wrapped in.
    fn validate(&self, ixs: &AccountInfo<'_>) -> Result<()> {
        let current_index = instructions::load_current_index_checked(ixs)? as usize;

        let current_ix = instructions::load_instruction_at_checked(current_index, ixs)?;
        validate!(
            current_ix.program_id == *self.program_id,
            ErrorCode::InvalidSwap,
            "SwapBegin must be a top-level instruction (cant be cpi)"
        )?;

        // The only other velocity program allowed is SwapEnd
        let mut found_end = false;
        for index in current_index + 1.. {
            let ix = match instructions::load_instruction_at_checked(index, ixs) {
                Ok(ix) => ix,
                Err(ProgramError::InvalidArgument) => break,
                Err(e) => return Err(e.into()),
            };

            // Check that the velocity program key is not used
            if ix.program_id == crate::id() {
                // must be the last ix -- this could possibly be relaxed
                validate!(
                    !found_end,
                    ErrorCode::InvalidSwap,
                    "the transaction must not contain a Velocity instruction after FlashLoanEnd"
                )?;
                found_end = true;

                self.validate_end_swap_ix(&ix)?;
            } else if found_end {
                self.validate_ix_after_end(&ix)?;
            } else {
                self.validate_ix_before_end(&ix)?;
            }
        }

        validate!(
            found_end,
            ErrorCode::InvalidSwap,
            "found no SwapEnd instruction in transaction"
        )?;

        Ok(())
    }

    /// The closing instruction must be `end_swap`, and it must name the same
    /// user, authority, vaults, token accounts, and remaining accounts.
    fn validate_end_swap_ix(&self, ix: &solana_program::instruction::Instruction) -> Result<()> {
        // must be the SwapEnd instruction
        let discriminator = crate::instruction::EndSwap::DISCRIMINATOR;
        validate!(
            &ix.data[0..8] == discriminator,
            ErrorCode::InvalidSwap,
            "last velocity ix must be end of swap"
        )?;

        for (index, expected, name) in [
            (1, self.accounts.user.key(), "user"),
            (3, self.accounts.authority.key(), "authority"),
            (
                4,
                self.accounts.out_spot_market_vault.key(),
                "out_spot_market_vault",
            ),
            (
                5,
                self.accounts.in_spot_market_vault.key(),
                "in_spot_market_vault",
            ),
            (
                6,
                self.accounts.out_token_account.key(),
                "out_token_account",
            ),
            (7, self.accounts.in_token_account.key(), "in_token_account"),
        ] {
            validate!(
                expected == ix.accounts[index].pubkey,
                ErrorCode::InvalidSwap,
                "the {} passed to SwapBegin and End must match",
                name
            )?;
        }

        validate!(
            self.remaining_accounts.len() == ix.accounts.len() - 11,
            ErrorCode::InvalidSwap,
            "begin and end ix must have the same number of accounts"
        )?;

        for i in 11..ix.accounts.len() {
            validate!(
                *self.remaining_accounts[i - 11].key == ix.accounts[i].pubkey,
                ErrorCode::InvalidSwap,
                "begin and end ix must have the same accounts. {}th account mismatch. begin: {}, end: {}",
                i,
                self.remaining_accounts[i - 11].key,
                ix.accounts[i].pubkey
            )?;
        }

        Ok(())
    }

    /// Nothing after `end_swap` may write. Closing the swap's own token
    /// accounts is the one exception, plus assertions that write nothing.
    fn validate_ix_after_end(&self, ix: &solana_program::instruction::Instruction) -> Result<()> {
        if ix.program_id == lighthouse::ID {
            return Ok(());
        }

        // Allow closing the swap's token accounts after end_swap
        if is_token_close_account_for_swap_ix(
            ix,
            &self.accounts.in_token_account.key(),
            &self.accounts.out_token_account.key(),
        ) {
            return Ok(());
        }

        for meta in ix.accounts.iter() {
            validate!(
                !meta.is_writable,
                ErrorCode::InvalidSwap,
                "instructions after swap end must not have writable accounts"
            )?;
        }

        Ok(())
    }

    /// Between the two halves only the allowlisted routing programs may run,
    /// and none of them may name velocity.
    fn validate_ix_before_end(&self, ix: &solana_program::instruction::Instruction) -> Result<()> {
        let mut whitelisted_programs = WHITELISTED_SWAP_PROGRAMS.to_vec();
        if !self.delegate_is_signer {
            whitelisted_programs.push(AssociatedToken::id());
            whitelisted_programs.push(Token::id());
            whitelisted_programs.push(Token2022::id());
            whitelisted_programs.push(marinade_mainnet::ID);
        }
        validate!(
            whitelisted_programs.contains(&ix.program_id),
            ErrorCode::InvalidSwap,
            "only allowed to pass in ixs to ATA, openbook, Jupiter v3/v4/v6, dflow, or titan programs"
        )?;

        for meta in ix.accounts.iter() {
            validate!(
                meta.pubkey != crate::id(),
                ErrorCode::InvalidSwap,
                "instructions between begin and end must not be velocity instructions"
            )?;
        }

        Ok(())
    }
}

/// Prove the account may open a swap, and report whether a delegate signed.
fn admit_swapper(
    user_loader: &AccountLoader<'_, User>,
    authority: &Signer<'_>,
    maps: &mut AccountMaps,
    liquidation_margin_buffer_ratio: u32,
) -> Result<bool> {
    let mut user = load_mut!(user_loader)?;
    let delegate_is_signer = user.delegate == authority.key();

    validate!(!user.is_bankrupt(), ErrorCode::UserBankrupt)?;

    math::liquidation::validate_user_not_being_liquidated(
        &mut user,
        maps,
        liquidation_margin_buffer_ratio,
    )?;

    Ok(delegate_is_signer)
}

/// Accrue interest on one leg of a swap and prove the market may take one.
///
/// Interest and the deposit, borrow, and utilization TWAPs advance. `None` is
/// passed, so the oracle TWAPs do not. `end_swap` measures the realized fill
/// against `last_oracle_price_twap_5min` on this market. A refresh here runs in
/// the same transaction, from an instruction the swapper controls. It pulls
/// that anchor toward the live oracle price and widens the band, so an
/// underpriced swap passes a check the pre-refresh TWAP rejects (OtterSec
/// #110). `validate_price_bands_for_swap` reads whichever of the two markets
/// has a zero initial margin ratio, so neither side may refresh before it runs.
///
/// `end_swap` advances both markets' oracle TWAPs after its band check, so the
/// swap still contributes to the EMA. The two halves are separate instructions,
/// so no in-memory snapshot can cross the check the way the perp fill does for
/// OtterSec #112.
fn arm_swap_leg(
    spot_market: &mut SpotMarket,
    market_index: u16,
    now: i64,
    funding_paused: bool,
) -> Result<()> {
    validate!(
        spot_market.fills_enabled(),
        ErrorCode::MarketFillOrderPaused,
        "Swaps disabled for {}",
        market_index
    )?;

    validate!(
        spot_market.flash_loan_initial_token_amount == 0 && spot_market.flash_loan_amount == 0,
        ErrorCode::InvalidSwap,
        "begin_swap ended in invalid state"
    )?;

    controller::spot_balance::update_spot_market_cumulative_interest(
        spot_market,
        None,
        now,
        funding_paused,
    )?;

    Ok(())
}

#[access_control(
    fill_not_paused(&ctx.accounts.state)
)]
pub fn handle_begin_swap<'c: 'info, 'info>(
    ctx: Context<'info, Swap<'info>>,
    in_market_index: u16,
    out_market_index: u16,
    amount_in: u64,
) -> Result<()> {
    let state = ctx.accounts.state.load()?;
    let clock = Clock::get()?;
    let now = clock.unix_timestamp;

    let remaining_accounts_iter = &mut ctx.remaining_accounts.iter().peekable();
    let mut maps = load_maps(
        remaining_accounts_iter,
        &MarketSet::new(),
        &get_writable_spot_market_set_from_many(vec![in_market_index, out_market_index]),
        clock.slot,
        state.slot_clock(),
        Some(state.oracle_guard_rails),
    )?;

    let _token_interface = get_token_interface(remaining_accounts_iter)?;
    let mint = get_token_mint(remaining_accounts_iter)?;

    let delegate_is_signer = admit_swapper(
        &ctx.accounts.user,
        &ctx.accounts.authority,
        &mut maps,
        state.liquidation_margin_buffer_ratio,
    )?;

    let funding_paused = state.funding_paused()?;

    let mut in_spot_market = maps.spot_market_map.get_ref_mut(&in_market_index)?;
    arm_swap_leg(&mut in_spot_market, in_market_index, now, funding_paused)?;

    let mut out_spot_market = maps.spot_market_map.get_ref_mut(&out_market_index)?;

    let in_spot_has_transfer_hook = in_spot_market.has_transfer_hook();
    validate!(
        !(in_spot_has_transfer_hook && out_spot_market.has_transfer_hook()),
        ErrorCode::InvalidSwap,
        "both in and out spot markets cannot both have transfer hooks"
    )?;

    arm_swap_leg(&mut out_spot_market, out_market_index, now, funding_paused)?;

    validate!(
        in_market_index != out_market_index,
        ErrorCode::InvalidSwap,
        "in and out market the same"
    )?;

    validate!(
        amount_in != 0,
        ErrorCode::InvalidSwap,
        "amount_out cannot be zero"
    )?;

    in_spot_market.flash_loan_amount = amount_in;
    in_spot_market.flash_loan_initial_token_amount = ctx.accounts.in_token_account.amount;
    out_spot_market.flash_loan_initial_token_amount = ctx.accounts.out_token_account.amount;

    controller::token::send_from_program_vault(
        &ctx.accounts.token_program,
        &ctx.accounts.in_spot_market_vault,
        &ctx.accounts.in_token_account,
        &ctx.accounts.velocity_signer,
        state.signer_nonce,
        amount_in,
        &mint,
        if in_spot_has_transfer_hook {
            Some(remaining_accounts_iter)
        } else {
            None
        },
    )?;

    SwapTransactionShape {
        program_id: ctx.program_id,
        accounts: ctx.accounts,
        remaining_accounts: ctx.remaining_accounts,
        delegate_is_signer,
    }
    .validate(ctx.accounts.instructions.as_ref())
}

/// The two spot markets one swap moves value between, borrowed for reading.
#[derive(Clone, Copy)]
struct SwapLegMarkets<'a> {
    in_market: &'a SpotMarket,
    out_market: &'a SpotMarket,
}

/// The two spot markets one swap moves value between, borrowed for writing.
struct SwapLegMarketsMut<'a> {
    in_market: &'a mut SpotMarket,
    out_market: &'a mut SpotMarket,
}

impl<'a> SwapLegMarketsMut<'a> {
    fn as_ref(&self) -> SwapLegMarkets<'_> {
        SwapLegMarkets {
            in_market: self.in_market,
            out_market: self.out_market,
        }
    }
}

/// What both legs settled to, and what the rest of the instruction must hold
/// the account to.
struct BookedSwap {
    oracles: SwapOracles,
    in_leg: InLeg,
    out_leg: OutLeg,
    margin_type: MarginRequirementType,
    /// A swap that only consumes an existing deposit to repay an existing
    /// borrow. It stays allowed under equity floor protection.
    strictly_reducing: bool,
}

/// The party and token accounts an `end_swap` moves value through. Held as
/// separate fields so the `User` load and the vault writes borrow disjoint
/// parts of the instruction's account set.
struct EndSwapAccounts<'a, 'info> {
    user: &'a AccountLoader<'info, User>,
    user_stats: &'a AccountLoader<'info, UserStats>,
    token_program: &'a Interface<'info, TokenInterface>,
    authority: &'a Signer<'info>,
    in_token_account: &'a mut InterfaceAccount<'info, TokenAccount>,
    in_vault: &'a mut InterfaceAccount<'info, TokenAccount>,
    out_token_account: &'a mut InterfaceAccount<'info, TokenAccount>,
    out_vault: &'a mut InterfaceAccount<'info, TokenAccount>,
}

/// One `end_swap` in flight.
struct EndSwap<'a, 'info> {
    accounts: EndSwapAccounts<'a, 'info>,
    maps: &'a mut AccountMaps<'info>,
    route: SwapTokenRoute<'a, 'info>,
    markets: SwapMarkets,
    request: SwapRequest,
    clock: Clock,
}

/// Read one leg's oracle.
///
/// The reading is copied out of the map rather than borrowed from it. The TWAP
/// refresh at the end of the instruction needs it, and holding a reference
/// would keep `oracle_map` borrowed across every margin call in between.
fn read_swap_leg_oracle(
    oracle_map: &mut OracleMap,
    spot_market: &SpotMarket,
) -> Result<(OraclePriceData, OracleValidity)> {
    let (data, validity) = oracle_map.get_price_data_and_validity(
        MarketType::Spot,
        spot_market.market_index,
        &spot_market.oracle_id(),
        spot_market.historical_oracle_data.last_oracle_price_twap,
        spot_market.get_max_confidence_interval_multiplier()?,
        -1,
        0,
        Some(LogMode::Margin),
    )?;

    Ok((*data, validity))
}

/// A debit that grows a liability needs margin trading, an account that is not
/// reduce-only, and a market that is not reduce-only.
fn validate_in_leg_liability(
    user: &User,
    in_market: &SpotMarket,
    leg: &InLeg,
    request: &SwapRequest,
) -> Result<()> {
    if leg.is_reduced {
        return Ok(());
    }

    validate!(
        !in_market.is_reduce_only(),
        ErrorCode::SpotMarketReduceOnly,
        "in spot market is reduce only but token amount before ({}) < amount in ({})",
        leg.token_amount_before,
        leg.amount_in
    )?;

    validate!(
        request.reduce_only != Some(SwapReduceOnly::In),
        ErrorCode::InvalidSwap,
        "reduce only violated. In position before ({}) < amount in ({})",
        leg.token_amount_before,
        leg.amount_in
    )?;

    validate!(
        user.is_margin_trading_enabled,
        ErrorCode::MarginTradingDisabled,
        "swap lead to increase in liability for in market {}",
        in_market.market_index
    )?;

    validate!(
        !user.is_reduce_only(),
        ErrorCode::UserReduceOnly,
        "swap lead to increase in liability for in market {}",
        in_market.market_index
    )?;

    Ok(())
}

/// Take back what the route did not spend, then debit the in leg. The debit
/// closes the flash loan the market lent.
fn repay_in_leg<'info>(
    accounts: &mut EndSwapAccounts<'_, 'info>,
    in_market: &mut SpotMarket,
    user: &mut User,
    route: &mut SwapTokenRoute<'_, 'info>,
    request: &SwapRequest,
) -> Result<InLeg> {
    let market_index = in_market.market_index;
    let mut amount_in = in_market.flash_loan_amount;

    if accounts.in_token_account.amount > in_market.flash_loan_initial_token_amount {
        let residual = accounts
            .in_token_account
            .amount
            .safe_sub(in_market.flash_loan_initial_token_amount)?;

        controller::token::receive(
            accounts.token_program,
            accounts.in_token_account,
            accounts.in_vault,
            accounts.authority,
            residual,
            &route.in_mint,
            if in_market.has_transfer_hook() {
                Some(route.hooks)
            } else {
                None
            },
        )?;
        accounts.in_token_account.reload()?;
        accounts.in_vault.reload()?;

        amount_in = amount_in.safe_sub(residual)?;
    }

    let token_amount_before = user
        .force_get_spot_position_mut(market_index)?
        .get_signed_token_amount(in_market)?;

    // checks deposit/borrow limits
    update_spot_balances_and_cumulative_deposits_with_limits(
        amount_in.cast()?,
        &SpotBalanceType::Borrow,
        in_market,
        user,
    )?;

    let token_amount_after = user
        .force_get_spot_position_mut(market_index)?
        .get_signed_token_amount(in_market)?;

    let leg = InLeg {
        amount_in,
        token_amount_before,
        token_amount_after,
        is_reduced: token_amount_before > 0
            && token_amount_before.unsigned_abs() >= amount_in.cast()?,
    };

    validate_in_leg_liability(user, in_market, &leg, request)?;

    math::spot_withdraw::validate_spot_market_vault_amount(in_market, accounts.in_vault.amount)?;

    in_market.flash_loan_initial_token_amount = 0;
    in_market.flash_loan_amount = 0;

    Ok(leg)
}

/// Take in whatever the route produced above the balance the vault started
/// with. The out leg's own token program is used when the caller passed one.
fn receive_out_tokens<'info>(
    accounts: &mut EndSwapAccounts<'_, 'info>,
    out_market: &SpotMarket,
    route: &mut SwapTokenRoute<'_, 'info>,
) -> Result<u64> {
    if accounts.out_token_account.amount <= out_market.flash_loan_initial_token_amount {
        return Ok(0);
    }

    let amount_out = accounts
        .out_token_account
        .amount
        .safe_sub(out_market.flash_loan_initial_token_amount)?;

    let token_program = route
        .out_token_program
        .as_ref()
        .unwrap_or(accounts.token_program);

    controller::token::receive(
        token_program,
        accounts.out_token_account,
        accounts.out_vault,
        accounts.authority,
        amount_out,
        &route.out_mint,
        if out_market.has_transfer_hook() {
            Some(route.hooks)
        } else {
            None
        },
    )?;

    accounts.out_vault.reload()?;

    Ok(amount_out)
}

/// Prove the realized exchange rate clears the caller's limit price.
fn validate_swap_limit_price(
    limit_price: Option<u64>,
    markets: SwapLegMarkets<'_>,
    amount_in: u64,
    amount_out: u64,
) -> Result<()> {
    let Some(limit_price) = limit_price else {
        return Ok(());
    };

    let swap_price = calculate_swap_price(
        amount_out.cast()?,
        amount_in.cast()?,
        markets.out_market.decimals,
        markets.in_market.decimals,
    )?;

    validate!(
        swap_price >= limit_price.cast()?,
        ErrorCode::SwapLimitPriceBreached,
        "swap_price ({}) < limit price ({})",
        swap_price,
        limit_price
    )?;

    Ok(())
}

/// Charge the swap fee and credit the taker volume it earns. The fee is zero
/// today, so the volume credit never runs.
fn charge_swap_fee(
    user: &mut User,
    user_stats: &mut UserStats,
    out_market: &mut SpotMarket,
    amount_out: u64,
    out_oracle_price: i64,
    now: i64,
) -> Result<u64> {
    let fee = 0_u64; // no fee
    out_market.total_swap_fee = out_market.total_swap_fee.saturating_add(fee);

    let fee_value = get_token_value(fee.cast()?, out_market.decimals, out_oracle_price)?;

    // update fees
    user.update_cumulative_spot_fees(-fee_value.cast()?)?;
    user_stats.increment_total_fees(fee_value.cast()?)?;

    if fee != 0 {
        // update taker volume
        let amount_out_value =
            get_token_value(amount_out.cast()?, out_market.decimals, out_oracle_price)?;
        user_stats.update_taker_volume_30d(amount_out_value.cast()?, now)?;
    }

    Ok(fee)
}

/// A credit that grows a deposit needs an account that is not reduce-only, and
/// a market that is not reduce-only.
fn validate_out_leg_increase(
    user: &User,
    out_market: &SpotMarket,
    leg: &OutLeg,
    request: &SwapRequest,
) -> Result<()> {
    if leg.is_reduced {
        return Ok(());
    }

    validate!(
        !out_market.is_reduce_only(),
        ErrorCode::SpotMarketReduceOnly,
        "out spot market is reduce only but token amount before ({}) < amount out ({})",
        leg.token_amount_before,
        leg.amount_out
    )?;

    validate!(
        request.reduce_only != Some(SwapReduceOnly::Out),
        ErrorCode::InvalidSwap,
        "reduce only violated. Out position before ({}) < amount out ({})",
        leg.token_amount_before,
        leg.amount_out
    )?;

    validate!(
        !user.is_reduce_only(),
        ErrorCode::UserReduceOnly,
        "swap lead to increase in deposit for in market {}, can only pay off borrow",
        out_market.market_index
    )?;

    Ok(())
}

/// Credit the out leg to the account and to the revenue pool.
fn credit_out_leg(
    user: &mut User,
    out_market: &mut SpotMarket,
    amount_out: u64,
    fee: u64,
    request: &SwapRequest,
) -> Result<OutLeg> {
    let market_index = out_market.market_index;
    let amount_out_after_fee = amount_out.safe_sub(fee)?;

    let token_amount_before = user
        .force_get_spot_position_mut(market_index)?
        .get_signed_token_amount(out_market)?;

    let deposit_token_amount_before = math::spot_balance::get_token_amount(
        out_market.deposit_balance,
        out_market,
        &SpotBalanceType::Deposit,
    )?;

    update_spot_balances_and_cumulative_deposits(
        amount_out_after_fee.cast()?,
        &SpotBalanceType::Deposit,
        out_market,
        user.force_get_spot_position_mut(market_index)?,
        false,
        Some(amount_out.cast()?),
    )?;

    let token_amount_after = user
        .force_get_spot_position_mut(market_index)?
        .get_signed_token_amount(out_market)?;

    // update fees
    update_revenue_pool_balances(fee.cast()?, &SpotBalanceType::Deposit, out_market, false)?;

    // The out leg credits deposits through the plain balance update, not the shared
    // `_with_limits` path, so the daily deposit cap did not apply to it at all. A swapper
    // could lift a market's deposit level far above its cap. That locks every other user
    // out of withdrawing or repaying in that market while liquidation stays live against
    // them (OtterSec #118). The check runs after the revenue-pool fee credit, so it sees the
    // whole out-side increase. It passes when the deposit level did not grow, so a swap
    // that only repays an existing borrow is never rejected.
    math::spot_withdraw::validate_deposit_cap_after_increase(
        out_market,
        deposit_token_amount_before,
    )?;

    let leg = OutLeg {
        amount_out,
        fee,
        token_amount_before,
        token_amount_after,
        is_reduced: token_amount_before < 0
            && token_amount_before.unsigned_abs() >= amount_out_after_fee.cast()?,
    };

    validate_out_leg_increase(user, out_market, &leg, request)?;

    Ok(leg)
}

/// Bound the value loss of a swap that the equity floor gates exempt.
///
/// The bound is the exemption's only safety, so both legs must have valid
/// oracles. It values the leg given up at the strict max and the leg received
/// at the strict min of the live price and the 5-minute TWAP, so a stale sample
/// cannot flatter the exchange rate.
fn validate_bounded_value_loss(
    markets: SwapLegMarkets<'_>,
    oracles: &SwapOracles,
    in_leg: &InLeg,
    out_leg: &OutLeg,
) -> Result<()> {
    validate!(
        is_oracle_valid_for_action(oracles.in_validity, Some(VelocityAction::MarginCalc))?,
        ErrorCode::InvalidOracle,
        "in oracle invalid for swap under equity floor protection"
    )?;

    validate!(
        is_oracle_valid_for_action(oracles.out_validity, Some(VelocityAction::MarginCalc))?,
        ErrorCode::InvalidOracle,
        "out oracle invalid for swap under equity floor protection"
    )?;

    let (in_strict_price, out_strict_price) = strict_swap_prices(markets, oracles);
    in_strict_price.validate()?;
    out_strict_price.validate()?;

    let in_value = get_token_value(
        in_leg.amount_in.cast()?,
        markets.in_market.decimals,
        in_strict_price.max(),
    )?;
    let out_value = get_token_value(
        out_leg.amount_out.cast()?,
        markets.out_market.decimals,
        out_strict_price.min(),
    )?;

    let min_out_value = in_value
        .safe_mul(
            (ONE_BPS_DENOMINATOR as i128).safe_sub(EQUITY_FLOOR_SWAP_MAX_VALUE_LOSS_BPS.cast()?)?,
        )?
        .safe_div(ONE_BPS_DENOMINATOR as i128)?;

    validate!(
        out_value >= min_out_value,
        ErrorCode::InvalidSwap,
        "swap under equity floor protection: out value {} below min {} (in value {})",
        out_value,
        min_out_value,
        in_value
    )?;

    Ok(())
}

/// Hold the swap to the equity floor.
///
/// A strictly reducing swap consumes an existing deposit and repays an existing
/// borrow. It stays allowed while the authority-wide breaker is tripped or the
/// subaccount is below its floor, so a frozen account can still deleverage
/// instead of being forced into a liquidation loss. Anything else must respect
/// the breaker like withdrawals and transfers out. The per-subaccount floor is
/// enforced in `meets_withdraw_margin_requirement_swap`.
fn validate_swap_under_floor(
    user: &User,
    user_stats: &UserStats,
    markets: SwapLegMarkets<'_>,
    oracles: &SwapOracles,
    in_leg: &InLeg,
    out_leg: &OutLeg,
) -> Result<()> {
    let strictly_reducing = in_leg.is_reduced && out_leg.is_reduced;

    if user_stats.is_equity_breaker_tripped() {
        validate!(
            strictly_reducing,
            ErrorCode::EquityBelowFloor,
            "equity floor breaker is tripped for this authority; only a swap consuming an existing deposit to repay an existing borrow is allowed"
        )?;
    }

    if strictly_reducing && (user_stats.is_equity_breaker_tripped() || user.equity_floor > 0) {
        validate_bounded_value_loss(markets, oracles, in_leg, out_leg)?;
    }

    Ok(())
}

/// The strict prices the margin check values each leg at.
fn strict_swap_prices(
    markets: SwapLegMarkets<'_>,
    oracles: &SwapOracles,
) -> (StrictOraclePrice, StrictOraclePrice) {
    (
        StrictOraclePrice::new(
            oracles.in_data.price,
            markets
                .in_market
                .historical_oracle_data
                .last_oracle_price_twap_5min,
            true,
        ),
        StrictOraclePrice::new(
            oracles.out_data.price,
            markets
                .out_market
                .historical_oracle_data
                .last_oracle_price_twap_5min,
            true,
        ),
    )
}

/// The margin type the account must meet after the swap.
fn swap_margin_type(
    markets: SwapLegMarkets<'_>,
    oracles: &SwapOracles,
    in_leg: &InLeg,
    out_leg: &OutLeg,
) -> Result<MarginRequirementType> {
    let (in_strict_price, out_strict_price) = strict_swap_prices(markets, oracles);

    let (margin_type, _) = spot_swap::select_margin_type_for_swap(
        markets.in_market,
        markets.out_market,
        &in_strict_price,
        &out_strict_price,
        in_leg.token_amount_before,
        out_leg.token_amount_before,
        in_leg.token_amount_after,
        out_leg.token_amount_after,
        MarginRequirementType::Initial,
    )?;

    Ok(margin_type)
}

/// Book both legs onto the account, once both markets are borrowed.
fn book_swap_onto_account<'info>(
    accounts: &mut EndSwapAccounts<'_, 'info>,
    markets: &mut SwapLegMarketsMut<'_>,
    route: &mut SwapTokenRoute<'_, 'info>,
    request: &SwapRequest,
    oracles: &SwapOracles,
    now: i64,
) -> Result<BookedSwap> {
    let mut user = load_mut!(accounts.user)?;
    let mut user_stats = load_mut!(accounts.user_stats)?;

    let in_leg = repay_in_leg(accounts, markets.in_market, &mut user, route, request)?;

    let amount_out = receive_out_tokens(accounts, markets.out_market, route)?;

    validate_swap_limit_price(
        request.limit_price,
        markets.as_ref(),
        in_leg.amount_in,
        amount_out,
    )?;

    let fee = charge_swap_fee(
        &mut user,
        &mut user_stats,
        markets.out_market,
        amount_out,
        oracles.out_data.price,
        now,
    )?;

    validate!(
        amount_out != 0,
        ErrorCode::InvalidSwap,
        "amount_out must be greater than 0"
    )?;

    let out_leg = credit_out_leg(&mut user, markets.out_market, amount_out, fee, request)?;

    validate_swap_under_floor(
        &user,
        &user_stats,
        markets.as_ref(),
        oracles,
        &in_leg,
        &out_leg,
    )?;

    math::spot_withdraw::validate_spot_market_vault_amount(
        markets.out_market,
        accounts.out_vault.amount,
    )?;

    markets.out_market.flash_loan_initial_token_amount = 0;
    markets.out_market.flash_loan_amount = 0;

    markets
        .out_market
        .validate_max_token_deposits_and_borrows(false)?;

    Ok(BookedSwap {
        margin_type: swap_margin_type(markets.as_ref(), oracles, &in_leg, &out_leg)?,
        strictly_reducing: in_leg.is_reduced && out_leg.is_reduced,
        oracles: SwapOracles { ..*oracles },
        in_leg,
        out_leg,
    })
}

/// Prove the swap left no flash loan open, hold the realized fill to the price
/// band, and then advance both markets' oracle TWAPs.
///
/// The TWAPs advance last, after the band check has read them. `begin_swap`
/// passes `None`, so the swap does not refresh the anchor that
/// `validate_price_bands_for_swap` measures the realized fill against
/// (OtterSec #110). The refresh happens here so the swap lane still
/// contributes to the EMA. It is safe because the check is done, and
/// `begin_swap` forbids any Velocity instruction after `end_swap`.
/// `begin_swap` left `last_oracle_price_twap_ts` alone, so this update still
/// weights the full elapsed interval. The deposit, borrow, and utilization
/// TWAPs advanced there and stamped `last_twap_ts`, so they do not change
/// here.
fn close_swap_flash_loan(
    maps: &mut AccountMaps,
    markets: &SwapMarkets,
    booked: &BookedSwap,
    max_oracle_twap_5min_percent_divergence: u64,
) -> Result<()> {
    let mut out_market = maps.spot_market_map.get_ref_mut(&markets.out_index)?;

    validate!(
        out_market.flash_loan_initial_token_amount == 0 && out_market.flash_loan_amount == 0,
        ErrorCode::InvalidSwap,
        "end_swap ended in invalid state"
    )?;

    let mut in_market = maps.spot_market_map.get_ref_mut(&markets.in_index)?;

    validate!(
        in_market.flash_loan_initial_token_amount == 0 && in_market.flash_loan_amount == 0,
        ErrorCode::InvalidSwap,
        "end_swap ended in invalid state"
    )?;

    validate_price_bands_for_swap(
        &in_market,
        &out_market,
        booked.in_leg.amount_in,
        booked.out_leg.amount_out,
        booked.oracles.in_data.price,
        booked.oracles.out_data.price,
        max_oracle_twap_5min_percent_divergence,
    )?;

    let now = Clock::get()?.unix_timestamp;
    controller::spot_balance::update_spot_market_twap_stats(
        &mut in_market,
        Some(&booked.oracles.in_data),
        now,
    )?;
    controller::spot_balance::update_spot_market_twap_stats(
        &mut out_market,
        Some(&booked.oracles.out_data),
        now,
    )?;

    Ok(())
}

impl EndSwap<'_, '_> {
    /// Book both legs while holding both markets, so a caller that names one
    /// market twice is refused.
    fn book_legs(&mut self) -> Result<BookedSwap> {
        let mut in_market = self
            .maps
            .spot_market_map
            .get_ref_mut(&self.markets.in_index)?;

        validate!(
            !in_market.is_operation_paused(SpotOperation::Withdraw),
            ErrorCode::MarketFillOrderPaused,
            "withdraw from market {} paused",
            self.markets.in_index
        )?;

        validate!(
            in_market.flash_loan_amount != 0,
            ErrorCode::InvalidSwap,
            "the in_spot_market must have a flash loan amount set"
        )?;

        let (in_data, in_validity) = read_swap_leg_oracle(&mut self.maps.oracle_map, &in_market)?;

        let mut out_market = self
            .maps
            .spot_market_map
            .get_ref_mut(&self.markets.out_index)?;

        validate!(
            !out_market.is_operation_paused(SpotOperation::Deposit),
            ErrorCode::MarketFillOrderPaused,
            "deposit to market {} paused",
            self.markets.out_index
        )?;

        let (out_data, out_validity) =
            read_swap_leg_oracle(&mut self.maps.oracle_map, &out_market)?;

        book_swap_onto_account(
            &mut self.accounts,
            &mut SwapLegMarketsMut {
                in_market: &mut in_market,
                out_market: &mut out_market,
            },
            &mut self.route,
            &self.request,
            &SwapOracles {
                in_data,
                in_validity,
                out_data,
                out_validity,
            },
            self.clock.unix_timestamp,
        )
    }

    /// Prove the account still stands, record the swap, and close the flash
    /// loan out.
    fn settle(&mut self, booked: &BookedSwap, state: &State) -> Result<()> {
        let now = self.clock.unix_timestamp;
        let user_key = self.accounts.user.key();
        let mut user = load_mut!(self.accounts.user)?;
        let mut user_stats = load_mut!(self.accounts.user_stats)?;

        // OtterSec #135: same shape as `handle_withdraw`. This handler cranks only the
        // two markets it swaps between, and the account's other borrow markets arrive
        // read-only, so their un-booked interest is missing from the check below.
        math::margin::validate_spot_borrow_interest_fresh_for_margin(
            &user,
            &self.maps.spot_market_map,
            now,
        )?;

        user.meets_withdraw_margin_requirement_swap(
            self.maps,
            booked.margin_type,
            booked.strictly_reducing,
        )?;

        // The exempt swap skips the buffered-floor gate and may legally end below
        // the raw floor. The breaker is armed here instead of waiting for the
        // permissionless trip.
        if booked.strictly_reducing {
            controller::equity_floor::try_lazy_equity_breaker_trip(
                &user,
                &mut user_stats,
                self.maps,
            )?;
        }

        user.update_last_active_slot(self.clock.slot);

        emit!(SwapRecord {
            ts: now,
            amount_in: booked.in_leg.amount_in,
            amount_out: booked.out_leg.amount_out,
            out_market_index: self.markets.out_index,
            in_market_index: self.markets.in_index,
            in_oracle_price: booked.oracles.in_data.price,
            out_oracle_price: booked.oracles.out_data.price,
            user: user_key,
            fee: booked.out_leg.fee,
        });

        close_swap_flash_loan(
            self.maps,
            &self.markets,
            booked,
            state
                .oracle_guard_rails
                .max_oracle_twap_5min_percent_divergence(),
        )?;

        user_stats.try_auto_enroll_accelerated_referral_and_emit(now);

        Ok(())
    }
}

#[access_control(
    fill_not_paused(&ctx.accounts.state)
)]
pub fn handle_end_swap<'c: 'info, 'info>(
    ctx: Context<'info, Swap<'info>>,
    in_market_index: u16,
    out_market_index: u16,
    limit_price: Option<u64>,
    reduce_only: Option<SwapReduceOnly>,
) -> Result<()> {
    let state = ctx.accounts.state.load()?;
    let clock = Clock::get()?;

    let remaining_accounts = &mut ctx.remaining_accounts.iter().peekable();
    let mut maps = load_maps(
        remaining_accounts,
        &MarketSet::new(),
        &get_writable_spot_market_set_from_many(vec![in_market_index, out_market_index]),
        clock.slot,
        state.slot_clock(),
        Some(state.oracle_guard_rails),
    )?;

    let route = SwapTokenRoute {
        out_token_program: get_token_interface(remaining_accounts)?,
        in_mint: get_token_mint(remaining_accounts)?,
        out_mint: get_token_mint(remaining_accounts)?,
        hooks: remaining_accounts,
    };

    let exchange_status = state.get_exchange_status()?;

    validate!(
        !exchange_status.contains(ExchangeStatus::DepositPaused | ExchangeStatus::WithdrawPaused),
        ErrorCode::ExchangePaused
    )?;

    let mut swap = EndSwap {
        accounts: EndSwapAccounts {
            user: &ctx.accounts.user,
            user_stats: &ctx.accounts.user_stats,
            token_program: &ctx.accounts.token_program,
            authority: &ctx.accounts.authority,
            in_token_account: &mut ctx.accounts.in_token_account,
            in_vault: &mut ctx.accounts.in_spot_market_vault,
            out_token_account: &mut ctx.accounts.out_token_account,
            out_vault: &mut ctx.accounts.out_spot_market_vault,
        },
        maps: &mut maps,
        route,
        markets: SwapMarkets {
            in_index: in_market_index,
            out_index: out_market_index,
        },
        request: SwapRequest {
            limit_price,
            reduce_only,
        },
        clock,
    };

    let booked = swap.book_legs()?;

    swap.settle(&booked, &state)
}

#[derive(Accounts)]
#[instruction(in_market_index: u16, out_market_index: u16, )]
pub struct Swap<'info> {
    pub state: AccountLoader<'info, State>,
    #[account(
        mut,
        constraint = can_sign_for_user(&user, &authority)?
    )]
    pub user: AccountLoader<'info, User>,
    #[account(
        mut,
        constraint = is_stats_for_user(&user, &user_stats)?
    )]
    pub user_stats: AccountLoader<'info, UserStats>,
    pub authority: Signer<'info>,
    #[account(
        mut,
        seeds = [b"spot_market_vault".as_ref(), out_market_index.to_le_bytes().as_ref()],
        bump,
    )]
    pub out_spot_market_vault: Box<InterfaceAccount<'info, TokenAccount>>,
    #[account(
        mut,
        seeds = [b"spot_market_vault".as_ref(), in_market_index.to_le_bytes().as_ref()],
        bump,
    )]
    pub in_spot_market_vault: Box<InterfaceAccount<'info, TokenAccount>>,
    #[account(
        mut,
        constraint = &out_spot_market_vault.mint.eq(&out_token_account.mint),
        token::authority = authority
    )]
    pub out_token_account: Box<InterfaceAccount<'info, TokenAccount>>,
    #[account(
        mut,
        constraint = &in_spot_market_vault.mint.eq(&in_token_account.mint),
        token::authority = authority
    )]
    pub in_token_account: Box<InterfaceAccount<'info, TokenAccount>>,
    pub token_program: Interface<'info, TokenInterface>,
    #[account(
        constraint = state.load()?.signer.eq(&velocity_signer.key())
    )]
    /// CHECK: forced velocity_signer
    pub velocity_signer: UncheckedAccount<'info>,
    /// Instructions Sysvar for instruction introspection
    /// CHECK: fixed instructions sysvar account
    #[account(address = instructions::ID)]
    pub instructions: UncheckedAccount<'info>,
}
