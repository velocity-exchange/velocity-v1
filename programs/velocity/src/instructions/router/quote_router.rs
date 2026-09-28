//! The router's quote view. It reports what a taker of `(direction, size)`
//! can get from each source right now.
//!
//! Every source is quoted through velocity rather than read directly, which
//! is what makes the answer verified and uniform.
//!
//! The view verifies depth rather than repeating an advertisement. A Custom
//! quoter's book is clamped to what its `User`'s margin supports before it
//! leaves this instruction. The clamp is the room the fill gives that quoter,
//! so depth that does not exist never reaches a router's selection or a depth
//! chart. Every ladder is also cut at the oracle band as the fill cuts it. CLOB
//! depth otherwise stands as quoted, because it was margin-gated at placement.
//! The fill applies one further per-maker cut this view does not. See
//! `quote_externals`.
//!
//! The view quotes the way the fill will quote. The sources are not
//! independent, because the vAMM shades its ladder against rival books. A vAMM
//! book quoted on its own prices better than the same vAMM inside a real fill.
//! This instruction runs the sources in fill order: the external books, then
//! the vAMM with every earlier book as a rival. The published books equal the
//! fill-time books by construction.
//!
//! The view is uniform. CLOB, PropAMM and vAMM all come back as
//! `(kind, key, priority, levels)` in one buffer, so a consumer needs one code
//! path rather than a decoder per source.
//!
//! The instruction reads state and does not move it. The vAMM is quoted off a
//! copy of the AMM, so the curve projection does not touch the market. The
//! only account written is the caller's own quote buffer. That is why this is
//! safe to expose as a view. It is meant to be simulated, and landing it
//! changes nothing that matters.
//!
//! `remaining_accounts` arrives in this order. First comes the oracle, spot
//! and perp map section. Then come the `(User, UserStats)` pairs for the
//! makers the books name and for any quoted Custom quoter's user, whose
//! account the clamp needs. Last comes the market's `QuoterSlabV0` and the union of the
//! consulted quoters' registered CPI accounts, which are the response
//! accounts, the quoter programs, and the velocity signer. A slab slot is
//! consulted when its response account rides the call, as in a fill.

use {
    super::{
        quoted_route::{book_rests_outside_band, levels_inside_band},
        route_fill::RouteMark,
        user_caps::CapInputs,
    },
    crate::{
        controller::{orders::MatchOracle, position::PositionDirection},
        error::ErrorCode,
        instructions::optional_accounts::{load_maps, AccountMaps},
        math::{casting::Cast, router::QuoterBook},
        msg,
        state::{
            perp_market_map::MarketSet,
            prop_amm::{
                usable_levels, DirectionV0, L3ArgsV0, PriceLevelV0, QuoteArgsV0, QuoterSlabExt,
                QuoterSlabV0, QuoterType, UserRefV0,
            },
            quoter::MarketQuoteInputs,
            router_quote::{QuotedRowV0, QuotedSourceKind, RouterQuoteBufferV0},
            state::State,
            user_map::load_user_maps,
        },
        validate,
        vlp::amm::{quoter::AmmQuoter, router_adapter::vamm_quote_levels, AMM},
    },
    anchor_lang::{prelude::*, Discriminator},
};

#[derive(Accounts)]
#[instruction(args: QuoteRouterArgs)]
pub struct QuoteRouter<'info> {
    pub state: AccountLoader<'info, State>,
    pub authority: Signer<'info>,
    #[account(
        mut,
        has_one = authority,
        constraint = quote_buffer.load()?.market == args.market_index,
    )]
    pub quote_buffer: AccountLoader<'info, RouterQuoteBufferV0>,
}

#[derive(Clone, Copy, AnchorSerialize, AnchorDeserialize)]
pub struct QuoteRouterArgs {
    pub market_index: u16,
    pub direction: DirectionV0,
    /// Size to quote up to. The books returned are what a taker of this size
    /// can get. A resting source is truncated by it. The vAMM and the
    /// PropAMMs price against it.
    pub size: u64,
    /// Whether the flow this view prices for served a protection window: the
    /// swift hold or the book's activation delay. A protected-flow quoter (e.g.
    /// midpoint's `require_attested_flow`) hides depth from unprotected flow, so
    /// the view must match the real route. Swift and the book publisher pass `true`.
    pub taker_served_window: bool,
    /// Quote the vAMM into the buffer as well. A market with more quoters than
    /// one view can carry is read in several passes, and the vAMM shades against
    /// every book in the call, so only one pass sets this flag; the caller merges
    /// the vAMM from that pass, while the rest skip computing a discarded ladder.
    pub include_vamm: bool,
}

pub fn handle_quote_router<'c: 'info, 'info>(
    ctx: Context<'info, QuoteRouter<'info>>,
    args: QuoteRouterArgs,
) -> Result<()> {
    let clock = Clock::get()?;
    // A view writes only its own buffer, which nothing on chain reads back.
    // Asserting it holds the property against the caller rather than against
    // review. The quoter tail is governed separately, by what the registry
    // vetted each quoter to mark writable.
    crate::instructions::constraints::require_view_accounts(
        &ctx.accounts.to_account_infos(),
        &[ctx.accounts.quote_buffer.key()],
    )?;

    let state = ctx.accounts.state.load()?;
    let market_index = args.market_index;

    let remaining_accounts_iter = &mut ctx.remaining_accounts.iter().peekable();
    // Nothing here writes the market. Sizing reads it through `get_ref`, and
    // the only account this instruction writes is its own quote buffer. Asking
    // for it writable would take a write lock on the market for the length of
    // the simulation and buy nothing.
    let mut maps: AccountMaps = load_maps(
        remaining_accounts_iter,
        &MarketSet::new(),
        &MarketSet::new(),
        clock.slot,
        state.slot_clock(),
        Some(state.oracle_guard_rails),
    )?;
    let (makers, maker_stats) = load_user_maps(remaining_accounts_iter, false)?;

    // Quoter section: the market's slab plus the union of the consulted
    // quoters' CPI accounts.
    let leftover: Vec<&'info AccountInfo<'info>> = remaining_accounts_iter.collect();
    let accounts: Vec<AccountInfo<'info>> = leftover.iter().map(|info| (*info).clone()).collect();
    let slab_loader = find_market_slab(&leftover, market_index)?;

    let mut buffer = ctx.accounts.quote_buffer.load_mut()?;
    buffer.begin(args.direction as u8, args.size, clock.slot);

    let oracle = MatchOracle::read(&mut maps, &state, market_index, clock.slot)?;
    let view = FillView {
        mark: RouteMark::read(&mut maps, market_index)?,
        oracle,
    };
    quote_externals(
        &args,
        slab_loader.as_ref(),
        &accounts,
        &view,
        &mut CapInputs {
            makers_and_referrer: &makers,
            makers_and_referrer_stats: &maker_stats,
            maps: &mut maps,
            taker_key: &Pubkey::default(),
            exchange_match_fills_allowed: oracle.exchange_match_fills_allowed,
        },
        &mut buffer,
    )?;

    let (oracle_price, amm_snapshot) = {
        let market = maps.perp_market_map.get_ref(&market_index)?;
        let oracle_pd = *maps.oracle_map.get_price_data(&market.oracle_id())?;
        (oracle_pd, market.amm)
    };

    quote_vamm(
        &args,
        &maps.perp_market_map,
        &state,
        oracle_price,
        amm_snapshot,
        clock.slot,
        &mut buffer,
    )?;

    msg!(
        "quoted {} sources for market {} at size {}",
        buffer.source_count,
        market_index,
        args.size
    );

    Ok(())
}

/// The market's quoter slab, when the call carries one.
///
/// The slab is found by its discriminator rather than by position, because the
/// quoter section is a union of account lists whose order the caller chooses.
/// A slab for another market is refused, because it would quote another
/// market's books into this market's view.
fn find_market_slab<'info>(
    accounts: &[&'info AccountInfo<'info>],
    market_index: u16,
) -> Result<Option<AccountLoader<'info, QuoterSlabV0>>> {
    for info in accounts.iter().copied() {
        let is_slab = info.owner == &crate::ID
            && info
                .try_borrow_data()
                .is_ok_and(|data| data.get(..8) == Some(QuoterSlabV0::DISCRIMINATOR));
        if !is_slab {
            continue;
        }

        let loader = AccountLoader::<QuoterSlabV0>::try_from(info)?;
        validate!(
            loader.load()?.market == market_index,
            ErrorCode::InvalidQuoterConfig,
            "quoter slab {} is for market {}, quote is for market {}",
            loader.key(),
            loader.load()?.market,
            market_index
        )?;

        return Ok(Some(loader));
    }

    Ok(None)
}

/// The market facts a fill would quote this view's books against.
struct FillView {
    mark: RouteMark,
    oracle: MatchOracle,
}

/// One slot's answer to the view: what it quoted, and who it says it is.
struct QuotedSlot<'info> {
    /// Routing tier at a shared price: lower fills first, pro rata within.
    priority: u8,
    quoter_type: QuoterType,
    /// The registry `user`: the margin account a Custom book is clamped to.
    user: Pubkey,
    /// The staging entry's address, which is the quoter's identity in the
    /// buffer and in error messages.
    entry: Pubkey,
    /// The band this quoter declared, or the market's initial margin ratio.
    oracle_band: u32,
    /// Where the quoter wrote its ladder.
    located: crate::state::prop_amm::ResponseLocationV0<'info>,
}

/// Quote one slab slot and locate the response it wrote.
///
/// Returns `None` when the slot quotes nothing. The slot may be suspended or
/// deactivated, or its activation delay may hold it back.
fn quote_one_slot<'info>(
    args: &QuoteRouterArgs,
    slab_loader: &AccountLoader<'info, QuoterSlabV0>,
    slot_index: usize,
    accounts: &[AccountInfo<'info>],
    view: &FillView,
    scratch: &mut crate::state::prop_amm::QuoterCpiScratch<'info>,
) -> Result<Option<QuotedSlot<'info>>> {
    let market_index = args.market_index;
    let slots = slab_loader.slots()?;
    let slot = &slots[slot_index];
    if !slot.quotes() {
        return Ok(None);
    }

    let entry_key = slot.entry;
    // Makers take priority, as the fill's route applies it. A book with an
    // activation delay quotes no depth to unprotected flow, so this view must
    // not show any.
    if slot.config.quoter_type == QuoterType::Clob
        && !args.taker_served_window
        && slot.config.book_default_activation_delay_slots > 0
    {
        return Ok(None);
    }

    let located = slot
        .quote_in_place(
            market_index,
            QuoteArgsV0 {
                // The view settles nothing, so it constrains nothing. It
                // reports the book as it stands.
                caps: crate::state::prop_amm::UserCapsV0::EMPTY,
                // The fill's mark, so a quoter that checks a band against it
                // quotes here what it quotes to a fill. The caps are empty, so
                // no budget is spent.
                reference_price: Some(view.mark.reference_price.cast()?),
                direction: args.direction,
                size: args.size,
                // A view has no settlement, so it applies no loaded-user
                // restriction. It quotes everything the book holds.
                users: &[],
                taker: None,
                // No taker, so there is no price to bound the ladder at. A
                // caller reads this view to decide what to route, which needs
                // the depth a bound would cut.
                limit_price: 0,
                taker_served_window: args.taker_served_window,
                include_taker_origin_reservations: false,
            },
            slab_loader,
            accounts,
            scratch,
        )
        .map_err(|e| {
            msg!("quoter {} quote failed: {}", entry_key, e);
            ErrorCode::FailedQuoterCpi
        })?;
    Ok(Some(QuotedSlot {
        priority: slot.config.priority,
        quoter_type: slot.config.quoter_type,
        user: slot.config.user,
        entry: entry_key,
        oracle_band: slot.config.oracle_band(view.mark.margin_ratio_initial),
        located,
    }))
}

/// Quote every consulted external quoter into the buffer.
///
/// The externals run first, because their books are the vAMM's last look. A
/// book is read from the quoter's response account and copied once, into the
/// buffer. Nothing holds a second copy. Velocity's heap is 32 KB and never
/// reclaims, and this runs once per quoter.
///
/// The view applies the fill's book-wide rules. No book quotes while the
/// oracle refuses a match fill. A ladder is cut at the oracle band as
/// [`fill_admitted_levels`] cuts it. A Custom quoter gets the room the fill
/// gives it. The fill also caps a book maker by that maker's own budget. The
/// view skips that cap, because it names no taker and loads no set of makers.
fn quote_externals<'info>(
    args: &QuoteRouterArgs,
    slab_loader: Option<&AccountLoader<'info, QuoterSlabV0>>,
    accounts: &[AccountInfo<'info>],
    view: &FillView,
    sizing: &mut CapInputs<'_, 'info>,
    buffer: &mut RouterQuoteBufferV0,
) -> Result<()> {
    let Some(slab_loader) = slab_loader else {
        return Ok(());
    };

    if !view.oracle.safe_match_fills_allowed {
        return Ok(());
    }

    let market_index = args.market_index;
    let maker_direction = PositionDirection::from(args.direction).opposite();
    let mut cpi_scratch = crate::state::prop_amm::QuoterCpiScratch::new();
    // The fill's own rule, so the view consults the slots a fill would and
    // refuses a tail a fill would refuse.
    let consulted = slab_loader.consulted_slots(accounts)?;

    for &slot_index in &consulted {
        let Some(quoted) = quote_one_slot(
            args,
            slab_loader,
            slot_index,
            accounts,
            view,
            &mut cpi_scratch,
        )?
        else {
            continue;
        };

        let QuotedSlot {
            priority,
            quoter_type,
            user: quoter_user,
            entry: entry_key,
            oracle_band,
            located,
        } = quoted;

        // The room is taken before the response is borrowed, because sizing
        // reads the maps.
        let cap = if quoter_type == QuoterType::Custom {
            custom_quoter_room(
                sizing,
                slab_loader,
                &consulted,
                &quoter_user,
                market_index,
                maker_direction,
            )?
        } else {
            u64::MAX
        };

        // The borrow ends with this block, before the next quoter's CPI. A
        // live borrow of a response account would fail the CPI that writes
        // it.
        let admitted = {
            let data = located.borrow()?;
            let response = located.checked_quote_response(&data, args.direction)?;
            let levels = fill_admitted_levels(
                usable_levels(response.levels),
                quoter_type,
                maker_direction,
                view.oracle.band_price,
                oracle_band,
            )?;
            buffer.push_capped(QuotedSourceKind::Quoter, entry_key, priority, levels, cap)?;

            buffer
                .levels_for(buffer.source_count as usize - 1)
                .iter()
                .map(|level| level.size)
                .fold(0u64, u64::saturating_add)
        };

        let rows_wanted = buffer.rows_remaining();
        if rows_wanted > 0 {
            // Who the ladder stands on: a Custom entry is bound to the user it
            // registered for, matching settlement's split. A quoter holding
            // other people's orders says so itself through the optional third
            // leg; every other quoter's rows point at the registry's one account.
            let bound_to = (quoter_type == QuoterType::Custom)
                .then(|| user_ref(sizing.makers_and_referrer, &quoter_user))
                .flatten();
            let described = quoter_rows(
                slab_loader,
                slot_index,
                market_index,
                args.direction,
                admitted,
                rows_wanted,
                &entry_key,
                accounts,
                &mut cpi_scratch,
                bound_to,
                buffer,
            )?;

            if !described {
                attribute_to_user(sizing.makers_and_referrer, &quoter_user, admitted, buffer)?;
            }
        }
    }

    Ok(())
}

/// Quote the vAMM into the buffer, with every book already in it as the
/// vAMM's rivals.
///
/// This runs only on the pass that asked for it. The shading reads every other
/// book in this call. A pass that carries a subset returns a vAMM shaded
/// against a subset, so a caller that reads a market in several passes gets a
/// different vAMM from each pass.
fn quote_vamm(
    args: &QuoteRouterArgs,
    perp_market_map: &crate::state::perp_market_map::PerpMarketMap,
    state: &State,
    oracle_price: crate::state::oracle::OraclePriceData,
    amm_snapshot: AMM,
    slot: u64,
    buffer: &mut RouterQuoteBufferV0,
) -> Result<()> {
    if !args.include_vamm {
        return Ok(());
    }

    let market_index = args.market_index;
    {
        // Quoted off a copy. `AmmQuoter::refresh` projects the curve, and this
        // instruction must not move the market's AMM.
        let mut amm: AMM = amm_snapshot;
        let inputs = {
            let market = perp_market_map.get_ref(&market_index)?;
            MarketQuoteInputs::load(
                &market,
                oracle_price,
                slot,
                &state.oracle_guard_rails.validity,
                state.slot_clock(),
            )?
        };

        // Every book quoted above is already in the buffer, in fill order, so
        // the rivals are views onto it rather than copies of it. The two level
        // types are the same 16 bytes. One is the borsh wire form and one is
        // the buffer's Pod form, which is what makes the cast free.
        let rivals: Vec<QuoterBook> = (0..buffer.source_count as usize)
            .map(|index| QuoterBook {
                withheld: crate::state::prop_amm::PriceLevelV0::default(),
                priority: buffer.sources[index].priority,
                levels: bytemuck::cast_slice(buffer.levels_for(index)),
            })
            .collect();
        let ctx = inputs.ctx(slot);
        let amm_levels = {
            let mut quoter = AmmQuoter::for_amm(&mut amm);
            quoter.refresh(&ctx)?;
            vamm_quote_levels(
                quoter.amm,
                args.direction,
                args.size,
                ctx.step_size,
                &rivals,
                None,
            )?
        };

        buffer.push(
            QuotedSourceKind::Vamm,
            perp_market_map.get_ref(&market_index)?.pubkey,
            QuoterType::Vamm.default_priority(),
            &amm_levels,
        )?;
    }

    Ok(())
}

/// Ask a quoter which orders its ladder stands on, and record them.
///
/// Returns `false` when the entry declares no `quote_l3_v0` leg, which is every
/// quoter that fills from one account.
#[allow(clippy::too_many_arguments)]
fn quoter_rows<'info>(
    slab_loader: &AccountLoader<'info, QuoterSlabV0>,
    slot_index: usize,
    market_index: u16,
    direction: DirectionV0,
    admitted: u64,
    rows_wanted: usize,
    entry: &Pubkey,
    accounts: &[AccountInfo<'info>],
    scratch: &mut crate::state::prop_amm::QuoterCpiScratch<'info>,
    // The one user a Custom entry may name, `None` for a book. This is the
    // rule settlement applies, applied to what the entry says about itself.
    bound_to: Option<UserRefV0>,
    buffer: &mut RouterQuoteBufferV0,
) -> Result<bool> {
    let located = {
        let slots = slab_loader.slots()?;
        slots[slot_index].quote_l3(
            market_index,
            L3ArgsV0 {
                direction,
                size: admitted,
                max_rows: rows_wanted.min(u16::MAX as usize) as u16,
                include_taker_origin_reservations: false,
            },
            slab_loader,
            accounts,
            scratch,
        )?
    };
    let Some(located) = located else {
        return Ok(false);
    };
    let data = located.borrow()?;
    // Cut to what the ladder admitted. A book whose depth verification clamped
    // must not name makers whose orders that clamp took away.
    let mut remaining = admitted;
    for row in located.l3_response(&data)?.rows {
        if remaining == 0 {
            break;
        }

        // A quoter that fills from one account may only describe that account.
        // Settlement refuses anything else, so a row that names a stranger is a
        // quoter that asks the caller to carry an account it could never move.
        // The row is reported rather than corrected, so the health layer can
        // hold the quoter responsible.
        if let Some(bound_to) = bound_to {
            validate!(
                row.user == bound_to,
                ErrorCode::QuoterSubjectNotPermitted,
                "quoter {} described a row for user {}/{}, which it cannot settle",
                entry,
                row.user.authority,
                row.user.sub_account_id
            )?;
        }

        let size = row.size.min(remaining);
        if !buffer.push_row(QuotedRowV0 {
            price: row.price,
            size,
            order_id: row.order_id,
            node_index: row.node_index,
            authority: row.user.authority,
            sub_account_id: row.user.sub_account_id,
            flags: row.flags,
            padding: [0; 1],
            placed_slot: row.placed_slot,
        })? {
            break;
        }

        remaining -= size;
    }

    Ok(true)
}

/// The loaded user's identity in derivable form, `None` when the call did not
/// carry its account.
fn user_ref(makers: &crate::state::user_map::UserMap, user: &Pubkey) -> Option<UserRefV0> {
    let maker = makers.get_ref(user).ok()?;
    Some(maker.clob_user_ref())
}

/// Record a ladder as one row against the user the registry names for it.
///
/// It records the whole ladder, because that is what a quoter without orders
/// means. Such a quoter fills from one account at whatever prices it quoted.
/// The row carries no order id for the same reason. Nothing is recorded when
/// the user's account did not ride the call, because the identity a caller
/// needs lives inside that account.
fn attribute_to_user(
    makers: &crate::state::user_map::UserMap,
    user: &Pubkey,
    admitted: u64,
    buffer: &mut RouterQuoteBufferV0,
) -> Result<()> {
    if admitted == 0 {
        return Ok(());
    }

    let Some(user) = user_ref(makers, user) else {
        return Ok(());
    };
    let index = buffer.source_count as usize - 1;
    let levels: Vec<(u64, u64)> = buffer
        .levels_for(index)
        .iter()
        .map(|level| (level.price, level.size))
        .collect();
    for (price, size) in levels {
        if !buffer.push_row(QuotedRowV0 {
            price,
            size,
            order_id: 0,
            // A rung attributed to the quoter's user, not to an order. It has
            // no handle and no placement of its own.
            node_index: 0,
            authority: user.authority,
            sub_account_id: user.sub_account_id,
            flags: 0,
            padding: [0; 1],
            placed_slot: 0,
        })? {
            break;
        }
    }

    Ok(())
}

/// The levels of a ladder the fill would keep at the oracle band.
///
/// A Custom ladder ends at its first level outside the band, as
/// `trim_to_quoter_room` ends it. A book with any level outside the band
/// offers nothing, as `book_rests_outside_band` decides for the fill.
fn fill_admitted_levels(
    levels: &[PriceLevelV0],
    quoter_type: QuoterType,
    maker_direction: PositionDirection,
    band_oracle_price: i64,
    oracle_band: u32,
) -> Result<&[PriceLevelV0]> {
    let kept = match quoter_type {
        QuoterType::Custom => {
            levels_inside_band(levels, maker_direction, band_oracle_price, oracle_band)?
        }
        QuoterType::Clob
            if book_rests_outside_band(
                levels,
                maker_direction,
                band_oracle_price,
                oracle_band,
            )? =>
        {
            0
        }
        _ => levels.len(),
    };

    Ok(&levels[..kept])
}

/// The base a Custom quoter's user may take on through this slot.
///
/// It is the fill's room: `quoter_base_room`, split evenly over every consulted
/// unreserved slot that settles for the same user.
fn custom_quoter_room(
    sizing: &mut CapInputs,
    slab_loader: &AccountLoader<QuoterSlabV0>,
    consulted: &[usize],
    user: &Pubkey,
    market_index: u16,
    maker_direction: PositionDirection,
) -> Result<u64> {
    if sizing.makers_and_referrer.get_ref(user).is_err() {
        // Without the quoter's user account there is nothing to verify
        // against, so the book is unusable rather than trusted.
        msg!("custom quoter user {} not passed; book dropped", user);
        return Ok(0);
    }

    let slots_for_user = {
        let slots = slab_loader.slots()?;
        consulted
            .iter()
            .filter(|&&index| {
                let config = &slots[index].config;
                !config.quoter_type.depth_is_margin_reserved() && config.user == *user
            })
            .count() as u64
    };
    let room = sizing.quoter_base_room(user, market_index, maker_direction)?;
    Ok(room / slots_for_user.max(1))
}

#[cfg(test)]
mod tests {
    use {
        super::fill_admitted_levels,
        crate::{
            controller::position::PositionDirection,
            math::constants::{MARGIN_PRECISION, PRICE_PRECISION_U64 as PRICE},
            state::prop_amm::{PriceLevelV0, QuoterType},
        },
    };

    /// Asks at 99, then 90, then 101, against a 5% band around 100.
    fn kept(quoter_type: QuoterType) -> usize {
        let levels: Vec<PriceLevelV0> = [99, 90, 101]
            .iter()
            .map(|price| PriceLevelV0 {
                price: price * PRICE,
                size: 1,
            })
            .collect();
        fill_admitted_levels(
            &levels,
            quoter_type,
            PositionDirection::Short,
            (100 * PRICE) as i64,
            MARGIN_PRECISION / 20,
        )
        .unwrap()
        .len()
    }

    /// The view keeps what the fill keeps. A Custom ladder ends at its first
    /// level outside the band, and a book with such a level offers nothing.
    #[test]
    fn the_view_cuts_a_ladder_at_the_band_as_the_fill_does() {
        assert_eq!(kept(QuoterType::Custom), 1);
        assert_eq!(kept(QuoterType::Clob), 0);
        assert_eq!(kept(QuoterType::Vamm), 3);
    }
}
