//! Resolving a deficit or a bankruptcy against the insurance fund.
//!
//! All three resolvers run the same frame: sweep the market's revenue pool
//! into the fund, value what is left unfunded, then pay that claim out of the
//! fund. [`InsuranceVaults`] holds the token half of that frame.

use super::*;

/// The vaults an insurance-fund repair moves tokens between.
struct InsuranceVaults<'a, 'info> {
    spot_market_vault: &'a mut Box<InterfaceAccount<'info, TokenAccount>>,
    insurance_fund_vault: &'a mut Box<InterfaceAccount<'info, TokenAccount>>,
    velocity_signer: &'a UncheckedAccount<'info>,
    token_program: &'a Interface<'info, TokenInterface>,
}

/// What a resolver asks the insurance fund to pay.
struct InsuranceClaim {
    amount: u64,
    /// The fund's balance before the payment. The outflow record is measured
    /// against it.
    fund_balance_before: u64,
    /// Whether to prove the fund keeps a balance before the payment is sent.
    /// The token transfer fails on a shortfall either way. The perp resolvers
    /// check first, so the failure names the fund rather than the vault.
    prove_headroom: bool,
}

impl<'info> InsuranceVaults<'_, 'info> {
    /// Sweep the market's revenue pool into the insurance fund, then reload
    /// both vaults so the resolver reads the balances that resulted.
    fn presettle_revenue(
        &mut self,
        spot_market: &mut SpotMarket,
        state: &State,
        mint: &Option<InterfaceAccount<'info, Mint>>,
        hook_accounts: &std::iter::Peekable<std::slice::Iter<'info, AccountInfo<'info>>>,
        now: i64,
    ) -> Result<()> {
        let mut hook_iter = hook_accounts.clone();
        controller::insurance::attempt_settle_revenue_to_insurance_fund(
            self.spot_market_vault,
            self.insurance_fund_vault,
            spot_market,
            now,
            self.token_program,
            self.velocity_signer,
            state,
            mint,
            if spot_market.has_transfer_hook() {
                Some(&mut hook_iter)
            } else {
                None
            },
        )?;

        // reload the spot market vault balance so it's up-to-date
        self.spot_market_vault.reload()?;
        self.insurance_fund_vault.reload()?;
        math::spot_withdraw::validate_spot_market_vault_amount(
            spot_market,
            self.spot_market_vault.amount,
        )?;
        Ok(())
    }

    /// Move `amount` out of the insurance fund into the market's vault.
    ///
    /// The fund must keep a positive balance. A fully drained fund reads as an
    /// uninitialized one everywhere else.
    fn pay_out(
        &mut self,
        spot_market: &SpotMarket,
        signer_nonce: u8,
        amount: u64,
        mint: &Option<InterfaceAccount<'info, Mint>>,
        hook_accounts: &std::iter::Peekable<std::slice::Iter<'info, AccountInfo<'info>>>,
    ) -> Result<()> {
        let mut hook_iter = hook_accounts.clone();
        controller::token::send_from_program_vault(
            self.token_program,
            self.insurance_fund_vault,
            self.spot_market_vault,
            self.velocity_signer,
            signer_nonce,
            amount,
            mint,
            if spot_market.has_transfer_hook() {
                Some(&mut hook_iter)
            } else {
                None
            },
        )?;

        validate!(
            self.insurance_fund_vault.amount > 0,
            ErrorCode::InvalidIFDetected,
            "insurance_fund_vault.amount must remain > 0"
        )?;
        Ok(())
    }

    /// Pay a resolver's claim, record the outflow, and re-prove the market's
    /// vault still covers what its depositors are owed.
    fn settle_claim(
        &mut self,
        spot_market: &mut SpotMarket,
        state: &State,
        claim: InsuranceClaim,
        mint: &Option<InterfaceAccount<'info, Mint>>,
        hook_accounts: &std::iter::Peekable<std::slice::Iter<'info, AccountInfo<'info>>>,
    ) -> Result<()> {
        if claim.amount > 0 {
            if claim.prove_headroom {
                validate!(
                    claim.amount < self.insurance_fund_vault.amount,
                    ErrorCode::InsufficientCollateral,
                    "Insurance Fund balance InsufficientCollateral for payment: !{} < {}",
                    claim.amount,
                    self.insurance_fund_vault.amount
                )?;
            }
            self.pay_out(
                spot_market,
                state.signer_nonce,
                claim.amount,
                mint,
                hook_accounts,
            )?;
        }

        controller::insurance::record_insurance_fund_outflow(
            spot_market,
            claim.fund_balance_before,
            claim.amount,
        );
        // reload the spot market vault balance so it's up-to-date
        self.spot_market_vault.reload()?;
        math::spot_withdraw::validate_spot_market_vault_amount(
            spot_market,
            self.spot_market_vault.amount,
        )?;
        Ok(())
    }
}

/// The estate and the liquidator, proven distinct.
fn distinct_parties(
    user: &AccountLoader<'_, User>,
    liquidator: &AccountLoader<'_, User>,
) -> Result<(Pubkey, Pubkey)> {
    let user_key = user.key();
    let liquidator_key = liquidator.key();
    validate!(
        user_key != liquidator_key,
        ErrorCode::UserCantLiquidateThemself
    )?;
    Ok((user_key, liquidator_key))
}

/// The perp markets a bankruptcy resolution writes to.
///
/// The resolver forfeits unfundable claims to their own markets' insurance
/// tranches, so every market that holds such a claim is written, not only the
/// one being resolved. Declaring them at load makes a caller that passes one
/// read-only fail with `MarketWrongMutability` instead of failing deep inside
/// the resolver.
fn forfeitable_claim_markets(user: &User, resolved: Option<u16>) -> Vec<u16> {
    let mut markets: Vec<u16> = resolved.into_iter().collect();
    markets.extend(perp_markets_with_forfeitable_claims(user));
    markets
}

/// The two markets one deficit resolution spans.
struct DeficitMarkets {
    spot: u16,
    perp: u16,
}

/// What the vaults hold when a resolver runs.
struct VaultAmounts {
    spot_market: u64,
    insurance_fund: u64,
}

#[access_control(
    solvency_repair_not_paused(&ctx.accounts.state)
)]
pub fn handle_resolve_perp_pnl_deficit<'c: 'info, 'info>(
    ctx: Context<'info, ResolvePerpPnlDeficit<'info>>,
    spot_market_index: u16,
    perp_market_index: u16,
) -> Result<()> {
    let clock = Clock::get()?;
    let now = clock.unix_timestamp;

    validate!(spot_market_index == 0, ErrorCode::InvalidSpotMarketAccount)?;
    let state = ctx.accounts.state.load()?;

    let remaining_accounts_iter = &mut ctx.remaining_accounts.iter().peekable();
    let mut maps = load_maps(
        remaining_accounts_iter,
        &get_writable_perp_market_set(perp_market_index),
        &get_writable_spot_market_set(spot_market_index),
        clock.slot,
        state.slot_clock(),
        Some(state.oracle_guard_rails),
    )?;

    let mint = get_token_mint(remaining_accounts_iter)?;

    // No `update_amm` here: this handler moves spot/IF balances and does
    // not read perp AMM peg or reserves. Refreshing the AMM was cargo-cult.

    let mut vaults = InsuranceVaults {
        spot_market_vault: &mut ctx.accounts.spot_market_vault,
        insurance_fund_vault: &mut ctx.accounts.insurance_fund_vault,
        velocity_signer: &ctx.accounts.velocity_signer,
        token_program: &ctx.accounts.token_program,
    };
    {
        let spot_market = &mut maps.spot_market_map.get_ref_mut(&spot_market_index)?;
        vaults.presettle_revenue(spot_market, &state, &mint, remaining_accounts_iter, now)?;
    }

    let balances = VaultAmounts {
        spot_market: vaults.spot_market_vault.amount,
        insurance_fund: vaults.insurance_fund_vault.amount,
    };
    let markets = DeficitMarkets {
        spot: spot_market_index,
        perp: perp_market_index,
    };
    let pay_from_insurance = resolve_perp_deficit(&mut maps, &state, markets, &balances, now)?;

    if pay_from_insurance > 0 {
        validate!(
            pay_from_insurance < vaults.insurance_fund_vault.amount,
            ErrorCode::InsufficientCollateral,
            "Insurance Fund balance InsufficientCollateral for payment: !{} < {}",
            pay_from_insurance,
            vaults.insurance_fund_vault.amount
        )?;

        let spot_market = &mut maps.spot_market_map.get_ref_mut(&spot_market_index)?;
        vaults.pay_out(
            spot_market,
            state.signer_nonce,
            pay_from_insurance,
            &mint,
            remaining_accounts_iter,
        )?;
        controller::insurance::record_insurance_fund_outflow(
            spot_market,
            balances.insurance_fund,
            pay_from_insurance,
        );
    }

    // todo: validate amounts transfered and spot_market before and after are zero-sum

    Ok(())
}

/// Value the perp market's deficit and decide what the insurance fund owes it.
///
/// The oracle drives a value transfer here, so it is gated before it is used.
/// A curve-update market must have validated its oracle in this slot, and the
/// sample the AMM validated must still be the sample being read: a later
/// oracle write in the same slot replaces the sample without touching
/// `last_oracle_valid`.
fn resolve_perp_deficit(
    maps: &mut AccountMaps,
    state: &State,
    markets: DeficitMarkets,
    balances: &VaultAmounts,
    now: i64,
) -> Result<u64> {
    let spot_market = &mut maps.spot_market_map.get_ref_mut(&markets.spot)?;
    let perp_market = &mut maps.perp_market_map.get_ref_mut(&markets.perp)?;

    let oracle_price_data = *maps.oracle_map.get_price_data(&perp_market.oracle_id())?;

    if perp_market.amm.is_curve_update_enabled() {
        validate!(
            perp_market.market_stats.last_oracle_valid,
            ErrorCode::InvalidOracle,
            "Oracle Price detected as invalid"
        )?;

        validate!(
            perp_market.amm.is_fresh_at(maps.oracle_map.slot),
            ErrorCode::AMMNotUpdatedInSameSlot,
            "AMM must be updated in a prior instruction within same slot"
        )?;

        validate!(
            perp_market.is_validated_oracle_sample(&oracle_price_data),
            ErrorCode::InvalidOracle,
            "Oracle rewritten after same-slot AMM update; sample no longer matches the validated one"
        )?;
    }

    validate!(
        !perp_market.is_in_settlement(now),
        ErrorCode::MarketActionPaused,
        "Market is in settlement mode",
    )?;

    controller::orders::validate_market_within_price_band(
        perp_market,
        state,
        oracle_price_data.price,
    )?;

    let pay_from_insurance = controller::insurance::resolve_perp_pnl_deficit(
        balances.spot_market,
        balances.insurance_fund,
        spot_market,
        perp_market,
        now,
        state.funding_paused()?,
    )?;
    Ok(pay_from_insurance)
}

#[access_control(
    solvency_repair_not_paused(&ctx.accounts.state)
)]
pub fn handle_resolve_perp_bankruptcy<'c: 'info, 'info>(
    ctx: Context<'info, ResolveBankruptcy<'info>>,
    quote_spot_market_index: u16,
    market_index: u16,
) -> Result<()> {
    let clock = Clock::get()?;
    let now = clock.unix_timestamp;

    let (user_key, liquidator_key) =
        distinct_parties(&ctx.accounts.user, &ctx.accounts.liquidator)?;

    validate!(
        quote_spot_market_index == QUOTE_SPOT_MARKET_INDEX,
        ErrorCode::InvalidSpotMarketAccount
    )?;

    let user = &mut load_mut!(ctx.accounts.user)?;
    let liquidator = &mut load_mut!(ctx.accounts.liquidator)?;
    let state = ctx.accounts.state.load()?;

    let remaining_accounts_iter = &mut ctx.remaining_accounts.iter().peekable();
    let mut maps = load_maps(
        remaining_accounts_iter,
        &get_writable_perp_market_set_from_vec(&forfeitable_claim_markets(
            user,
            Some(market_index),
        )),
        &get_writable_spot_market_set(quote_spot_market_index),
        clock.slot,
        state.slot_clock(),
        Some(state.oracle_guard_rails),
    )?;

    let mint = get_token_mint(remaining_accounts_iter)?;

    let mut vaults = InsuranceVaults {
        spot_market_vault: &mut ctx.accounts.spot_market_vault,
        insurance_fund_vault: &mut ctx.accounts.insurance_fund_vault,
        velocity_signer: &ctx.accounts.velocity_signer,
        token_program: &ctx.accounts.token_program,
    };
    {
        let spot_market = &mut maps.spot_market_map.get_ref_mut(&quote_spot_market_index)?;
        vaults.presettle_revenue(spot_market, &state, &mint, remaining_accounts_iter, now)?;
    }

    let fund_balance_before = vaults.insurance_fund_vault.amount;
    let pay_from_insurance = controller::liquidation::resolve_perp_bankruptcy(
        market_index,
        user,
        &user_key,
        liquidator,
        &liquidator_key,
        &mut maps,
        now,
        fund_balance_before,
        state.funding_paused()?,
    )?;

    let spot_market = &mut maps.spot_market_map.get_ref_mut(&quote_spot_market_index)?;
    vaults.settle_claim(
        spot_market,
        &state,
        InsuranceClaim {
            amount: pay_from_insurance,
            fund_balance_before,
            prove_headroom: true,
        },
        &mint,
        remaining_accounts_iter,
    )
}

#[access_control(
    solvency_repair_not_paused(&ctx.accounts.state)
)]
pub fn handle_resolve_spot_bankruptcy<'c: 'info, 'info>(
    ctx: Context<'info, ResolveBankruptcy<'info>>,
    market_index: u16,
) -> Result<()> {
    let state = ctx.accounts.state.load()?;
    let clock = Clock::get()?;
    let now = clock.unix_timestamp;

    let (user_key, liquidator_key) =
        distinct_parties(&ctx.accounts.user, &ctx.accounts.liquidator)?;

    let user = &mut load_mut!(ctx.accounts.user)?;
    let liquidator = &mut load_mut!(ctx.accounts.liquidator)?;

    let remaining_accounts_iter = &mut ctx.remaining_accounts.iter().peekable();
    let mut maps = load_maps(
        remaining_accounts_iter,
        // This resolver also recovers and winds up the estate's perp claims,
        // so the markets holding them are written to even though the
        // bankruptcy being resolved is a spot borrow.
        &get_writable_perp_market_set_from_vec(&forfeitable_claim_markets(user, None)),
        // The quote market is written too: a recovered claim lands in the estate's quote deposit,
        // and the borrow being resolved may be in another market entirely. It was already a required
        // account here, because the claim passes read it, but only as read-only.
        &get_writable_spot_market_set_from_many(vec![market_index, QUOTE_SPOT_MARKET_INDEX]),
        clock.slot,
        state.slot_clock(),
        Some(state.oracle_guard_rails),
    )?;

    let mint = get_token_mint(remaining_accounts_iter)?;

    let mut vaults = InsuranceVaults {
        spot_market_vault: &mut ctx.accounts.spot_market_vault,
        insurance_fund_vault: &mut ctx.accounts.insurance_fund_vault,
        velocity_signer: &ctx.accounts.velocity_signer,
        token_program: &ctx.accounts.token_program,
    };
    {
        let spot_market = &mut maps.spot_market_map.get_ref_mut(&market_index)?;
        vaults.presettle_revenue(spot_market, &state, &mint, remaining_accounts_iter, now)?;
    }

    let fund_balance_before = vaults.insurance_fund_vault.amount;
    let pay_from_insurance = controller::liquidation::resolve_spot_bankruptcy(
        market_index,
        user,
        &user_key,
        liquidator,
        &liquidator_key,
        &mut maps,
        now,
        fund_balance_before,
        state.funding_paused()?,
    )?;

    let spot_market = &mut maps.spot_market_map.get_ref_mut(&market_index)?;
    vaults.settle_claim(
        spot_market,
        &state,
        InsuranceClaim {
            amount: pay_from_insurance,
            fund_balance_before,
            prove_headroom: false,
        },
        &mint,
        remaining_accounts_iter,
    )
}

#[derive(Accounts)]
#[instruction(spot_market_index: u16,)]
pub struct ResolveBankruptcy<'info> {
    pub state: AccountLoader<'info, State>,
    pub authority: Signer<'info>,
    #[account(
        mut,
        constraint = can_sign_for_user(&liquidator, &authority)?
    )]
    pub liquidator: AccountLoader<'info, User>,
    #[account(
        mut,
        constraint = is_stats_for_user(&liquidator, &liquidator_stats)?
    )]
    pub liquidator_stats: AccountLoader<'info, UserStats>,
    #[account(mut)]
    pub user: AccountLoader<'info, User>,
    #[account(
        mut,
        constraint = is_stats_for_user(&user, &user_stats)?
    )]
    pub user_stats: AccountLoader<'info, UserStats>,
    #[account(
        mut,
        seeds = [b"spot_market_vault".as_ref(), spot_market_index.to_le_bytes().as_ref()],
        bump,
    )]
    pub spot_market_vault: Box<InterfaceAccount<'info, TokenAccount>>,
    #[account(
        mut,
        seeds = [b"insurance_fund_vault".as_ref(), spot_market_index.to_le_bytes().as_ref()], // todo: market_index=0 hardcode for perps?
        bump,
    )]
    pub insurance_fund_vault: Box<InterfaceAccount<'info, TokenAccount>>,
    #[account(
        constraint = state.load()?.signer.eq(&velocity_signer.key())
    )]
    /// CHECK: forced velocity_signer
    pub velocity_signer: UncheckedAccount<'info>,
    pub token_program: Interface<'info, TokenInterface>,
}

#[derive(Accounts)]
#[instruction(spot_market_index: u16,)]
pub struct ResolvePerpPnlDeficit<'info> {
    pub state: AccountLoader<'info, State>,
    pub authority: Signer<'info>,
    #[account(
        mut,
        seeds = [b"spot_market_vault".as_ref(), spot_market_index.to_le_bytes().as_ref()],
        bump,
    )]
    pub spot_market_vault: Box<InterfaceAccount<'info, TokenAccount>>,
    #[account(
        mut,
        seeds = [b"insurance_fund_vault".as_ref(), spot_market_index.to_le_bytes().as_ref()], // todo: market_index=0 hardcode for perps?
        bump,
    )]
    pub insurance_fund_vault: Box<InterfaceAccount<'info, TokenAccount>>,
    #[account(
        constraint = state.load()?.signer.eq(&velocity_signer.key())
    )]
    /// CHECK: forced velocity_signer
    pub velocity_signer: UncheckedAccount<'info>,
    pub token_program: Interface<'info, TokenInterface>,
}
