//! Paying or writing off a builder or referrer row.
//!
//! A row is normally paid by the pnl settle of the account that owes it, so it
//! is collectable only while that account still settles. These two
//! instructions let a beneficiary or a keeper finish a row without the owner,
//! and let the protocol end a row nobody can collect.

use super::*;

/// Load the escrow the seeds pin to `escrow_authority`, and prove its header
/// names the same authority.
///
/// The caller owns the `AccountInfo`, because the loader borrows from it.
fn load_escrow_for_authority<'a, 'info>(
    account_info: &'a AccountInfo<'info>,
    escrow_authority: &Pubkey,
) -> Result<RevenueShareEscrowZeroCopyMut<'a>> {
    // Fully qualified: `ZeroCopyLoader` also defines `load_zc_mut` for account infos.
    let escrow: RevenueShareEscrowZeroCopyMut =
        crate::state::revenue_share::RevenueShareEscrowLoader::load_zc_mut(account_info)?;
    validate!(
        escrow.fixed.authority == *escrow_authority,
        ErrorCode::RevenueShareEscrowAuthorityMismatch,
        "escrow header authority {} does not match the seed authority {}",
        escrow.fixed.authority,
        escrow_authority
    )?;
    Ok(escrow)
}

/// Writes off one revenue-share row that the program cannot pay. Anyone can call this.
///
/// `settle_expired_market_pools_to_revenue_pool` refuses to delist a market that still owes
/// revenue share. That value belongs to the beneficiaries, not to the revenue pool. A row that
/// nobody can collect would block the delist forever. This instruction ends such a row.
///
/// The program requires proof that it cannot pay the row. One of these must be true:
///
///   * The beneficiary has no payout `User` account. The handler derives the address of that
///     account from the row, so the caller cannot substitute or omit it.
///   * The market is closed and the pool is smaller than the row.
///   * The row names no beneficiary that the program can reach.
///
/// This moves no tokens. Only the row and the counter change.
#[access_control(
    exchange_not_paused(&ctx.accounts.state)
)]
pub fn handle_forfeit_revenue_share_order(
    ctx: Context<ForfeitRevenueShareOrder>,
    args: ForfeitRevenueShareOrderArgs,
) -> Result<()> {
    let ForfeitRevenueShareOrderArgs {
        market_index,
        order_index,
    } = args;
    let clock = Clock::get()?;
    let state = ctx.accounts.state.load()?;
    let perp_market = &mut load_mut!(ctx.accounts.perp_market)?;
    let spot_market = &mut load_mut!(ctx.accounts.spot_market)?;
    let escrow_authority = ctx.accounts.escrow_authority.key();

    // The proof below compares the pool against the amount that the row owes. `get_token_amount`
    // scales the pool by the cumulative deposit interest of the market. That interest only grows.
    // An old value therefore makes the pool look too small, and the program could write off a row
    // that it can pay. Accrue the interest first. The sweep does the same.
    controller::spot_balance::update_spot_market_cumulative_interest(
        spot_market,
        None,
        clock.unix_timestamp,
        state.funding_paused()?,
    )?;

    // Bind the account info to a local first. The loader borrows from it, and a temporary
    // value from `to_account_info()` does not live long enough.
    let escrow_account_info = ctx.accounts.revenue_share_escrow.to_account_info();
    let mut escrow = load_escrow_for_authority(&escrow_account_info, &escrow_authority)?;

    // The proof that the program cannot pay the row. It reads the beneficiary from the escrow and
    // derives the payout address itself, so the caller cannot substitute or omit that account.
    let reason = controller::revenue_share::resolve_revenue_share_forfeit_reason(
        perp_market,
        spot_market,
        &mut escrow,
        market_index,
        order_index,
        &ctx.accounts.beneficiary_user.key(),
        ctx.accounts.beneficiary_user.data_is_empty()
            && *ctx.accounts.beneficiary_user.owner == anchor_lang::system_program::ID,
        clock.unix_timestamp,
        state.escrow_period_before_transfer()?,
    )?;

    controller::revenue_share::forfeit_revenue_share_order(
        perp_market,
        &mut escrow,
        order_index,
        reason,
    )?;

    Ok(())
}

/// Pays the accrued builder and referrer fees in one escrow for one perp market. Anyone can call
/// this.
///
/// A pnl settle runs the same sweep, but only after it settles pnl. A row is therefore payable
/// only while the escrow owner still has pnl to settle on the market. After the owner closes the
/// position and stops trading, nobody can collect the fee. `PerpMarket.pending_revenue_share` then
/// holds pnl-pool value against that claim forever, and the fee sweeps cannot use it. This
/// instruction lets a beneficiary or a keeper collect without the owner.
///
/// `remaining_accounts` holds three groups, in this order:
///   1. The oracle, spot market and perp market accounts that `load_maps` reads.
///   2. `num_owner_sub_accounts` read-only `User` accounts of the escrow authority. The handler
///      uses them to complete rows whose orders are closed. A builder row needs `Completed`.
///   3. The `User` and `RevenueShare` accounts of the beneficiaries, writable. These go to
///      `load_revenue_share_map`.
#[access_control(
    exchange_not_paused(&ctx.accounts.state)
    settle_pnl_not_paused(&ctx.accounts.state)
)]
pub fn handle_settle_revenue_share<'c: 'info, 'info>(
    ctx: Context<'info, SettleRevenueShare<'info>>,
    args: SettleRevenueShareArgs,
) -> Result<()> {
    let SettleRevenueShareArgs {
        market_index,
        num_owner_sub_accounts,
    } = args;
    let clock = Clock::get()?;
    let state = ctx.accounts.state.load()?;

    validate!(
        state.builder_codes_enabled(),
        ErrorCode::DefaultError,
        "builder codes feature is disabled"
    )?;

    let escrow_authority = ctx.accounts.escrow_authority.key();

    // Bind the account info to a local first. The loader borrows from it, and a temporary
    // value from `to_account_info()` does not live long enough.
    let escrow_account_info = ctx.accounts.revenue_share_escrow.to_account_info();
    let mut escrow = load_escrow_for_authority(&escrow_account_info, &escrow_authority)?;

    let mut remaining_accounts = ctx.remaining_accounts.iter().peekable();
    let mut maps = load_maps(
        &mut remaining_accounts,
        &get_writable_perp_market_set(market_index),
        &get_writable_spot_market_set(QUOTE_SPOT_MARKET_INDEX),
        clock.slot,
        state.slot_clock(),
        Some(state.oracle_guard_rails),
    )?;

    // The sweep pays from the market that `get_quote_spot_market_mut` returns. A perp market with
    // a different quote market would find no balance.
    validate!(
        maps.perp_market_map
            .get_ref(&market_index)?
            .quote_spot_market_index
            == QUOTE_SPOT_MARKET_INDEX,
        ErrorCode::DefaultError,
        "perp market {} is not quoted in the quote spot market",
        market_index
    )?;

    complete_owner_rows(
        &mut remaining_accounts,
        &mut escrow,
        &escrow_authority,
        num_owner_sub_accounts,
    )?;

    // This uses `?`, not `.ok()`. The settle handlers process a batch and must continue. This
    // instruction has one job, so a bad beneficiary account must fail the transaction.
    let revenue_share_map = load_revenue_share_map(&mut remaining_accounts)?;

    let reserve_price = sweep_reserve_price(&mut maps, &state, market_index)?;

    let discharged = controller::revenue_share::sweep_completed_revenue_share_for_market(
        market_index,
        &mut escrow,
        &maps.perp_market_map,
        &maps.spot_market_map,
        &revenue_share_map,
        clock.unix_timestamp,
        reserve_price,
        state.builder_codes_enabled(),
        state.funding_paused()?,
    )?;

    msg!(
        "settled revenue share for market {} escrow {}: {}",
        market_index,
        escrow_authority,
        discharged
    );

    let spot_market = maps.spot_market_map.get_quote_spot_market()?;
    validate_spot_market_vault_amount(&spot_market, ctx.accounts.spot_market_vault.amount)?;

    Ok(())
}

/// Complete the rows whose orders are closed.
///
/// This is the only way that a builder row gets the `Completed` flag that the
/// sweep needs. Payment clears a row. If the program paid an `Open` row, it
/// would remove the `order_id` and `sub_account_id` that
/// `find_builder_order_index` reads. A third party could then stop the fees of
/// a builder on a live order.
fn complete_owner_rows<'a: 'b, 'b>(
    remaining_accounts: &mut std::iter::Peekable<std::slice::Iter<'a, AccountInfo<'b>>>,
    escrow: &mut RevenueShareEscrowZeroCopyMut<'_>,
    escrow_authority: &Pubkey,
    num_owner_sub_accounts: u8,
) -> Result<()> {
    let owner_sub_accounts = load_escrow_owner_sub_accounts(
        remaining_accounts,
        escrow_authority,
        num_owner_sub_accounts,
    )?;
    for loader in owner_sub_accounts.iter() {
        let user = load!(loader)?;
        escrow.revoke_completed_orders(&user)?;
    }
    Ok(())
}

/// The price that sets the `max(net_user_pnl, 0)` reserve.
///
/// The settle handlers get this check from the `settle_pnl` that runs before
/// their sweep. This instruction must do the check itself, and it uses the
/// same gate as `handle_sweep_perp_market_fees`, which values the same
/// reserve.
fn sweep_reserve_price(maps: &mut AccountMaps, state: &State, market_index: u16) -> Result<i64> {
    let perp_market = maps.perp_market_map.get_ref(&market_index)?;

    // A delist requires a zero liability and moves the pnl pool to the revenue pool. A
    // delisted market therefore owes nothing and holds nothing. Report this. A silent success
    // would look like a completed settle.
    validate!(
        perp_market.status != MarketStatus::Delisted,
        ErrorCode::MarketDelisted,
        "perp market {} is delisted; its pnl pool is drained and it owes nothing",
        market_index
    )?;

    controller::perp_pools::get_pnl_pool_drain_reserve_price(
        &perp_market,
        state,
        &mut maps.oracle_map,
    )
    .map_err(Into::into)
}

#[derive(Clone, Copy, AnchorSerialize, AnchorDeserialize)]
pub struct ForfeitRevenueShareOrderArgs {
    pub market_index: u16,
    /// Index of the escrow order row to forfeit.
    pub order_index: u32,
}

#[derive(Accounts)]
#[instruction(args: ForfeitRevenueShareOrderArgs)]
pub struct ForfeitRevenueShareOrder<'info> {
    pub state: AccountLoader<'info, State>,
    #[account(
        mut,
        seeds = [b"perp_market", args.market_index.to_le_bytes().as_ref()],
        bump,
    )]
    pub perp_market: AccountLoader<'info, PerpMarket>,
    /// The quote spot market of the perp market. The PDA seeds enforce this. The handler values
    /// the pnl pool against it. It is writable because the handler accrues interest first.
    #[account(
        mut,
        seeds = [b"spot_market", perp_market.load()?.quote_spot_market_index.to_le_bytes().as_ref()],
        bump
    )]
    pub spot_market: AccountLoader<'info, SpotMarket>,
    /// The owner of the escrow that holds the row.
    /// CHECK: the PDA seeds below bind this key to the escrow. The handler also compares it with the authority in the escrow header.
    pub escrow_authority: UncheckedAccount<'info>,
    /// The escrow that holds the row to write off.
    /// CHECK: `load_zc_mut` reads this account and validates the owner and the discriminator. The seeds fix the address.
    #[account(
        mut,
        seeds = [REVENUE_SHARE_ESCROW_PDA_SEED.as_bytes(), escrow_authority.key().as_ref()],
        bump,
    )]
    pub revenue_share_escrow: UncheckedAccount<'info>,
    /// Sub-account 0 of the beneficiary of the row. This is the payout account. The handler proves
    /// that it does not exist.
    /// CHECK: the handler derives the required address from the beneficiary of the row and rejects any other address. Anchor `seeds` cannot express this, because the address depends on `builder_idx` and on `approved_builders`, which the handler reads at run time.
    pub beneficiary_user: UncheckedAccount<'info>,
}

#[derive(Clone, Copy, AnchorSerialize, AnchorDeserialize)]
pub struct SettleRevenueShareArgs {
    pub market_index: u16,
    /// How many of the owner's sub-accounts ride the remaining accounts.
    pub num_owner_sub_accounts: u8,
}

#[derive(Accounts)]
pub struct SettleRevenueShare<'info> {
    pub state: AccountLoader<'info, State>,
    /// The owner of the escrow to settle.
    /// CHECK: the PDA seeds below bind this key to the escrow. The handler also compares it with the authority in the escrow header.
    pub escrow_authority: UncheckedAccount<'info>,
    /// The escrow that holds the accrued builder and referrer rows.
    /// CHECK: `load_zc_mut` reads this account and validates the owner and the discriminator. The seeds fix the address.
    #[account(
        mut,
        seeds = [REVENUE_SHARE_ESCROW_PDA_SEED.as_bytes(), escrow_authority.key().as_ref()],
        bump,
    )]
    pub revenue_share_escrow: UncheckedAccount<'info>,
    #[account(
        seeds = [b"spot_market_vault".as_ref(), 0_u16.to_le_bytes().as_ref()],
        bump
    )]
    pub spot_market_vault: Box<InterfaceAccount<'info, TokenAccount>>,
}
