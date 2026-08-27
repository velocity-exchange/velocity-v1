//! Resolver for a distress threshold: work out which stage of the ladder
//! this account is actually in right now, and stage that.
//!
//! Cancelling comes before liquidating. `force_cancel_clob_orders` answers to
//! the initial margin requirement and liquidation to the maintenance one, so
//! anything liquidatable was already cancellable — the two are stages of one
//! ladder, and one watch drives both. The sync prices the threshold at the
//! stage the account is in; this picks the executor to match, and relay's
//! level-triggered wake brings it back for the next stage.
//!
//! Orders first, deliberately: a liquidation that leaves risk-increasing
//! orders resting on a book hands the liquidated account new exposure the
//! moment one fills.
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
        math::margin::{
            calculate_margin_requirement_and_total_collateral_and_liability_info,
            calculate_net_equity_for_floor, MarginRequirementType,
        },
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
            state.slot_clock(),
            Some(state.oracle_guard_rails),
        )?;

        // Stage one: a book this account may no longer rest risk-increasing
        // orders on. The grounds are the executor's own — initial margin, a
        // provable floor breach, or the authority-wide latch — recomputed
        // here so the wake's conservative single-oracle estimate is never
        // what acts.
        let cancel_target = {
            let user = crate::load!(ctx.accounts.user)?;
            let initial = calculate_margin_requirement_and_total_collateral_and_liability_info(
                &user,
                &perp_market_map,
                &spot_market_map,
                &mut oracle_map,
                MarginContext::standard(MarginRequirementType::Initial),
            )?;
            let below_floor = calculate_net_equity_for_floor(
                &user,
                &perp_market_map,
                &spot_market_map,
                &mut oracle_map,
            )?
            .is_some_and(|net_equity| net_equity.proves_below_floor(&user));
            if initial.meets_margin_requirement() && !below_floor {
                None
            } else {
                // One market per wake; relay comes back for the rest while
                // the account still qualifies.
                user.perp_positions
                    .iter()
                    .map(|position| position.market_index)
                    .find(|market_index| user.clob_resident_open_orders(*market_index) > 0)
            }
        };
        if let Some(market_index) = cancel_target {
            let quoter = perp_market_map.get_ref(&market_index)?.clob_quoter;
            if quoter != Pubkey::default() {
                let user_stats =
                    crate::state::pdas::user_stats(&crate::load!(ctx.accounts.user)?.authority);
                let (protocol_user, protocol_user_stats) = crate::state::pdas::protocol_user_pair();
                // The book and its program come off the registry entry, which
                // the sync stored in the list's inert tail alongside the
                // margin map.
                let Some(entry_info) =
                    crate::state::prop_amm::find_account(ctx.remaining_accounts, &quoter)
                else {
                    msg!(
                        "quoter {} absent from the stored list; cannot stage a cancel",
                        quoter
                    );
                    return Ok(None);
                };
                let entry =
                    AccountLoader::<crate::state::prop_amm::QuoterV0>::try_from(entry_info)?;
                let (clob_market, clob_program) = {
                    let entry = entry.load()?;
                    (entry.response_account, entry.program_id)
                };
                return Ok(
                    Some(
                        crate::instructions::StagedCall::new::<
                            crate::instruction::ForceCancelClobOrders,
                        >(crate::accounts::ForceCancelClobOrders {
                            state: ctx.accounts.state.key(),
                            authority: crate::state::pdas::keeper_placeholder(),
                            filler: protocol_user,
                            filler_stats: protocol_user_stats,
                            user: ctx.accounts.user.key(),
                            user_stats,
                            quoter,
                            clob_market,
                            clob_program,
                            clob_authority: crate::signer::find_clob_authority().0,
                            crank_conditions: Some(crate::state::pdas::clob_crank_conditions(
                                market_index,
                            )),
                        })
                        .refs(stored)
                        .arg(market_index)?
                        // The sweep takes the side that cannot be reducing, so
                        // the resolver never has to read the book to name refs.
                        .arg(Vec::<crate::instructions::ForceCancelClobRefV0>::new())?,
                    ),
                );
            }
        }

        // Stage two: no orders left in the way, so the question is whether
        // this is liquidatable.
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
            // Spot-only distress. The account is liquidatable and the check
            // above proved it, but there is no crank that can act on it.
            //
            // `liquidate_spot` settles by giving the liquidator the borrow and
            // the collateral behind it, so a protocol keeper would end up
            // holding spot inventory and its price risk. The perp path avoids
            // that by routing the fill through the book; spot has no such
            // flavor without an external swap venue, which nothing here wires.
            // A real liquidator carries that inventory on its own balance
            // sheet and unwinds it elsewhere, so this stays a keeper-bot path
            // by design rather than by omission.
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
                    // The crank reads its own priority fee back from
                    // here to price the reimbursement.
                    instructions_sysvar: Some(solana_program::sysvar::instructions::ID),
                },
            )
            .refs(stored)
            .arg(market_index)?,
        ))
    })
}
