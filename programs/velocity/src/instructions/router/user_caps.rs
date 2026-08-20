//! Size every maker resting on the route's books before those books are
//! quoted, so a quote never stands on liquidity the fill would refuse.
//!
//! `fulfill_perp_order_post_checks` refuses a fill that leaves a maker short
//! of what it owes, and the refusal takes the whole transaction — the taker
//! and every other maker in it. Until something intervenes the next fill does
//! the same. Handing the book each maker's room turns that from a revert into
//! a skip, and a skip is mid-book: the depth *behind* a maker who is out of
//! room stays quoted and stays fillable.
//!
//! # What the numbers mean
//!
//! Room is measured at the tier the fill will judge by, not the tier a
//! placement would. `select_margin_type_for_perp_maker` answers `Fill` for a
//! maker taking on risk, so that is what a cap is sized against; asking at
//! `Initial` would deny liquidity the fill would have accepted.
//!
//! The reducing direction is left unconstrained. It answers to maintenance
//! margin, a maker reducing is the action the protocol wants, and pricing it
//! would double the walks for the side that almost never refuses.
//!
//! # What they are not
//!
//! Not a trust boundary. A book that ignores a cap leaves velocity exactly
//! where it stands without one — the post-fill checks still refuse the fill.
//! What honouring them buys is that the honest case stops reverting. The one
//! exception is a cap of zero, which also drops that maker from the permitted
//! subject set velocity derives from the book itself, and a response naming
//! someone outside that set is refused outright.

use {
    crate::{
        controller::position::PositionDirection,
        instructions::router::quoted_route::QuoteInputs,
        math::margin::{
            calculate_margin_requirement_and_total_collateral_and_liability_info,
            calculate_net_equity_for_floor, MarginRequirementType,
        },
        state::{
            margin_calculation::{MarginContext, MarginTypeConfig},
            oracle_map::OracleMap,
            perp_market_map::PerpMarketMap,
            prop_amm::{
                clob_resting_prefix, find_account, ClobSide, ClobUserRefV0, QuoterUserCapV0,
                QuoterUserCapsV0, QuoterV0,
            },
            spot_market_map::SpotMarketMap,
            user_map::{UserMap, UserStatsMap},
        },
    },
    anchor_lang::{prelude::*, Discriminator},
};

/// Everything sizing a maker needs that quoting does not.
pub struct CapInputs<'a, 'info> {
    pub makers_and_referrer: &'a UserMap<'info>,
    pub makers_and_referrer_stats: &'a UserStatsMap<'info>,
    pub perp_market_map: &'a PerpMarketMap<'info>,
    pub spot_market_map: &'a SpotMarketMap<'info>,
    pub oracle_map: &'a mut OracleMap<'info>,
    pub slot: u64,
    pub now: i64,
}

/// Room for every constrained maker resting on the route's CLOB books.
pub fn build_user_caps<'info>(
    tail: &'info [AccountInfo<'info>],
    inputs: &QuoteInputs<'_>,
    ctx: &mut CapInputs<'_, 'info>,
) -> Result<QuoterUserCapsV0> {
    // The side a taker of this direction sweeps, which is the only side these
    // books will be asked for. The other stays unconstrained.
    let resting_side = inputs.direction.side();
    let maker_direction = match resting_side {
        ClobSide::Bid => PositionDirection::Long,
        ClobSide::Ask => PositionDirection::Short,
    };

    // Distinct makers actually in reach of this fill, in book order. A maker
    // deeper than the taker's size cannot be filled, so sizing them would be
    // a margin walk spent on nothing.
    // Counted per book, not just listed: one cap goes to every book in the
    // route, so a maker resting on two of them would be offered the same room
    // twice and could take it on each. The executes all run after every quote
    // is taken, so the budget cannot be decremented between them without the
    // second book's execute disagreeing with its own quote. Splitting the
    // room by how many books hold the maker keeps the total inside it.
    let mut reachable: Vec<(ClobUserRefV0, u32)> = Vec::new();
    for info in tail {
        let is_entry = info.owner == &crate::ID
            && info
                .try_borrow_data()
                .is_ok_and(|data| data.get(..8) == Some(QuoterV0::DISCRIMINATOR));
        if !is_entry {
            continue;
        }
        let loader = AccountLoader::<QuoterV0>::try_from(info)?;
        let (is_clob, book_key) = {
            let quoter = loader.load()?;
            (
                quoter.quoter_type == crate::state::prop_amm::QuoterType::Clob
                    && quoter.market == inputs.market_index,
                quoter.response_account,
            )
        };
        if !is_clob {
            continue;
        }
        let Some(book) = find_account(tail, &book_key) else {
            continue;
        };
        let data = book.try_borrow_data()?;
        // One increment per book, however many orders the maker rests on it.
        let mut counted_here: Vec<ClobUserRefV0> = Vec::new();
        for order in clob_resting_prefix(
            &data,
            resting_side,
            inputs.size,
            inputs.users,
            &QuoterUserCapsV0::EMPTY,
            &inputs.taker,
            ctx.slot,
            ctx.now,
        ) {
            match reachable.iter_mut().find(|(seen, _)| *seen == order.user) {
                Some(entry) => {
                    if !counted_here.contains(&order.user) {
                        entry.1 += 1;
                        counted_here.push(order.user);
                    }
                }
                None => {
                    reachable.push((order.user, 1));
                    counted_here.push(order.user);
                }
            }
        }
    }
    if reachable.is_empty() {
        return Ok(QuoterUserCapsV0::EMPTY);
    }

    let mut caps: Vec<QuoterUserCapV0> = Vec::with_capacity(reachable.len());
    for (user_ref, books) in reachable {
        let Some(index) = inputs.users.iter().position(|named| *named == user_ref) else {
            continue;
        };
        let Some(key) = ctx
            .makers_and_referrer
            .0
            .iter()
            .find(|(_, loader)| {
                loader.load().is_ok_and(|maker| {
                    maker.authority == user_ref.authority
                        && u16::from(maker.sub_account_id) == user_ref.sub_account_id
                })
            })
            .map(|(key, _)| *key)
        else {
            continue;
        };
        let room = maker_room(ctx, &key, inputs.market_index, maker_direction)?;
        if room == u64::MAX {
            continue;
        }
        let _ = books;
        caps.push(match resting_side {
            ClobSide::Bid => QuoterUserCapV0 {
                index: index as u8,
                bid_base: room,
                ask_base: u64::MAX,
            },
            ClobSide::Ask => QuoterUserCapV0 {
                index: index as u8,
                bid_base: u64::MAX,
                ask_base: room,
            },
        });
    }
    Ok(QuoterUserCapsV0::from_caps(caps))
}

/// Room this maker has left in `direction`: `0` when the fill would refuse
/// them, `u64::MAX` when nothing does.
///
/// Boolean, not a size, and that is a statement about what a fill does rather
/// than a shortcut. A resting order was margin-reserved when it was placed —
/// the walk prices `open_bids`/`open_asks` at worst case, as though they had
/// already filled — so filling one converts a reservation into a position and
/// barely moves the requirement. It is also judged at `Fill`, which is looser
/// than the `Initial` its placement answered to. A maker who passed placement
/// and has not deteriorated since therefore passes the fill, and one who has
/// deteriorated fails it whatever size is offered.
///
/// Sizing it instead would double-count: subtracting what is already working
/// from a number that was computed with it already subtracted.
pub(crate) fn maker_room(
    ctx: &mut CapInputs<'_, '_>,
    key: &Pubkey,
    market_index: u16,
    _direction: PositionDirection,
) -> Result<u64> {
    let maker = ctx.makers_and_referrer.get_ref(key)?;

    // The two that answer without pricing anything: an authority-wide latch
    // bars every subaccount from risk-increasing activity, and a floor the
    // program cannot verify cannot authorise one either.
    if ctx
        .makers_and_referrer_stats
        .get_ref(&maker.authority)
        .map(|stats| stats.is_equity_breaker_tripped())
        .unwrap_or(false)
    {
        return Ok(0);
    }
    if let Some(net_equity) = calculate_net_equity_for_floor(
        &maker,
        ctx.perp_market_map,
        ctx.spot_market_map,
        ctx.oracle_map,
    )? {
        if !net_equity.all_oracles_valid || net_equity.proves_below_floor(&maker) {
            return Ok(0);
        }
    }

    // And the one that does: the same requirement, at the same tier, that the
    // post-fill check will hold this maker to.
    let margin_type_config = if ctx
        .perp_market_map
        .get_ref(&market_index)
        .is_ok()
        .then(|| maker.get_perp_position(market_index).ok())
        .flatten()
        .is_some_and(|position| position.is_isolated())
    {
        MarginTypeConfig::IsolatedPositionOverride {
            market_index,
            margin_requirement_type: MarginRequirementType::Fill,
            default_isolated_margin_requirement_type: MarginRequirementType::Maintenance,
            cross_margin_requirement_type: MarginRequirementType::Maintenance,
        }
    } else {
        MarginTypeConfig::CrossMarginOverride {
            margin_requirement_type: MarginRequirementType::Fill,
            default_margin_requirement_type: MarginRequirementType::Maintenance,
        }
    };
    let calculation = calculate_margin_requirement_and_total_collateral_and_liability_info(
        &maker,
        ctx.perp_market_map,
        ctx.spot_market_map,
        ctx.oracle_map,
        MarginContext::standard_with_config(margin_type_config)
            .ignore_invalid_deposit_oracles(true),
    )?;
    Ok(if calculation.meets_margin_requirement() {
        u64::MAX
    } else {
        0
    })
}

#[cfg(test)]
mod tests;
