//! Resolver for a distress threshold. It works out which stage of the ladder
//! the account is in now, and stages the call that matches.
//!
//! A force cancel comes first when it has work. It answers to the initial
//! margin requirement, so it reaches a failing account before a liquidation
//! does, and it takes only the side of a book that adds risk. A latched account
//! skips it, because each liquidation call cancels the orders in its own scope.
//!
//! The liquidation stage runs the full maintenance-margin calculation with the
//! code the executor runs. It stages the largest position in a scope that
//! fails, because the executor liquidates the scope of the market it is given.
//! An account that is not liquidatable returns NoWork, and the poll wakes the
//! resolver again later.

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

        let Some(market_index) = failing_perp_market(
            &ctx.accounts.user,
            &mut maps,
            state.liquidation_margin_buffer_ratio,
        )?
        else {
            // No failing scope holds a perp position. The account is healthy, or its distress is
            // spot-only, which no crank can act on. `liquidate_spot` gives the liquidator the borrow and the
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

/// Stage one of the ladder. Finds a book that still holds a risk-increasing
/// side this account may no longer rest. The grounds are recomputed here, from
/// the initial margin requirement and a provable floor breach. The executor
/// accepts a third ground, the authority-wide equity breaker, which this
/// resolver does not read.
fn find_cancel_target(
    user_loader: &AccountLoader<'_, User>,
    maps: &mut AccountMaps,
) -> Result<Option<u16>> {
    let user = crate::load!(user_loader)?;
    if user.is_being_liquidated() || user.is_bankrupt() {
        return Ok(None);
    }

    let grounds = crate::controller::orders::ForceCancelGrounds::measure(&user, maps)?;
    if !grounds.any() {
        return Ok(None);
    }

    // One market per wake. Relay comes back for the rest while the account
    // still qualifies.
    for position in user.perp_positions.iter().filter(|p| !p.is_available()) {
        let market_index = position.market_index;
        if user.clob_resident_open_orders(market_index) > 0
            && crate::instructions::clob::sweep_has_work(&user, market_index)
            && !grounds.market_recoverable(&user, market_index)?
        {
            return Ok(Some(market_index));
        }
    }

    Ok(None)
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
    let (protocol_user, _) = crate::state::pdas::protocol_user_pair();
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

/// The market to liquidate: the largest perp position in a scope that fails
/// the maintenance calculation the executor runs. A latched scope fails until
/// it clears the buffer the executor exits at.
fn failing_perp_market(
    user_loader: &AccountLoader<'_, User>,
    maps: &mut AccountMaps,
    liquidation_margin_buffer_ratio: u32,
) -> Result<Option<u16>> {
    let user = crate::load!(user_loader)?;
    let calculation = calculate_margin_requirement_and_total_collateral_and_liability_info(
        &user,
        maps,
        MarginContext::liquidation(liquidation_margin_buffer_ratio),
    )?;

    let cross_fails = if user.is_cross_margin_being_liquidated() {
        !calculation.can_exit_cross_margin_liquidation()?
    } else {
        !calculation.meets_cross_margin_requirement()
    };

    let mut largest_failing: Option<(u64, u16)> = None;
    for position in user
        .perp_positions
        .iter()
        .filter(|p| p.base_asset_amount != 0)
    {
        let market_index = position.market_index;
        let scope_fails = if !position.is_isolated() {
            cross_fails
        } else if user.is_isolated_margin_being_liquidated(market_index)? {
            !calculation.can_exit_isolated_margin_liquidation(market_index)?
        } else {
            !calculation.meets_isolated_margin_requirement(market_index)?
        };

        let size = position.base_asset_amount.unsigned_abs();
        if scope_fails && largest_failing.is_none_or(|(largest, _)| size > largest) {
            largest_failing = Some((size, market_index));
        }
    }

    Ok(largest_failing.map(|(_, market_index)| market_index))
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
    // books of other markets it sweeps, the `(User, UserStats)` pairs it may
    // settle against, then the quoter tail.
    let map_section = map_section.min(stored.len());
    let tail = split_stored_tail(
        &*crate::load!(user_loader)?,
        &remaining_accounts[map_section.min(remaining_accounts.len())..],
        &stored[map_section..],
        market_index,
    )?;
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
    .refs(stored[..map_section].iter().copied())
    .refs(tail.foreign_books)
    .maker_refs(makers)
    .refs(tail.route)
    .arg(market_index)
}

/// The stored list's tail, split for a liquidation of one market.
struct LiquidationTail {
    /// The `(slab, book)` pairs of the other markets in the liquidation's
    /// scope where the account rests book orders.
    foreign_books: Vec<relay_spec::AccountRefV0>,
    /// Everything else. The route refuses a slab of another market, so no
    /// such slab stays here.
    route: Vec<relay_spec::AccountRefV0>,
}

/// Split the stored tail. The sync stores each slab with its book right after
/// it, and `tail_accounts` holds the same accounts as `tail_refs`, in order.
fn split_stored_tail<'info>(
    user: &User,
    tail_accounts: &'info [AccountInfo<'info>],
    tail_refs: &[relay_spec::AccountRefV0],
    market_index: u16,
) -> Result<LiquidationTail> {
    let isolated = user.get_perp_position(market_index)?.is_isolated();
    let swept_with_this_market = |other: u16| {
        !isolated
            && user.clob_resident_open_orders(other) > 0
            && user
                .get_perp_position(other)
                .is_ok_and(|position| !position.is_isolated())
    };

    let mut tail = LiquidationTail {
        foreign_books: Vec::new(),
        route: Vec::new(),
    };
    let mut index = 0;
    while index < tail_refs.len() {
        let foreign_market = tail_accounts.get(index).and_then(|info| {
            let slab =
                AccountLoader::<crate::state::prop_amm::QuoterSlabV0>::try_from(info).ok()?;
            let market = slab.load().ok()?.market;
            (market != market_index).then_some(market)
        });

        let Some(other) = foreign_market else {
            tail.route.push(tail_refs[index]);
            index += 1;
            continue;
        };

        let pair_end = (index + 2).min(tail_refs.len());
        if swept_with_this_market(other) {
            tail.foreign_books
                .extend_from_slice(&tail_refs[index..pair_end]);
        }

        index = pair_end;
    }

    Ok(tail)
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

#[cfg(test)]
mod tests {
    use {
        super::find_cancel_target,
        crate::{
            create_anchor_account_info,
            instructions::optional_accounts::AccountMaps,
            state::{
                oracle_map::OracleMap,
                perp_market_map::PerpMarketMap,
                spot_market_map::SpotMarketMap,
                user::{PerpPosition, User, UserStatus},
            },
        },
        anchor_lang::prelude::AccountLoader,
    };

    /// A latched account has no cancel stage. Its measure refuses it, so the
    /// resolver must not ask, or relay never stages the next step.
    #[test]
    fn a_latched_account_continues_to_the_liquidation_stage() {
        let mut user = User::default();
        user.add_user_status(UserStatus::BeingLiquidated);
        user.perp_positions[0] = PerpPosition {
            market_index: 0,
            open_orders: 1,
            open_bids: 1,
            ..PerpPosition::default()
        };
        create_anchor_account_info!(user, User, user_account_info);
        let user_loader = AccountLoader::<User>::try_from(&user_account_info).unwrap();
        let mut maps = AccountMaps::new(
            PerpMarketMap::empty(),
            SpotMarketMap::empty(),
            OracleMap::empty(),
        );

        assert_eq!(find_cancel_target(&user_loader, &mut maps).unwrap(), None);
    }
}
