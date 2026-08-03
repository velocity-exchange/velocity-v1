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
            margin_calculation::MarginContext, perp_market_map::MarketSet, state::State,
            user::User, user_conditions::UserConditionsV0,
        },
        validate,
    },
    anchor_lang::prelude::*,
};

#[derive(Accounts)]
pub struct ResolveLiquidatePerpWithFill<'info> {
    /// The shared staging account, index 0 by convention — a resolver's
    /// response pointer is interpreted against it.
    #[account(mut, seeds = [crate::state::relay_scratch::RELAY_SCRATCH_PDA_SEED], bump)]
    pub scratch: AccountLoader<'info, crate::state::relay_scratch::RelayScratchV0>,
    /// Read-only: resolvers stage into the shared scratch account, not
    /// into the block they read.
    #[account(constraint = liq_conditions.load()?.user == user.key())]
    pub liq_conditions: AccountLoader<'info, UserConditionsV0>,
    pub user: AccountLoader<'info, User>,
    pub state: AccountLoader<'info, State>,
}

pub fn handle_resolve_liquidate_perp_with_fill<'c: 'info, 'info>(
    ctx: Context<'info, ResolveLiquidatePerpWithFill<'info>>,
) -> Result<()> {
    crate::instructions::resolve_into(&ctx.accounts.scratch, || {
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
            return Ok(None);
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
            return Ok(None);
        };

        let (protocol_user, protocol_user_stats) = crate::state::pdas::protocol_user_pair();
        let user_stats =
            crate::state::pdas::user_stats(&crate::load!(ctx.accounts.user)?.authority);
        Ok(Some(
            // `liquidate_perp_with_fill` shares `liquidate_perp`'s account
            // list, so the two cannot be paired by name: this is the
            // inventory-free flavor, and staging the plain one would have
            // the protocol acquire the position.
            crate::instructions::StagedCall::new::<crate::instruction::LiquidatePerpWithFill>(
                crate::accounts::LiquidatePerp {
                    state: ctx.accounts.state.key(),
                    authority: crate::state::pdas::keeper_placeholder(),
                    liquidator: protocol_user,
                    liquidator_stats: protocol_user_stats,
                    user: ctx.accounts.user.key(),
                    user_stats,
                    crank_conditions: Some(crate::state::pdas::clob_crank_conditions(market_index)),
                },
            )
            .refs(stored)
            .arg(market_index)?,
        ))
    })
}
