//! Resolver for a liquidation threshold: confirm the user is actually
//! liquidatable right now and stage `liquidate_perp_with_fill` with the
//! protocol `User` as the liquidator.
//!
//! The threshold that woke this is a conservative single-oracle estimate,
//! so the resolver is where the *real* answer is computed — the full
//! maintenance-margin calculation over every position and deposit, using
//! the same code the executor runs. Not liquidatable yet → NoWork, and the
//! turner's backoff re-checks on the next ticks while the wake stays
//! level-triggered. That pairing is the relay-native form of the keeper
//! bot's "recheck the high-risk bucket on every oracle update".

use {
    crate::{
        error::ErrorCode,
        instructions::optional_accounts::{load_maps, AccountMaps},
        math::margin::calculate_margin_requirement_and_total_collateral_and_liability_info,
        state::{
            liq_conditions::LiqConditionsV0, margin_calculation::MarginContext,
            perp_market_map::MarketSet, state::State, user::User,
        },
        validate,
    },
    anchor_lang::prelude::*,
    relay_spec::{ResolvedCrankV0, KEEPER_PLACEHOLDER},
    solana_program::{instruction::AccountMeta, program::set_return_data},
};

#[derive(Accounts)]
pub struct ResolveLiquidatePerpWithFill<'info> {
    /// Writable only for the staging region; simulation-only.
    #[account(mut, constraint = liq_conditions.load()?.user == user.key())]
    pub liq_conditions: AccountLoader<'info, LiqConditionsV0>,
    pub user: AccountLoader<'info, User>,
    pub state: AccountLoader<'info, State>,
    /// CHECK: part of the margin map; validated by `load_maps` below.
    pub oracle: UncheckedAccount<'info>,
}

pub fn handle_resolve_liquidate_perp_with_fill<'c: 'info, 'info>(
    ctx: Context<'info, ResolveLiquidatePerpWithFill<'info>>,
) -> Result<()> {
    let clock = Clock::get()?;
    let state = ctx.accounts.state.load()?;

    // The stored account list carries the user's full margin maps, so the
    // real calculation runs here under simulation.
    let stored = ctx.accounts.liq_conditions.load()?.read_sync_accounts();
    validate!(
        !ctx.remaining_accounts.is_empty(),
        ErrorCode::DefaultError,
        "resolver needs the stored margin-map accounts"
    )?;
    let AccountMaps {
        perp_market_map,
        spot_market_map,
        mut oracle_map,
    } = load_maps(
        &mut ctx.remaining_accounts.iter().peekable(),
        &MarketSet::new(),
        &MarketSet::new(),
        clock.slot,
        Some(state.oracle_guard_rails),
    )?;

    let liquidatable = {
        let user = crate::load!(ctx.accounts.user)?;
        let calculation = calculate_margin_requirement_and_total_collateral_and_liability_info(
            &user,
            &perp_market_map,
            &spot_market_map,
            &mut oracle_map,
            MarginContext::liquidation(state.liquidation_margin_buffer_ratio),
        )?;
        !calculation.meets_margin_requirement()
    };
    if !liquidatable {
        return crate::instructions::no_work();
    }

    // Which market to liquidate: the user's largest live perp position.
    let target = {
        let user = crate::load!(ctx.accounts.user)?;
        user.perp_positions
            .iter()
            .filter(|p| p.base_asset_amount != 0)
            .max_by_key(|p| (p.base_asset_amount as i128).abs())
            .map(|p| p.market_index)
    };
    let Some(market_index) = target else {
        // Nothing perp-shaped to liquidate (spot-only distress): the
        // keeper bots own that path.
        return crate::instructions::no_work();
    };

    let (signer, _) = Pubkey::find_program_address(&[b"velocity_signer"], &crate::ID);
    let (protocol_user, protocol_user_stats) =
        crate::instructions::derive_protocol_user_pdas(&signer);
    let (user_stats, _) = Pubkey::find_program_address(
        &[
            b"user_stats",
            crate::load!(ctx.accounts.user)?.authority.as_ref(),
        ],
        &crate::ID,
    );
    let (market_conditions, _) = Pubkey::find_program_address(
        &[
            crate::state::clob_crank::CLOB_CRANK_CONDITIONS_PDA_SEED,
            market_index.to_le_bytes().as_ref(),
        ],
        &crate::ID,
    );

    let mut metas = crate::accounts::LiquidatePerp {
        state: ctx.accounts.state.key(),
        authority: Pubkey::new_from_array(KEEPER_PLACEHOLDER),
        liquidator: protocol_user,
        liquidator_stats: protocol_user_stats,
        user: ctx.accounts.user.key(),
        user_stats,
        crank_conditions: Some(market_conditions),
    }
    .to_account_metas(None);
    for r in &stored {
        let pubkey = Pubkey::new_from_array(r.address);
        metas.push(if r.writable != 0 {
            AccountMeta::new(pubkey, false)
        } else {
            AccountMeta::new_readonly(pubkey, false)
        });
    }

    let mut args = Vec::with_capacity(2);
    market_index.serialize(&mut args)?;
    let resolved = ResolvedCrankV0 {
        accounts: crate::instructions::to_account_refs(metas),
        data: args,
    };
    let pointer = ctx.accounts.liq_conditions.load_mut()?.stage(&resolved)?;
    set_return_data(&pointer);
    Ok(())
}
