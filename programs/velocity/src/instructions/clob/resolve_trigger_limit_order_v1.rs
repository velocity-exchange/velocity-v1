//! The relay resolver for `trigger_limit_order_v1`. It is simulation-only and
//! stages the crank for the one trigger slot whose condition fired.

use {
    super::{FiredConditionArgV0, TriggerLimitOrderV1Args},
    crate::{
        error::ErrorCode,
        state::{prop_amm::QuoterSlabV0, state::State, user::User},
    },
    anchor_lang::prelude::*,
};

/// The accounts the resolver reads. It is staged from the user's synced
/// trigger conditions.
#[derive(Accounts)]
pub struct ResolveTriggerLimitOrderV1<'info> {
    /// The shared staging account, at index 0 by convention. A resolver's
    /// response pointer is read against it.
    #[account(mut, seeds = [crate::state::relay_scratch::RELAY_SCRATCH_PDA_SEED], bump)]
    pub scratch: AccountLoader<'info, crate::state::relay_scratch::RelayScratchV0>,
    /// Read-only. A resolver stages into the shared scratch account rather
    /// than into the block it reads.
    #[account(constraint = trigger_conditions.load()?.user == user.key())]
    pub trigger_conditions: AccountLoader<'info, crate::state::user_conditions::UserConditionsV0>,
    pub user: AccountLoader<'info, User>,
    /// CHECK: the perp market's `has_one` binds it.
    pub oracle: UncheckedAccount<'info>,
    #[account(has_one = oracle)]
    pub perp_market: AccountLoader<'info, crate::state::perp_market::PerpMarket>,
    /// The exchange pause, the median-price flag and the oracle guard rails
    /// the executor judges the trigger with.
    pub state: AccountLoader<'info, State>,
    #[account(
        has_one = clob_market,
        constraint = quoter_slab.load()?.market == perp_market.load()?.market_index,
    )]
    pub quoter_slab: AccountLoader<'info, QuoterSlabV0>,
    /// CHECK: the slab's `has_one` binds it. The resolver asks it for the
    /// room left on the side the order rests on.
    pub clob_market: UncheckedAccount<'info>,
    /// CHECK: address-locked to velocity's CLOB.
    #[account(address = crate::ids::clob_program::id())]
    pub clob_program: UncheckedAccount<'info>,
}

pub fn handle_resolve_trigger_limit_order_v1(
    ctx: Context<ResolveTriggerLimitOrderV1>,
    fired: FiredConditionArgV0,
) -> Result<()> {
    crate::instructions::constraints::require_view_accounts(
        &ctx.accounts.to_account_infos(),
        &[ctx.accounts.scratch.key()],
    )?;
    crate::instructions::resolve_into(&ctx.accounts.scratch, || {
        let accounts = &ctx.accounts;
        let due = crate::instructions::clob::helpers::crank_common::resolve_due_trigger(
            &crate::instructions::clob::helpers::crank_common::TriggerResolverAccounts {
                trigger_conditions: &accounts.trigger_conditions,
                user: &accounts.user,
                oracle: &accounts.oracle,
                perp_market: &accounts.perp_market,
                state: &accounts.state,
                quoter_slab: &accounts.quoter_slab,
                clob_market: &accounts.clob_market,
                clob_program: &accounts.clob_program,
            },
            &fired,
            crate::instructions::clob::helpers::crank_common::TriggerResolverKind::ClobRest,
        )?;
        let Some(meta) = due else {
            return Ok(None);
        };

        let (protocol_user, protocol_user_stats) = crate::state::pdas::protocol_user_pair();
        let user_stats =
            crate::state::pdas::user_stats(&crate::load!(ctx.accounts.user)?.authority);
        Ok(Some(
            crate::staged_call!(TriggerLimitOrderV1 {
                state: crate::state::pdas::state(),
                authority: crate::state::pdas::keeper_placeholder(),
                filler: protocol_user,
                filler_stats: protocol_user_stats,
                user: ctx.accounts.user.key(),
                user_stats,
                quoter_slab: meta.quoter_slab,
                clob_market: meta.clob_market,
                clob_program: meta.clob_program,
                crank_conditions: Some(crate::state::pdas::clob_crank_conditions(
                    meta.market_index,
                )),

                trigger_conditions: ctx.accounts.trigger_conditions.key(),
                sol_spot_market:
                    crate::instructions::clob::helpers::crank_common::sol_spot_market_ref(
                        &*ctx.accounts.state.load()?,
                    ),
            })
            .refs(ctx.accounts.trigger_conditions.load()?.read_sync_accounts())
            .arg(TriggerLimitOrderV1Args {
                market_index: meta.market_index,
                order_id: meta.order_id,
            })?,
        ))
    })
}
