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
        controller::liquidation::LiquidationSizing,
        error::ErrorCode,
        instructions::optional_accounts::{load_maps, AccountMaps},
        math::{
            casting::Cast,
            margin::calculate_margin_requirement_and_total_collateral_and_liability_info,
            oracle::{is_oracle_valid_for_action, VelocityAction},
            position::calculate_base_asset_value_with_oracle_price,
        },
        state::{
            clob_crank::LIQUIDATION_FLAT_PAYMENT_MIN_FILLED_QUOTE,
            liquidation_mode::get_perp_liquidation_mode,
            margin_calculation::{MarginContext, MarketIdentifier},
            market_status::MarketStatus,
            perp_market_map::MarketSet,
            prop_amm::QuoterSlabExt,
            state::State,
            user::{MarketType, User},
            user_conditions::UserConditionsV0,
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

        if !liquidation_can_pay_the_crank(
            &ctx.accounts.user,
            &mut maps,
            &state,
            market_index,
            clock.slot,
        )? {
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
/// side this account may no longer rest. The grounds are the initial margin
/// requirement and a provable floor breach. The executor measures them with the
/// same `ForceCancelGrounds::measure`, so the two agree on every account.
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

/// Whether a liquidation call on this market can pay the crank that runs it.
///
/// A relay crank that pays nothing fails its own payment guard, after the
/// liquidation already ran. A call that sweeps book orders earns the
/// force-cancel figure. Any other call earns the flat payment only for a fill
/// at or above [`LIQUIDATION_FLAT_PAYMENT_MIN_FILLED_QUOTE`]. That fill is sized
/// here the way the executor sizes its forced order, and priced at an oracle
/// the executor would accept. An account whose call cannot pay stays with the
/// keeper bots, which carry a balance sheet and do not need the reservoir.
fn liquidation_can_pay_the_crank(
    user_loader: &AccountLoader<'_, User>,
    maps: &mut AccountMaps,
    state: &State,
    market_index: u16,
    slot: u64,
) -> Result<bool> {
    let user = crate::load!(user_loader)?;
    if sweeps_book_orders(&user, market_index)? {
        return Ok(true);
    }

    let Some(oracle_price) = liquidation_oracle_price(maps, market_index)? else {
        return Ok(false);
    };

    // The executor latches the account before it sizes the order, and the
    // latch resets the pace of an account that was not latched. A copy takes
    // the same step.
    let mut projected = heap_copy(&user);
    drop(user);
    let liquidation_mode = get_perp_liquidation_mode(&projected, market_index)?;
    liquidation_mode.enter_liquidation(&mut projected, slot)?;

    let margin_calculation = calculate_margin_requirement_and_total_collateral_and_liability_info(
        &projected,
        maps,
        MarginContext::liquidation(state.liquidation_margin_buffer_ratio)
            .track_market_margin_requirement(MarketIdentifier::perp(market_index))?,
    )?;
    let sizing = LiquidationSizing {
        user: &projected,
        market_index,
        liquidation_mode: liquidation_mode.as_ref(),
        margin_calculation: &margin_calculation,
        oracle_price,
    };
    // A size the executor cannot compute is a call that fails, such as one
    // under a state with no liquidation margin buffer. That is no work.
    let Ok(Some(size)) = sizing.size(maps, state, slot) else {
        return Ok(false);
    };

    let fill_notional =
        calculate_base_asset_value_with_oracle_price(size.base_asset_amount.cast()?, oracle_price)?;
    Ok(fill_notional >= u128::from(LIQUIDATION_FLAT_PAYMENT_MIN_FILLED_QUOTE))
}

/// A copy of `user` that never passes through the stack.
///
/// `Box::new(*user)` builds the copy in the caller's frame first. A `User` is
/// larger than the 4 KB SBF frame, so that overruns the frame, and the program
/// faults at run time with a bad call target rather than an error.
fn heap_copy(user: &User) -> Box<User> {
    // SAFETY: `User` is a zero-copy `Pod` account, so all-zero bytes are a
    // valid value.
    let mut copy = unsafe { Box::<User>::new_zeroed().assume_init() };
    bytemuck::bytes_of_mut(&mut *copy).copy_from_slice(bytemuck::bytes_of(user));
    copy
}

/// Whether a liquidation of `market_index` takes book orders off a book. The
/// executor sweeps every market in the liquidation's scope.
fn sweeps_book_orders(user: &User, market_index: u16) -> Result<bool> {
    let isolated = user.get_perp_position(market_index)?.is_isolated();
    Ok(user
        .perp_positions
        .iter()
        .filter(|position| !position.is_available())
        .filter(|position| {
            if isolated {
                position.market_index == market_index
            } else {
                !position.is_isolated()
            }
        })
        .any(|position| user.clob_resident_open_orders(position.market_index) > 0))
}

/// The price the executor liquidates `market_index` at, or `None` when the
/// oracle is not valid for a liquidation. A settling market uses its committed
/// expiry price, as the executor does.
fn liquidation_oracle_price(maps: &mut AccountMaps, market_index: u16) -> Result<Option<i64>> {
    let market = maps.perp_market_map.get_ref(&market_index)?;
    if market.status == MarketStatus::Settlement {
        return Ok(Some(market.expiry_price));
    }

    let (price_data, validity) = maps.oracle_map.get_price_data_and_validity(
        MarketType::Perp,
        market_index,
        &market.oracle_id(),
        market
            .market_stats
            .historical_oracle_data
            .last_oracle_price_twap,
        market.get_max_confidence_interval_multiplier()?,
        0,
        0,
        None,
    )?;

    let valid = is_oracle_valid_for_action(validity, Some(VelocityAction::Liquidate))?;
    Ok(valid.then_some(price_data.price))
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

#[cfg(test)]
mod pay_check_tests {
    use {
        super::liquidation_can_pay_the_crank,
        crate::{
            create_anchor_account_info,
            instructions::optional_accounts::AccountMaps,
            math::{
                constants::{
                    AMM_RESERVE_PRECISION, BASE_PRECISION_I64, LIQUIDATION_FEE_PRECISION,
                    LIQUIDATION_PCT_PRECISION, PEG_PRECISION, PRICE_PRECISION_I64,
                    QUOTE_PRECISION_I64, SPOT_BALANCE_PRECISION_U64,
                    SPOT_CUMULATIVE_INTEREST_PRECISION, SPOT_WEIGHT_PRECISION,
                },
                time::{legacy_slot_duration_u8, SlotClock},
            },
            state::{
                market_status::MarketStatus,
                oracle::{HistoricalOracleData, OracleSource},
                oracle_map::OracleMap,
                perp_market::{MarketStats, PerpMarket, AMM},
                perp_market_map::PerpMarketMap,
                pyth_lazer_oracle::PythLazerOracle,
                spot_market::{SpotBalanceType, SpotMarket},
                spot_market_map::SpotMarketMap,
                state::State,
                user::{PerpPosition, SpotPosition, User},
            },
            test_utils::{get_positions, get_pyth_price, get_spot_positions},
        },
        anchor_lang::prelude::{AccountLoader, Pubkey},
    };

    /// A one-unit long at $100 with `deposit` quote behind it, asked whether
    /// the liquidation call relay would stage can pay its crank.
    fn can_pay(deposit_scaled: u64) -> bool {
        let slot = 100_u64;
        let mut oracle_price = get_pyth_price(100, 6);
        oracle_price.posted_slot = slot;
        let oracle_key = Pubkey::new_unique();
        create_anchor_account_info!(oracle_price, &oracle_key, PythLazerOracle, oracle_info);
        let oracle_map =
            OracleMap::load_one(&oracle_info, slot, SlotClock::baseline(), None).unwrap();

        let mut market = PerpMarket {
            amm: AMM {
                base_asset_reserve: 100 * AMM_RESERVE_PRECISION,
                quote_asset_reserve: 100 * AMM_RESERVE_PRECISION,
                sqrt_k: 100 * AMM_RESERVE_PRECISION,
                peg_multiplier: 100 * PEG_PRECISION,
                ..AMM::default()
            },
            margin_ratio_initial: 1000,
            margin_ratio_maintenance: 500,
            status: MarketStatus::Active,
            liquidator_fee: LIQUIDATION_FEE_PRECISION / 100,
            if_liquidation_fee: LIQUIDATION_FEE_PRECISION / 100,
            order_step_size: 10_000_000,
            order_tick_size: 1,
            oracle: oracle_key,
            oracle_source: OracleSource::PythLazer,
            market_stats: MarketStats {
                historical_oracle_data: HistoricalOracleData::default_price(oracle_price.price),
                ..MarketStats::default()
            },
            ..PerpMarket::default()
        };
        create_anchor_account_info!(market, PerpMarket, market_info);
        let mut spot_market = SpotMarket {
            market_index: 0,
            oracle_source: OracleSource::QuoteAsset,
            cumulative_deposit_interest: SPOT_CUMULATIVE_INTEREST_PRECISION,
            decimals: 6,
            initial_asset_weight: SPOT_WEIGHT_PRECISION,
            historical_oracle_data: HistoricalOracleData {
                last_oracle_price_twap: PRICE_PRECISION_I64,
                last_oracle_price_twap_5min: PRICE_PRECISION_I64,
                ..HistoricalOracleData::default()
            },
            ..SpotMarket::default()
        };
        create_anchor_account_info!(spot_market, SpotMarket, spot_market_info);
        let mut maps = AccountMaps::new(
            PerpMarketMap::load_one(&market_info, true).unwrap(),
            SpotMarketMap::load_one(&spot_market_info, true).unwrap(),
            oracle_map,
        );

        let mut user = User {
            perp_positions: get_positions(PerpPosition {
                market_index: 0,
                base_asset_amount: BASE_PRECISION_I64,
                quote_asset_amount: -100 * QUOTE_PRECISION_I64,
                ..PerpPosition::default()
            }),
            spot_positions: get_spot_positions(SpotPosition {
                market_index: 0,
                balance_type: SpotBalanceType::Deposit,
                scaled_balance: deposit_scaled,
                ..SpotPosition::default()
            }),
            ..User::default()
        };
        create_anchor_account_info!(user, User, user_info);
        let user_loader = AccountLoader::<User>::try_from(&user_info).unwrap();
        let state = State {
            liquidation_margin_buffer_ratio: 10,
            initial_pct_to_liquidate: LIQUIDATION_PCT_PRECISION as u16,
            liquidation_duration: legacy_slot_duration_u8(150),
            ..Default::default()
        };

        liquidation_can_pay_the_crank(&user_loader, &mut maps, &state, 0, slot).unwrap()
    }

    /// The whole $100 position clears the payment floor, but a shortage of a
    /// few cents sizes a fill of a few dollars. That fill earns no flat
    /// payment, so relay must not stage it.
    #[test]
    fn a_shallow_shortage_fills_too_little_to_pay() {
        assert!(!can_pay(49 * SPOT_BALANCE_PRECISION_U64 / 10));
    }

    #[test]
    fn a_deep_shortage_fills_enough_to_pay() {
        assert!(can_pay(SPOT_BALANCE_PRECISION_U64));
    }
}
