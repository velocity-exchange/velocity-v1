//! Retiring a dormant, near-empty subaccount.
//!
//! The keeper takes over the account's remaining deposits, in exchange for
//! paying the rent back to the authority and freeing the subaccount id. Every
//! gate here exists because the tokens move to the keeper, so each one fails
//! closed.

use super::*;

pub fn handle_force_delete_user<'c: 'info, 'info>(
    ctx: Context<'info, ForceDeleteUser<'info>>,
) -> Result<()> {
    // Pyra accounts are exempt from force_delete_user
    let pyra_program = pubkey!("6JjHXLheGSNvvexgzMthEcgjkcirDrGduc3HAKB2P1v2");
    validate!(
        *ctx.accounts.authority.owner != pyra_program,
        ErrorCode::DefaultError,
        "pyra accounts are exempt from force_delete_user"
    )?;

    let state = ctx.accounts.state.load()?;
    let keeper_key = *ctx.accounts.keeper.key;

    let user_key = ctx.accounts.user.key();
    let user = &mut load_mut!(ctx.accounts.user)?;
    let user_stats = &mut load_mut!(ctx.accounts.user_stats)?;

    let clock = Clock::get()?;
    let slot = clock.slot;
    let now = clock.unix_timestamp;
    let mut maps = load_maps(
        &mut ctx.remaining_accounts.iter().peekable(),
        &MarketSet::new(),
        &get_market_set_for_spot_positions(&user.spot_positions),
        slot,
        state.slot_clock(),
        Some(state.oracle_guard_rails),
    )?;

    require_deletable(user, &state, &mut maps, slot)?;

    // cancel all open orders
    cancel_orders(
        user,
        &user_key,
        Some(&keeper_key),
        &mut maps,
        now,
        slot,
        OrderActionExplanation::None,
        None,
        None,
        None,
        false,
    )?;

    validate!(
        !user.perp_positions.iter().any(|p| !p.is_available()),
        ErrorCode::DefaultError,
        "user must have no perp positions"
    )?;

    sweep_deposits_to_keeper(&ctx, user, &mut maps, &state, now)?;

    validate_user_deletion(user, user_stats, &state, now)?;
    settle_escrow_on_delete(&ctx, user)?;

    safe_decrement!(user_stats.number_of_sub_accounts, 1);

    // Release the shared `State` borrow taken at the top of this handler. `Ref` implements `Drop`,
    // so the borrow lives to the end of the scope and a shadowing `let` does not end it. Without
    // this the `load_mut` below fails with `AccountBorrowFailed`, and it fails after the user's
    // tokens have already moved to the keeper.
    drop(state);

    let mut state = ctx.accounts.state.load_mut()?;
    safe_decrement!(state.number_of_sub_accounts, 1);

    emit!(DeleteUserRecord {
        ts: now,
        user_authority: *ctx.accounts.authority.key,
        user: user_key,
        sub_account_id: user.sub_account_id,
        keeper: Some(*ctx.accounts.keeper.key),
    });

    Ok(())
}

/// Prove the account is dust and has been dormant long enough to retire.
///
/// Deletion sends the user's remaining deposits to the keeper's own token
/// account, so the oracle gate fails closed. A stale-low price understates the
/// equity and makes a funded account look like dust.
fn require_deletable(user: &User, state: &State, maps: &mut AccountMaps, slot: u64) -> Result<()> {
    let (user_equity, all_oracles_valid) = calculate_user_equity(user, maps)?;

    validate!(
        all_oracles_valid,
        ErrorCode::InvalidOracle,
        "cannot force delete user with an invalid oracle"
    )?;

    let max_equity = QUOTE_PRECISION_I128 / 20;
    validate!(
        user_equity <= max_equity,
        ErrorCode::DefaultError,
        "user equity must be less than {}",
        max_equity
    )?;

    // The inactivity gate is the only reader of these two, and test builds
    // compile it out.
    let _ = (state, slot);
    #[cfg(not(feature = "anchor-test"))]
    {
        let time_since_last_active = state.slot_clock().elapsed(user.last_active_slot, slot);

        validate!(
            // ~3 months (12 weeks)
            time_since_last_active >= Millis::from_secs(7_257_600),
            ErrorCode::DefaultError,
            "user not inactive for long enough: {} ms",
            time_since_last_active.as_ms()
        )?;
    }

    Ok(())
}

/// Hand every remaining spot position to the keeper.
///
/// A deposit leaves the market vault for the keeper's token account, and a
/// borrow is repaid from it. Either way the position is zeroed, which is what
/// lets the subaccount retire.
fn sweep_deposits_to_keeper<'c: 'info, 'info>(
    ctx: &Context<'info, ForceDeleteUser<'info>>,
    user: &mut User,
    maps: &mut AccountMaps,
    state: &State,
    now: i64,
) -> Result<()> {
    // Book the interest of every market the user still holds before the transfers below read a
    // token amount. Cancelling the orders above can free a position that only open orders kept
    // alive, so the set is taken here rather than reused from the account load.
    //
    // The dust gate earlier in this handler still values the user through the stored indexes. An
    // account close to the cap can therefore read below it and be deleted, which sends its
    // deposits to the keeper. Moving that gate after this call is a separate change.
    controller::spot_balance::refresh_spot_market_interest(
        &maps.spot_market_map,
        Some(&mut maps.oracle_map),
        &get_market_set_for_spot_positions(&user.spot_positions),
        now,
        state.funding_paused()?,
    )?;

    let keeper_key = *ctx.accounts.keeper.key;
    for spot_position in user.spot_positions.iter_mut() {
        if spot_position.is_available() {
            continue;
        }

        let spot_market = &mut maps
            .spot_market_map
            .get_ref_mut(&spot_position.market_index)?;
        let transfer = KeeperTransfer::find(ctx.remaining_accounts, spot_market, &keeper_key)?;
        transfer.settle_position(ctx, spot_position, spot_market, state.signer_nonce)?;
    }

    Ok(())
}

/// The token accounts one spot position settles through, picked out of
/// `remaining_accounts` by address.
struct KeeperTransfer<'info> {
    token_program: Interface<'info, TokenInterface>,
    mint: Option<InterfaceAccount<'info, Mint>>,
    keeper_vault: InterfaceAccount<'info, TokenAccount>,
    market_vault: InterfaceAccount<'info, TokenAccount>,
}

impl<'info> KeeperTransfer<'info> {
    fn find(
        remaining_accounts: &'info [AccountInfo<'info>],
        spot_market: &SpotMarket,
        keeper_key: &Pubkey,
    ) -> Result<Self> {
        let token_program_pubkey = spot_market.get_token_program();
        let token_program = remaining_accounts
            .iter()
            .find(|acc| acc.key() == token_program_pubkey)
            .map(Interface::try_from)
            .unwrap()
            .unwrap();

        let spot_market_mint = &spot_market.mint;
        let mint = remaining_accounts
            .iter()
            .find(|acc| acc.key() == spot_market_mint.key())
            .map(|acc| InterfaceAccount::try_from(acc).unwrap());

        let keeper_vault_key = get_associated_token_address_with_program_id(
            keeper_key,
            spot_market_mint,
            &token_program_pubkey,
        );
        let keeper_vault = remaining_accounts
            .iter()
            .find(|acc| acc.key() == keeper_vault_key.key())
            .map(InterfaceAccount::try_from)
            .unwrap()
            .unwrap();

        let spot_market_vault = spot_market.vault;
        let market_vault = remaining_accounts
            .iter()
            .find(|acc| acc.key() == spot_market_vault.key())
            .map(InterfaceAccount::try_from)
            .unwrap()
            .unwrap();

        Ok(Self {
            token_program,
            mint,
            keeper_vault,
            market_vault,
        })
    }

    /// Move the position's whole balance, and prove the vault still covers
    /// what its depositors are owed.
    fn settle_position(
        mut self,
        ctx: &Context<'info, ForceDeleteUser<'info>>,
        spot_position: &mut crate::state::user::SpotPosition,
        spot_market: &mut SpotMarket,
        signer_nonce: u8,
    ) -> Result<()> {
        let token_amount = spot_position.get_token_amount(spot_market)?;
        let balance_type = spot_position.balance_type;

        if balance_type == SpotBalanceType::Deposit {
            update_spot_balances(
                token_amount,
                &SpotBalanceType::Borrow,
                spot_market,
                spot_position,
                true,
            )?;

            // TODO: support transfer hook tokens
            send_from_program_vault(
                &self.token_program,
                &self.market_vault,
                &self.keeper_vault,
                &ctx.accounts.velocity_signer,
                signer_nonce,
                token_amount.cast()?,
                &self.mint,
                None,
            )?;
        } else {
            update_spot_balances(
                token_amount,
                &SpotBalanceType::Deposit,
                spot_market,
                spot_position,
                false,
            )?;

            // TODO: support transfer hook tokens
            receive(
                &self.token_program,
                &self.keeper_vault,
                &self.market_vault,
                &ctx.accounts.keeper.to_account_info(),
                token_amount.cast()?,
                &self.mint,
                None,
            )?;
        }

        self.market_vault.reload()?;
        math::spot_withdraw::validate_spot_market_vault_amount(
            spot_market,
            self.market_vault.amount,
        )?;

        Ok(())
    }
}

/// Settle this subaccount's revenue-share rows before the id goes away.
///
/// This path retires the id exactly as `delete_user` does, so it orphans a fee-bearing row the same
/// way. See `handle_delete_user` for why the row then becomes unreachable. `cancel_orders` closed
/// every order of this subaccount, so each row for it becomes `Completed`, or is cleared when it
/// carries no fees. That is the state the permissionless sweep pays out of.
///
/// The escrow is pinned to the authority's PDA by `seeds`, so an empty account proves this
/// authority has no escrow rather than signalling an omitted account.
fn settle_escrow_on_delete<'c: 'info, 'info>(
    ctx: &Context<'info, ForceDeleteUser<'info>>,
    user: &User,
) -> Result<()> {
    if ctx.accounts.revenue_share_escrow.data_is_empty() {
        return Ok(());
    }

    // `ZeroCopyLoader` is in scope for this module and also has a `load_zc_mut`, so
    // name the trait to pick the escrow's loader.
    use crate::state::revenue_share::RevenueShareEscrowLoader;

    let mut escrow = RevenueShareEscrowLoader::load_zc_mut(&*ctx.accounts.revenue_share_escrow)?;
    escrow.revoke_completed_orders(user)?;

    // After the revoke above, nothing for this subaccount may still be
    // outstanding. If something is, fail rather than retire the id over it.
    validate!(
        !escrow.has_outstanding_orders_for_sub_account(user.sub_account_id)?,
        ErrorCode::UserCantBeDeleted,
        "sub account {} still has outstanding revenue-share orders",
        user.sub_account_id
    )?;

    Ok(())
}

#[derive(Accounts)]
pub struct ForceDeleteUser<'info> {
    #[account(
        mut,
        has_one = authority,
        close = authority
    )]
    pub user: AccountLoader<'info, User>,
    #[account(
        mut,
        has_one = authority
    )]
    pub user_stats: AccountLoader<'info, UserStats>,
    #[account(mut)]
    pub state: AccountLoader<'info, State>,
    /// CHECK: authority
    #[account(mut)]
    pub authority: UncheckedAccount<'info>,
    #[account(
        mut,
        constraint = check_hot(&keeper.key(), &state, HotRole::UserFlag)?
    )]
    pub keeper: Signer<'info>,
    /// CHECK: forced velocity_signer
    pub velocity_signer: UncheckedAccount<'info>,
    /// CHECK: the authority's `RevenueShareEscrow`. It may legitimately not exist,
    /// because most users never create one. It carries the same contract as
    /// `DeleteUser::revenue_share_escrow`: an `UncheckedAccount` pinned by `seeds`, so
    /// the handler can tell "this authority has no escrow" (`data_is_empty()`) from "the
    /// keeper omitted the account to skip the check". It is required rather than
    /// `Option` for that second reason.
    #[account(
        mut,
        seeds = [REVENUE_SHARE_ESCROW_PDA_SEED.as_bytes(), authority.key().as_ref()],
        bump,
    )]
    pub revenue_share_escrow: UncheckedAccount<'info>,
}
