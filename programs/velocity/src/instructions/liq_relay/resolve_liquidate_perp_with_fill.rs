//! Resolver for a distress threshold. It works out which stage of the ladder
//! the account is in now, and stages the call that matches.
//!
//! A cancel comes before a liquidation. `force_cancel_clob_orders` answers to
//! the initial margin requirement and a liquidation answers to the maintenance
//! one, so anything liquidatable was already cancellable. The two are stages
//! of one ladder, and one watch drives both. The sync prices the threshold at
//! the stage the account is in. This resolver picks the matching executor, and
//! relay's level-triggered wake brings the account back for the next stage.
//!
//! Orders come first. A liquidation that leaves risk-increasing orders resting
//! on a book hands the liquidated account new exposure the moment one fills.
//!
//! The threshold that woke this is a conservative single-oracle estimate. The
//! resolver computes the real answer. It runs the full maintenance-margin
//! calculation over every position and deposit, with the same code the
//! executor runs. An account that is not liquidatable yet returns NoWork. The
//! wake stays level-triggered, so the turner's backoff rechecks the account on
//! the next ticks.

use {
    crate::{
        error::ErrorCode,
        instructions::optional_accounts::{load_maps, AccountMaps},
        math::margin::calculate_margin_requirement_and_total_collateral_and_liability_info,
        state::{
            clob_crank::LIQUIDATION_FLAT_PAYMENT_MIN_FILLED_QUOTE,
            margin_calculation::MarginContext, perp_market_map::MarketSet, prop_amm::QuoterSlabExt,
            state::State, user::User, user_conditions::UserConditionsV0,
        },
        validate,
    },
    anchor_lang::prelude::*,
};

/// Book makers one staged liquidation carries. Each maker costs two account
/// locks in the executor. The cap is what one transaction holds beside the
/// margin map and the quoter tail.
const MAX_LIQUIDATION_MAKERS: usize = 4;

/// Rows read off the swept side to find those makers. The count is deeper than
/// the cap because one owner can hold several of the rows in front.
const BOOK_MAKER_ROWS: u16 = 32;

#[derive(Accounts)]
pub struct ResolveLiquidatePerpWithFill<'info> {
    /// The shared staging account, at index 0 by convention. A resolver's
    /// response pointer is read against it.
    #[account(mut, seeds = [crate::state::relay_scratch::RELAY_SCRATCH_PDA_SEED], bump)]
    pub scratch: AccountLoader<'info, crate::state::relay_scratch::RelayScratchV0>,
    /// Read-only. A resolver stages into the shared scratch account rather
    /// than into the block it reads.
    #[account(constraint = liq_conditions.load()?.user == user.key())]
    pub liq_conditions: AccountLoader<'info, UserConditionsV0>,
    pub user: AccountLoader<'info, User>,
    pub state: AccountLoader<'info, State>,
}

pub fn handle_resolve_liquidate_perp_with_fill<'c: 'info, 'info>(
    ctx: Context<'info, ResolveLiquidatePerpWithFill<'info>>,
) -> Result<()> {
    // A resolver is a view: the scratch account is the only thing it writes,
    // and nothing reads that back on chain. Asserting it here makes landing
    // this instruction inert against the caller rather than against review.
    crate::instructions::constraints::require_view_accounts(
        &ctx.accounts.to_account_infos(),
        &[ctx.accounts.scratch.key()],
    )?;
    crate::instructions::resolve_into(&ctx.accounts.scratch, || {
        let clock = Clock::get()?;
        let state = ctx.accounts.state.load()?;

        // The stored account list carries the user's full margin maps, so the
        // real calculation runs here under simulation.
        let stored = ctx.accounts.liq_conditions.load()?.read_sync_accounts();
        validate!(
            !ctx.remaining_accounts.is_empty(),
            ErrorCode::ResolverMarginMapMissing,
            "resolver needs the stored margin-map accounts"
        )?;

        let account_iter = &mut ctx.remaining_accounts.iter().peekable();
        let mut maps = load_maps(
            account_iter,
            &MarketSet::new(),
            &MarketSet::new(),
            clock.slot,
            state.slot_clock(),
            Some(state.oracle_guard_rails),
        )?;

        // Where the stored list stops being the margin map. The liquidation
        // executor reads its leftover accounts in sections, and the makers it
        // may settle against belong between the map and the quoter tail.
        let map_section = ctx.remaining_accounts.len() - account_iter.len();

        let cancel_target = find_cancel_target(&ctx.accounts.user, &mut maps)?;
        if let Some(market_index) = cancel_target {
            let book = maps.perp_market_map.get_ref(&market_index)?.clob_market;
            if book != Pubkey::default() {
                return stage_force_cancel(
                    ctx.accounts.state.key(),
                    &ctx.accounts.user,
                    ctx.remaining_accounts,
                    market_index,
                    stored,
                );
            }
        }

        // Stage two of the ladder. No orders are left in the way, so the
        // question is whether the account is liquidatable.
        let liquidatable = is_liquidatable(
            &ctx.accounts.user,
            &mut maps,
            state.liquidation_margin_buffer_ratio,
        )?;

        if !liquidatable {
            return Ok(None);
        }

        let Some(market_index) = largest_perp_position(&ctx.accounts.user)? else {
            // Spot-only distress. The account is liquidatable and the check above proved it, but no
            // crank can act on it. `liquidate_spot` gives the liquidator the borrow and the
            // collateral behind it, so a protocol keeper would hold spot inventory and its price
            // risk. The perp path avoids that by routing the fill through the book. Spot has no
            // such flavor without an external swap venue, and nothing here wires one. A real
            // liquidator carries that inventory on its own balance sheet and unwinds it elsewhere,
            // so this stays a keeper-bot path.
            //
            // allow-verbose: no other comment says why a liquidatable account returns no work.
            return Ok(None);
        };

        if !position_can_pay_the_crank(&ctx.accounts.user, &mut maps, market_index)? {
            return Ok(None);
        }

        stage_liquidate_perp(
            ctx.accounts.state.key(),
            &ctx.accounts.user,
            ctx.remaining_accounts,
            market_index,
            stored,
            map_section,
        )
        .map(Some)
    })
}

/// Stage one of the ladder. Finds a book this account may no longer rest
/// risk-increasing orders on. The grounds are recomputed here, from the
/// initial margin requirement and a provable floor breach, so the wake's
/// conservative single-oracle estimate is never what acts. The executor
/// accepts a third ground, the authority-wide equity breaker, which this
/// resolver does not read.
fn find_cancel_target(
    user_loader: &AccountLoader<'_, User>,
    maps: &mut AccountMaps,
) -> Result<Option<u16>> {
    let user = crate::load!(user_loader)?;
    let grounds = crate::controller::orders::ForceCancelGrounds::measure(&user, maps)?;
    Ok(if !grounds.any() {
        None
    } else {
        // One market per wake. Relay comes back for the rest while
        // the account still qualifies.
        user.perp_positions
            .iter()
            .map(|position| position.market_index)
            .find(|market_index| user.clob_resident_open_orders(*market_index) > 0)
    })
}

/// Stage the cancel of the account's resting orders on `market_index`.
///
/// The book and its program come off the market's slab. The sync stored that
/// slab in the list's inert tail beside the margin map. A slab the stored list
/// does not carry leaves no work, because the liquidation must not run while
/// risk-increasing orders rest.
fn stage_force_cancel<'info>(
    state_key: Pubkey,
    user_loader: &AccountLoader<'_, User>,
    remaining_accounts: &'info [AccountInfo<'info>],
    market_index: u16,
    stored: Vec<relay_spec::AccountRefV0>,
) -> Result<Option<crate::instructions::StagedCall>> {
    let (protocol_user, protocol_user_stats) = crate::state::pdas::protocol_user_pair();
    let quoter_slab = crate::state::pdas::quoter_slab(market_index);
    let Some(slab_info) = crate::state::prop_amm::find_account(remaining_accounts, &quoter_slab)
    else {
        msg!(
            "quoter slab {} absent from the stored list; cannot stage a cancel",
            quoter_slab
        );

        return Ok(None);
    };

    let slab = AccountLoader::<crate::state::prop_amm::QuoterSlabV0>::try_from(slab_info)?;
    let (clob_market, clob_program) = {
        let slots = slab.slots()?;
        let Some(index) = crate::state::prop_amm::clob_slot_index(&slots) else {
            msg!("quoter slab holds no book slot; cannot stage a cancel");
            return Ok(None);
        };

        (
            slots[index].config.response_account,
            slots[index].config.program_id,
        )
    };

    Ok(Some(
        crate::instructions::StagedCall::new::<crate::instruction::ForceCancelClobOrders>(
            crate::accounts::ForceCancelClobOrders {
                state: state_key,
                authority: crate::state::pdas::keeper_placeholder(),
                filler: protocol_user,
                filler_stats: protocol_user_stats,
                user: user_loader.key(),
                quoter_slab,
                clob_market,
                clob_program,
                crank_conditions: Some(crate::state::pdas::clob_crank_conditions(market_index)),
            },
        )
        .refs(stored)
        // The sweep takes the side that cannot be reducing, so
        // the resolver never has to read the book to name refs.
        .arg(crate::instructions::ForceCancelClobOrdersArgs {
            market_index,
            order_refs: Vec::new(),
        })?,
    ))
}

/// The full maintenance-margin calculation, the same code the executor
/// runs.
fn is_liquidatable(
    user_loader: &AccountLoader<'_, User>,
    maps: &mut AccountMaps,
    liquidation_margin_buffer_ratio: u32,
) -> Result<bool> {
    let user = crate::load!(user_loader)?;
    let calculation = calculate_margin_requirement_and_total_collateral_and_liability_info(
        &user,
        maps,
        MarginContext::liquidation(liquidation_margin_buffer_ratio),
    )?;

    Ok(!calculation.meets_margin_requirement())
}

/// Which market to liquidate: the user's largest live perp position.
fn largest_perp_position(user_loader: &AccountLoader<'_, User>) -> Result<Option<u16>> {
    let user = crate::load!(user_loader)?;
    Ok(user
        .perp_positions
        .iter()
        .filter(|p| p.base_asset_amount != 0)
        .max_by_key(|p| (p.base_asset_amount as i128).abs())
        .map(|p| p.market_index))
}

/// Whether a liquidation of this position can pay the crank that runs it.
///
/// A fill below [`LIQUIDATION_FLAT_PAYMENT_MIN_FILLED_QUOTE`] earns no flat
/// payment. A relay crank that pays nothing then fails its own payment guard,
/// after the liquidation already ran. The whole position priced at the oracle
/// is the most any fill of it can settle, so a position below the floor can
/// never pay and is not staged. Such an account stays with the keeper bots,
/// which carry a balance sheet and do not need the reservoir.
fn position_can_pay_the_crank(
    user_loader: &AccountLoader<'_, User>,
    maps: &mut AccountMaps,
    market_index: u16,
) -> Result<bool> {
    let base = {
        let user = crate::load!(user_loader)?;
        (user.get_perp_position(market_index)?.base_asset_amount as i128).unsigned_abs()
    };
    let oracle_id = maps.perp_market_map.get_ref(&market_index)?.oracle_id();
    let price = maps.oracle_map.get_price_data(&oracle_id)?.price.max(0) as u128;
    let notional = base
        .saturating_mul(price)
        .saturating_div(crate::math::constants::BASE_PRECISION);
    Ok(notional >= u128::from(LIQUIDATION_FLAT_PAYMENT_MIN_FILLED_QUOTE))
}

/// Stage the liquidation of `market_index`.
///
/// `liquidate_perp_with_fill` shares `liquidate_perp`'s account list, so the
/// account struct's name does not name the instruction. This stages the flavor
/// that takes no inventory. Staging the plain one would make the protocol
/// acquire the position.
fn stage_liquidate_perp<'info>(
    state_key: Pubkey,
    user_loader: &AccountLoader<'info, User>,
    remaining_accounts: &'info [AccountInfo<'info>],
    market_index: u16,
    stored: Vec<relay_spec::AccountRefV0>,
    map_section: usize,
) -> Result<crate::instructions::StagedCall> {
    let (protocol_user, protocol_user_stats) = crate::state::pdas::protocol_user_pair();
    let user_stats = crate::state::pdas::user_stats(&crate::load!(user_loader)?.authority);
    let makers = book_makers(user_loader, remaining_accounts, market_index)?;
    // The executor parses its leftover accounts in order: the margin map, the
    // `(User, UserStats)` pairs it may settle against, then the quoter tail.
    // The stored list holds the first section and the last, so the makers go
    // between them rather than after.
    let (map_refs, tail_refs) = stored.split_at(map_section.min(stored.len()));
    crate::instructions::StagedCall::new::<crate::instruction::LiquidatePerpWithFill>(
        crate::accounts::LiquidatePerp {
            state: state_key,
            authority: crate::state::pdas::keeper_placeholder(),
            liquidator: protocol_user,
            liquidator_stats: protocol_user_stats,
            user: user_loader.key(),
            user_stats,
            crank_conditions: Some(crate::state::pdas::clob_crank_conditions(market_index)),
            // The crank reads its own priority fee back from
            // here to price the reimbursement.
            instructions_sysvar: Some(solana_program::sysvar::instructions::ID),
        },
    )
    .refs(map_refs.iter().copied())
    .maker_refs(makers)
    .refs(tail_refs.iter().copied())
    .arg(market_index)
}

/// The book makers a liquidation of `market_index` would settle against.
///
/// The liquidation closes the account's position, so it sweeps the side
/// opposite to that position. A quoter fills nobody the caller did not load.
/// Owners come off the book best price first, and the walk stops at the cap
/// that one transaction's account locks hold. A position deeper than that
/// liquidates a stage at a time, because relay's level-triggered wake brings
/// the account back while it still qualifies.
///
/// An empty list is a market with no book, a book that cannot quote, or a
/// stored list that carries neither. All three leave the fill to the vAMM,
/// which is what it reached before the book existed.
fn book_makers<'info>(
    user_loader: &AccountLoader<'info, User>,
    remaining_accounts: &'info [AccountInfo<'info>],
    market_index: u16,
) -> Result<Vec<crate::state::prop_amm::UserRefV0>> {
    use crate::state::prop_amm::{
        clob_slot_index, find_account, QuoterCpiScratch, QuoterSlabExt, QuoterSlabV0,
    };

    let Some(direction) = sweep_direction(user_loader, market_index)? else {
        return Ok(Vec::new());
    };
    let slab_key = crate::state::pdas::quoter_slab(market_index);
    let Some(slab_info) = find_account(remaining_accounts, &slab_key) else {
        return Ok(Vec::new());
    };
    let slab = AccountLoader::<QuoterSlabV0>::try_from(slab_info)?;
    // Copied out so no slab borrow lives across the book CPI.
    let book_slot = {
        let slots = slab.slots()?;
        let Some(index) = clob_slot_index(&slots) else {
            return Ok(Vec::new());
        };

        if !slots[index].quotes() {
            return Ok(Vec::new());
        }

        slots[index]
    };
    let (Some(book), Some(program)) = (
        find_account(remaining_accounts, &book_slot.config.response_account),
        find_account(remaining_accounts, &book_slot.config.program_id),
    ) else {
        return Ok(Vec::new());
    };

    let taker = crate::load!(user_loader)?.clob_user_ref();
    let accounts = [book.clone(), program.clone()];
    let mut scratch = QuoterCpiScratch::new();
    let owners = crate::instructions::clob::helpers::crank_common::book_l3_side(
        &book_slot,
        &slab,
        market_index,
        direction,
        BOOK_MAKER_ROWS,
        &accounts,
        &mut scratch,
        // The cover a crossing remainder claims is not depth this fill may
        // take, so the owners behind it are not owners it has to carry.
        false,
        |row| row.user,
    )?
    .unwrap_or_default();

    Ok(owners
        .into_iter()
        .filter(|owner| *owner != taker)
        .fold(Vec::new(), |mut makers, owner| {
            if makers.len() < MAX_LIQUIDATION_MAKERS && !makers.contains(&owner) {
                makers.push(owner);
            }

            makers
        }))
}

/// The side a liquidation of `market_index` sweeps, as the taker direction
/// the book answers on. `None` when the account holds no position there.
fn sweep_direction(
    user_loader: &AccountLoader<'_, User>,
    market_index: u16,
) -> Result<Option<crate::state::prop_amm::DirectionV0>> {
    let user = crate::load!(user_loader)?;
    let Ok(position) = user.get_perp_position(market_index) else {
        return Ok(None);
    };

    Ok(match position.base_asset_amount {
        0 => None,
        // Closing a long means selling, and a seller sweeps the bids.
        base if base > 0 => Some(crate::state::prop_amm::DirectionV0::Short),
        _ => Some(crate::state::prop_amm::DirectionV0::Long),
    })
}
