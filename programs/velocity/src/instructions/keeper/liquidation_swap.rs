//! The flash-loan liquidation pair.
//!
//! `begin` lends the liquidatee's collateral out of the asset vault and proves
//! that a matching `end` closes the transaction. The swap runs between them.
//! `end` books what came back and settles the liquidation against it.

use super::*;

/// The two markets one flash-loan liquidation moves between, and how much of
/// the asset it lends out.
struct SwapLegs {
    asset_market_index: u16,
    liability_market_index: u16,
    swap_amount: u64,
}

#[access_control(
    liq_not_paused(&ctx.accounts.state)
)]
pub fn handle_liquidate_spot_with_swap_begin<'c: 'info, 'info>(
    ctx: Context<'info, LiquidateSpotWithSwap<'info>>,
    asset_market_index: u16,
    liability_market_index: u16,
    swap_amount: u64,
) -> Result<()> {
    let state = ctx.accounts.state.load()?;
    let clock = Clock::get()?;
    let now = clock.unix_timestamp;
    let legs = SwapLegs {
        asset_market_index,
        liability_market_index,
        swap_amount,
    };

    let user_key = ctx.accounts.user.key();
    let liquidator_key = ctx.accounts.liquidator.key();

    validate!(
        user_key != liquidator_key,
        ErrorCode::UserCantLiquidateThemself
    )?;

    let user = &mut load_mut!(ctx.accounts.user)?;
    let liquidator = &mut load_mut!(ctx.accounts.liquidator)?;
    // A swap-backed liquidation earns the liquidation fee like the other
    // liquidator routes, even though the tokens flow through the authority's
    // wallet accounts rather than the liquidator subaccount. The check runs
    // before any flash-loan state opens; `end` runs in the same transaction,
    // so checking `begin` covers the pair.
    //
    // Only the breaker, deliberately: the four direct routes additionally
    // require the liquidator subaccount to clear its own buffered floor
    // (`validate_clears_buffered_floor`), because the liquidation moves the
    // liquidatee's position onto that subaccount. This route moves nothing
    // onto it (both `update_spot_balances_and_cumulative_deposits` calls in
    // `liquidate_spot_with_swap_end` target the liquidatee, and the fees go
    // to the revenue and protocol pools), so there is no exposure for a
    // per-subaccount floor to gate.
    let liquidator_stats = load!(ctx.accounts.liquidator_stats)?;
    require_liquidator_not_frozen(&liquidator_stats)?;

    let remaining_accounts_iter = &mut ctx.remaining_accounts.iter().peekable();
    let mut maps = load_maps(
        remaining_accounts_iter,
        &MarketSet::new(),
        &get_writable_spot_market_set_from_many(vec![asset_market_index, liability_market_index]),
        clock.slot,
        state.slot_clock(),
        Some(state.oracle_guard_rails),
    )?;

    let _token_interface = get_token_interface(remaining_accounts_iter)?;
    let mint = get_token_mint(remaining_accounts_iter)?;

    accrue_swap_market_interest(&mut maps, &state, &legs, now)?;
    validate_swap_not_paused(&maps, &state, &legs)?;

    validate_swap_request(&legs)?;

    liquidate_spot_with_swap_begin(
        LiquidateSpotSwapBeginRequest {
            asset_market_index,
            liability_market_index,
            swap_amount_in: swap_amount,
        },
        &mut LiquidationParties {
            user,
            user_key: &user_key,
            liquidator,
            liquidator_key: &liquidator_key,
        },
        &mut maps,
        &LiquidationTerms::from_state(&state, now, clock.slot),
        &state,
    )?;

    open_flash_loan(
        ctx.accounts,
        &mut maps,
        &legs,
        &mint,
        remaining_accounts_iter,
    )?;

    validate_swap_transaction(&ctx)
}

/// Book the lending interest of both markets, and prove neither one already
/// holds an open flash loan.
///
/// `None` for the oracle deliberately: this accrues interest and advances the
/// deposit, borrow and utilization TWAPs, but does NOT advance the markets'
/// *oracle* TWAPs. `liquidate_spot_with_swap_begin` gates itself on
/// `is_oracle_too_divergent_with_twap_5min` against the liability market's
/// `last_oracle_price_twap_5min`. Refreshing that anchor in the same
/// instruction pulls it toward the live oracle price and lets a liquidation
/// the band check would reject proceed and transfer collateral.
///
/// The direct `liquidate_spot` lane already runs that same check with no
/// pre-refresh, so this only brings the swap-backed lane in line with it. A
/// band-blocked swap liquidation can still be routed through the direct path.
/// The refresh is moved, not dropped: `liquidate_spot_with_swap_end` advances
/// both markets' oracle TWAPs once every check in the lane is done.
fn accrue_swap_market_interest(
    maps: &mut AccountMaps,
    state: &State,
    legs: &SwapLegs,
    now: i64,
) -> Result<()> {
    for market_index in [legs.asset_market_index, legs.liability_market_index] {
        let spot_market = &mut maps.spot_market_map.get_ref_mut(&market_index)?;

        validate!(
            spot_market.flash_loan_initial_token_amount == 0 && spot_market.flash_loan_amount == 0,
            ErrorCode::InvalidLiquidateSpotWithSwap,
            "begin_swap ended in invalid state"
        )?;

        controller::spot_balance::update_spot_market_cumulative_interest(
            spot_market,
            None,
            now,
            state.funding_paused()?,
        )?;
    }
    Ok(())
}

/// Refuse a swap that would move tokens a pause has stopped.
///
/// The swap sends asset-vault tokens out and pulls liability tokens in — the
/// same egress and ingress the direct spot withdraw and deposit paths gate.
/// `liq_not_paused` alone does not cover them, so mirror `end_swap`. Gating
/// the begin instruction is sufficient: a matching end instruction is required
/// in the same atomic transaction.
fn validate_swap_not_paused(maps: &AccountMaps, state: &State, legs: &SwapLegs) -> Result<()> {
    validate!(
        !(state.deposit_paused()? || state.withdraw_paused()?),
        ErrorCode::ExchangePaused
    )?;

    validate!(
        !maps
            .spot_market_map
            .get_ref(&legs.asset_market_index)?
            .is_operation_paused(SpotOperation::Withdraw),
        ErrorCode::MarketWithdrawPaused,
        "asset spot market {} withdraws paused",
        legs.asset_market_index
    )?;

    validate!(
        !maps
            .spot_market_map
            .get_ref(&legs.liability_market_index)?
            .is_operation_paused(SpotOperation::Deposit),
        ErrorCode::MarketActionPaused,
        "liability spot market {} deposits paused",
        legs.liability_market_index
    )?;

    Ok(())
}

/// A swap must move between two different markets, and must move something.
fn validate_swap_request(legs: &SwapLegs) -> Result<()> {
    validate!(
        legs.asset_market_index != legs.liability_market_index,
        ErrorCode::InvalidSwap,
        "asset and liability market the same"
    )?;

    validate!(
        legs.swap_amount != 0,
        ErrorCode::InvalidSwap,
        "swap_amount cannot be zero"
    )?;
    Ok(())
}

/// Record what the loan lent, then send the asset tokens to the swapper.
///
/// The initial token amounts are the baseline `end` measures the swap's
/// proceeds against.
fn open_flash_loan<'info>(
    accounts: &LiquidateSpotWithSwap<'info>,
    maps: &mut AccountMaps,
    legs: &SwapLegs,
    mint: &Option<InterfaceAccount<'info, Mint>>,
    remaining_accounts: &mut std::iter::Peekable<std::slice::Iter<'info, AccountInfo<'info>>>,
) -> Result<()> {
    let mut asset_spot_market = maps.spot_market_map.get_ref_mut(&legs.asset_market_index)?;
    let mut liability_spot_market = maps
        .spot_market_map
        .get_ref_mut(&legs.liability_market_index)?;

    asset_spot_market.flash_loan_amount = legs.swap_amount;
    asset_spot_market.flash_loan_initial_token_amount = accounts.asset_token_account.amount;
    liability_spot_market.flash_loan_initial_token_amount = accounts.liability_token_account.amount;

    validate!(
        !(asset_spot_market.has_transfer_hook() && liability_spot_market.has_transfer_hook()),
        ErrorCode::InvalidSwap,
        "both asset and liability spot markets cannot both have transfer hooks"
    )?;

    controller::token::send_from_program_vault(
        &accounts.token_program,
        &accounts.asset_spot_market_vault,
        &accounts.asset_token_account,
        &accounts.velocity_signer,
        accounts.state.load()?.signer_nonce,
        legs.swap_amount,
        mint,
        if asset_spot_market.has_transfer_hook() {
            Some(remaining_accounts)
        } else {
            None
        },
    )?;
    Ok(())
}

/// Prove the transaction closes the loan it just opened.
///
/// A matching `LiquidateSpotWithSwapEnd` must be the last velocity instruction
/// of the transaction, and it must carry the same accounts. Everything between
/// the two must belong to a whitelisted swap program, and anything after the
/// end must write nothing.
fn validate_swap_transaction<'info>(
    ctx: &Context<'info, LiquidateSpotWithSwap<'info>>,
) -> Result<()> {
    let ixs = ctx.accounts.instructions.as_ref();
    let current_index = instructions::load_current_index_checked(ixs)? as usize;

    let current_ix = instructions::load_instruction_at_checked(current_index, ixs)?;
    validate!(
        current_ix.program_id == *ctx.program_id,
        ErrorCode::InvalidLiquidateSpotWithSwap,
        "LiquidateSpotWithSwapBegin must be a top-level instruction (cant be cpi)"
    )?;

    let mut index = current_index + 1;
    let mut found_end = false;
    loop {
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
                ErrorCode::InvalidLiquidateSpotWithSwap,
                "the transaction must not contain a Velocity instruction after FlashLoanEnd"
            )?;
            found_end = true;
            validate_swap_end_ix(ctx, &ix)?;
        } else if found_end {
            for meta in ix.accounts.iter() {
                validate!(
                    !meta.is_writable,
                    ErrorCode::InvalidLiquidateSpotWithSwap,
                    "instructions after swap end must not have writable accounts"
                )?;
            }
        } else {
            validate_swap_middle_ix(&ix)?;
        }

        index += 1;
    }

    validate!(
        found_end,
        ErrorCode::InvalidLiquidateSpotWithSwap,
        "found no LiquidateSpotWithSwapEnd instruction in transaction"
    )?;

    Ok(())
}

/// Hold the end instruction to the accounts the begin instruction was given.
fn validate_swap_end_ix<'info>(
    ctx: &Context<'info, LiquidateSpotWithSwap<'info>>,
    ix: &solana_program::instruction::Instruction,
) -> Result<()> {
    // must be the SwapEnd instruction
    let discriminator = crate::instruction::LiquidateSpotWithSwapEnd::DISCRIMINATOR;
    validate!(
        &ix.data[0..8] == discriminator,
        ErrorCode::InvalidLiquidateSpotWithSwap,
        "last velocity ix must be end of swap"
    )?;

    let pinned = [
        (1, ctx.accounts.authority.key(), "authority"),
        (2, ctx.accounts.liquidator.key(), "liquidator"),
        (3, ctx.accounts.user.key(), "user"),
        (
            4,
            ctx.accounts.liability_spot_market_vault.key(),
            "liability_spot_market_vault",
        ),
        (
            5,
            ctx.accounts.asset_spot_market_vault.key(),
            "asset_spot_market_vault",
        ),
        (
            6,
            ctx.accounts.liability_token_account.key(),
            "liability_token_account",
        ),
        (
            7,
            ctx.accounts.asset_token_account.key(),
            "asset_token_account",
        ),
        (11, ctx.accounts.liquidator_stats.key(), "liquidator_stats"),
    ];
    for (position, key, name) in pinned {
        validate!(
            key == ix.accounts[position].pubkey,
            ErrorCode::InvalidLiquidateSpotWithSwap,
            "the {} passed to SwapBegin and End must match",
            name
        )?;
    }

    // `LiquidateSpotWithSwap` has 12 fixed accounts (indexes 0..=11);
    // remaining (swap) accounts start at index 12 and must match between
    // begin and end.
    validate!(
        ctx.remaining_accounts.len() == ix.accounts.len() - 12,
        ErrorCode::InvalidLiquidateSpotWithSwap,
        "begin and end ix must have the same number of accounts"
    )?;

    for i in 12..ix.accounts.len() {
        validate!(
            *ctx.remaining_accounts[i - 12].key == ix.accounts[i].pubkey,
            ErrorCode::InvalidLiquidateSpotWithSwap,
            "begin and end ix must have the same accounts. {}th account mismatch. begin: {}, end: {}",
            i,
            ctx.remaining_accounts[i - 12].key,
            ix.accounts[i].pubkey
        )?;
    }

    Ok(())
}

/// Allow only a whitelisted swap venue between begin and end.
fn validate_swap_middle_ix(ix: &solana_program::instruction::Instruction) -> Result<()> {
    let whitelisted_programs = [
        serum_program::id(),
        AssociatedToken::id(),
        jupiter_mainnet_3::ID,
        jupiter_mainnet_4::ID,
        jupiter_mainnet_6::ID,
        dflow_mainnet_aggregator_4::ID,
        titan_mainnet_argos_v1::ID,
    ];
    validate!(
        whitelisted_programs.contains(&ix.program_id),
        ErrorCode::InvalidLiquidateSpotWithSwap,
        "only allowed to pass in ixs to ATA, openbook, Jupiter v3/v4/v6, dflow, or titan programs"
    )?;

    for meta in ix.accounts.iter() {
        validate!(
            meta.pubkey != crate::id(),
            ErrorCode::InvalidLiquidateSpotWithSwap,
            "instructions between begin and end must not be velocity instructions"
        )?;
    }

    Ok(())
}

#[access_control(
    liq_not_paused(&ctx.accounts.state)
)]
pub fn handle_liquidate_spot_with_swap_end<'c: 'info, 'info>(
    ctx: Context<'info, LiquidateSpotWithSwap<'info>>,
    asset_market_index: u16,
    liability_market_index: u16,
) -> Result<()> {
    let clock = Clock::get()?;
    let now = clock.unix_timestamp;
    let legs = SwapLegs {
        asset_market_index,
        liability_market_index,
        swap_amount: 0,
    };

    let remaining_accounts = &mut ctx.remaining_accounts.iter().peekable();
    let (slot_clock, guard_rails) = {
        let state = ctx.accounts.state.load()?;
        (state.slot_clock(), state.oracle_guard_rails)
    };
    let mut maps = load_maps(
        remaining_accounts,
        &MarketSet::new(),
        &get_writable_spot_market_set_from_many(vec![asset_market_index, liability_market_index]),
        clock.slot,
        slot_clock,
        Some(guard_rails),
    )?;
    let tokens = SwapTokens {
        liability_token_program: get_token_interface(remaining_accounts)?,
        asset_mint: get_token_mint(remaining_accounts)?,
        liability_mint: get_token_mint(remaining_accounts)?,
    };

    let user_key = ctx.accounts.user.key();
    let liquidator_key = ctx.accounts.liquidator.key();

    // `State` is deliberately not held across this call. A `Ref` taken from
    // one field of the accounts struct blocks passing the struct itself, and
    // the legs need it whole.
    let (amount_in, amount_out) =
        close_flash_loan(ctx.accounts, &mut maps, &legs, &tokens, remaining_accounts)?;

    let state = ctx.accounts.state.load()?;
    let mut user = load_mut!(&ctx.accounts.user)?;
    liquidate_spot_with_swap_end(
        LiquidateSpotSwapEndRequest {
            asset_market_index,
            liability_market_index,
            asset_transfer: amount_in.cast()?,
            liability_transfer: amount_out.cast()?,
        },
        &mut user,
        &user_key,
        &liquidator_key,
        &mut maps,
        &LiquidationTerms::from_state(&state, now, clock.slot),
    )?;

    validate_swap_closed(
        &mut maps,
        &legs,
        [
            ctx.accounts.liability_spot_market_vault.amount,
            ctx.accounts.asset_spot_market_vault.amount,
        ],
    )?;
    advance_swap_oracle_twaps(&mut maps, &legs, now)
}

/// The mints and token programs the two legs move through.
struct SwapTokens<'info> {
    asset_mint: Option<InterfaceAccount<'info, Mint>>,
    liability_mint: Option<InterfaceAccount<'info, Mint>>,
    /// The liability market may run on a different token program than the
    /// asset market.
    liability_token_program: Option<Interface<'info, TokenInterface>>,
}

/// Close both sides of the loan. Reports how much of the loan the swap
/// consumed, and how much of the liability it repaid.
fn close_flash_loan<'info>(
    accounts: &mut LiquidateSpotWithSwap<'info>,
    maps: &mut AccountMaps,
    legs: &SwapLegs,
    tokens: &SwapTokens<'info>,
    remaining_accounts: &mut std::iter::Peekable<std::slice::Iter<'info, AccountInfo<'info>>>,
) -> Result<(u64, u64)> {
    let amount_in = {
        let mut spot_market = maps.spot_market_map.get_ref_mut(&legs.asset_market_index)?;
        close_asset_leg(
            SwapSide {
                vault: &mut accounts.asset_spot_market_vault,
                token_account: &mut accounts.asset_token_account,
                token_program: &accounts.token_program,
                authority: &accounts.authority,
                mint: &tokens.asset_mint,
            },
            &mut spot_market,
            remaining_accounts,
        )?
    };
    let amount_out = {
        let mut spot_market = maps
            .spot_market_map
            .get_ref_mut(&legs.liability_market_index)?;
        close_liability_leg(
            SwapSide {
                vault: &mut accounts.liability_spot_market_vault,
                token_account: &mut accounts.liability_token_account,
                token_program: tokens
                    .liability_token_program
                    .as_ref()
                    .unwrap_or(&accounts.token_program),
                authority: &accounts.authority,
                mint: &tokens.liability_mint,
            },
            &mut spot_market,
            remaining_accounts,
        )?
    };
    Ok((amount_in, amount_out))
}

/// One side of the flash loan: the vault it moves against, the caller's token
/// account, and how to move between them.
struct SwapSide<'a, 'info> {
    vault: &'a mut InterfaceAccount<'info, TokenAccount>,
    token_account: &'a mut InterfaceAccount<'info, TokenAccount>,
    token_program: &'a Interface<'info, TokenInterface>,
    authority: &'a Signer<'info>,
    mint: &'a Option<InterfaceAccount<'info, Mint>>,
}

/// Pull back whatever the swap did not spend, and close the asset side of the
/// loan. Reports how much of the loan the swap actually consumed.
fn close_asset_leg<'info>(
    side: SwapSide<'_, 'info>,
    spot_market: &mut SpotMarket,
    remaining_accounts: &mut std::iter::Peekable<std::slice::Iter<'info, AccountInfo<'info>>>,
) -> Result<u64> {
    validate!(
        spot_market.flash_loan_amount != 0,
        ErrorCode::InvalidSwap,
        "the asset_spot_market must have a flash loan amount set"
    )?;

    let mut amount_in = spot_market.flash_loan_amount;
    if side.token_account.amount > spot_market.flash_loan_initial_token_amount {
        let residual = side
            .token_account
            .amount
            .safe_sub(spot_market.flash_loan_initial_token_amount)?;

        controller::token::receive(
            side.token_program,
            side.token_account,
            side.vault,
            side.authority,
            residual,
            side.mint,
            if spot_market.has_transfer_hook() {
                Some(remaining_accounts)
            } else {
                None
            },
        )?;
        side.token_account.reload()?;
        side.vault.reload()?;

        amount_in = amount_in.safe_sub(residual)?;
    }

    spot_market.flash_loan_initial_token_amount = 0;
    spot_market.flash_loan_amount = 0;
    Ok(amount_in)
}

/// Bank what the swap bought, and close the liability side of the loan.
/// Reports how much of the liability the swap repaid.
fn close_liability_leg<'info>(
    side: SwapSide<'_, 'info>,
    spot_market: &mut SpotMarket,
    remaining_accounts: &mut std::iter::Peekable<std::slice::Iter<'info, AccountInfo<'info>>>,
) -> Result<u64> {
    let mut amount_out = 0_u64;
    if side.token_account.amount > spot_market.flash_loan_initial_token_amount {
        amount_out = side
            .token_account
            .amount
            .safe_sub(spot_market.flash_loan_initial_token_amount)?;

        controller::token::receive(
            side.token_program,
            side.token_account,
            side.vault,
            side.authority,
            amount_out,
            side.mint,
            if spot_market.has_transfer_hook() {
                Some(remaining_accounts)
            } else {
                None
            },
        )?;

        side.vault.reload()?;
    }

    validate!(
        amount_out != 0,
        ErrorCode::InvalidSwap,
        "amount_out must be greater than 0"
    )?;

    spot_market.flash_loan_initial_token_amount = 0;
    spot_market.flash_loan_amount = 0;
    Ok(amount_out)
}

/// Prove both markets came out of the loan clean, and that each vault still
/// covers what its depositors are owed.
/// `vault_amounts` are the liability and the asset vault balances, in that
/// order.
fn validate_swap_closed(
    maps: &mut AccountMaps,
    legs: &SwapLegs,
    vault_amounts: [u64; 2],
) -> Result<()> {
    let markets = [legs.liability_market_index, legs.asset_market_index];
    for (market_index, vault_amount) in std::iter::zip(markets, vault_amounts) {
        let spot_market = maps.spot_market_map.get_ref(&market_index)?;
        validate!(
            spot_market.flash_loan_initial_token_amount == 0 && spot_market.flash_loan_amount == 0,
            ErrorCode::InvalidSwap,
            "end_swap ended in invalid state"
        )?;
        math::spot_withdraw::validate_spot_market_vault_amount(&spot_market, vault_amount)?;
    }
    Ok(())
}

/// Advance both markets' oracle TWAPs, last.
///
/// The begin instruction passes `None` so it cannot refresh the anchor its own
/// divergence check reads, and both this lane's checks are done by here. The
/// begin instruction left `last_oracle_price_twap_ts` alone, so this update
/// still weights the full elapsed interval.
fn advance_swap_oracle_twaps(maps: &mut AccountMaps, legs: &SwapLegs, now: i64) -> Result<()> {
    for market_index in [legs.asset_market_index, legs.liability_market_index] {
        let mut spot_market = maps.spot_market_map.get_ref_mut(&market_index)?;
        let oracle_data = *maps.oracle_map.get_price_data(&spot_market.oracle_id())?;
        controller::spot_balance::update_spot_market_twap_stats(
            &mut spot_market,
            Some(&oracle_data),
            now,
        )?;
    }
    Ok(())
}

#[derive(Accounts)]
#[instruction(asset_market_index: u16, liability_market_index: u16, )]
pub struct LiquidateSpotWithSwap<'info> {
    pub state: AccountLoader<'info, State>,
    pub authority: Signer<'info>,
    #[account(
        mut,
        constraint = can_sign_for_user(&liquidator, &authority)?
    )]
    pub liquidator: AccountLoader<'info, User>,
    #[account(mut)]
    pub user: AccountLoader<'info, User>,
    #[account(
        mut,
        seeds = [b"spot_market_vault".as_ref(), liability_market_index.to_le_bytes().as_ref()],
        bump,
    )]
    pub liability_spot_market_vault: Box<InterfaceAccount<'info, TokenAccount>>,
    #[account(
        mut,
        seeds = [b"spot_market_vault".as_ref(), asset_market_index.to_le_bytes().as_ref()],
        bump,
    )]
    pub asset_spot_market_vault: Box<InterfaceAccount<'info, TokenAccount>>,
    #[account(
        mut,
        constraint = &liability_spot_market_vault.mint.eq(&liability_token_account.mint),
        token::authority = authority
    )]
    pub liability_token_account: Box<InterfaceAccount<'info, TokenAccount>>,
    #[account(
        mut,
        constraint = &asset_spot_market_vault.mint.eq(&asset_token_account.mint),
        token::authority = authority
    )]
    pub asset_token_account: Box<InterfaceAccount<'info, TokenAccount>>,
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
    /// The liquidator's `UserStats`, read by `begin` to bar an authority whose
    /// equity breaker is tripped.
    ///
    /// It sits last, not beside `liquidator` where the direct liquidation
    /// contexts carry it, because this pair is addressed by position rather
    /// than by name: `begin` introspects the matching `end` and compares the
    /// two account lists index by index, and the swap accounts both forward
    /// begin where this fixed block ends. Taking the last slot renumbered
    /// nothing. Slotting it beside `liquidator` would have moved `user`, both
    /// vaults and both token accounts down one, silently invalidating every
    /// hand-built transaction that still filled the old order.
    #[account(
        constraint = is_stats_for_user(&liquidator, &liquidator_stats)?
    )]
    pub liquidator_stats: AccountLoader<'info, UserStats>,
}
