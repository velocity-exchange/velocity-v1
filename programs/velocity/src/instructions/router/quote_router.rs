//! The router's quote view: what a taker of `(direction, size)` can actually
//! get, per source, right now.
//!
//! Quoting every source through *velocity* rather than reading each one
//! directly is what makes the answer trustworthy and uniform:
//!
//! - **Verified, not advertised.** A Custom quoter's book is clamped to what
//!   its `User`'s margin supports before it leaves this instruction (the same
//!   `calculate_max_perp_order_size` bound the fill applies), so phantom depth
//!   never reaches a router's selection or a UI's depth chart. CLOB depth
//!   stands as quoted: it was margin-gated at placement. The fill applies one
//!   further cut this view does not — see the clamp below.
//! - **Quoted as the fill will quote.** The sources are not independent: the
//!   vAMM shades its ladder against rival books (last look), so a vAMM book
//!   quoted in isolation prices better than the same vAMM inside a real fill.
//!   This runs them in fill order — externals, then DLOB makers, then the vAMM
//!   with everything before it as rivals — so published books equal fill-time
//!   books by construction.
//! - **Uniform.** CLOB, PropAMM, DLOB and vAMM all come back as
//!   `(kind, key, priority, levels)` in one buffer, so a consumer has one code
//!   path instead of a decoder per source.
//!
//! Read-only: the vAMM is quoted off a *copy* of the AMM, so `setup`'s
//! projection doesn't touch the market. The only account written is the
//! caller's own quote buffer, which is why this is safe to expose as a view —
//! it is meant to be simulated, and landing it changes nothing that matters.
//!
//! `remaining_accounts`, in order: the oracle/spot/perp map section, then
//! `(User, UserStats)` pairs for DLOB makers *and* for any quoted Custom
//! quoter's user (the clamp needs its account), then the market's
//! `QuoterSlabV0` and the union of the consulted quoters' registered CPI
//! accounts (response accounts, quoter programs, the velocity signer). As in
//! a fill, a slab slot is consulted when its response account rides the
//! call.

use {
    crate::{
        controller::position::PositionDirection,
        error::ErrorCode,
        instructions::optional_accounts::{load_maps, AccountMaps},
        math::{orders::calculate_max_perp_order_size, router::QuoterBook},
        msg,
        state::{
            perp_market_map::{get_writable_perp_market_set, MarketSet},
            prop_amm::{
                find_account, occupied_slots, ClobUserRefV0, Direction, L3ArgsV0, PriceLevel,
                QuoteArgsV0, QuoterSlabExt, QuoterSlabV0, QuoterType, WireDirectionExt,
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
    /// `has_one` pins the writer, and the constraint pins the market.
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
    pub direction: Direction,
    /// Size to quote up to. The books returned are what's available for a
    /// taker of this size — resting sources are merely truncated by it, the
    /// vAMM and PropAMMs genuinely price against it.
    pub size: u64,
    /// Whether the flow this view prices for served a protection window —
    /// the swift hold, or the book's activation delay. What the real route
    /// asks: a bumped book quotes no depth to unprotected flow, and a
    /// protected-flow quoter (the midpoint's `require_attested_flow`)
    /// refuses it, so a view for unprotected flow must show the same books
    /// the fill would get. Swift and the book publisher price protected
    /// flow and pass `true`.
    pub taker_served_window: bool,
    /// Quote the vAMM into the buffer as well.
    ///
    /// A market with more quoters than one view can carry is read in several
    /// passes. The vAMM prices against every other book in the same call, so
    /// a pass holding a subset would shade it against a subset and each pass
    /// would return a different vAMM. Exactly one pass sets this, and the
    /// caller merges the vAMM from that one. The passes that clear it also
    /// stop paying to compute a ladder they would discard.
    pub include_vamm: bool,
}

pub fn handle_quote_router<'c: 'info, 'info>(
    ctx: Context<'info, QuoteRouter<'info>>,
    args: QuoteRouterArgs,
) -> Result<()> {
    let clock = Clock::get()?;
    let state = ctx.accounts.state.load()?;
    let market_index = args.market_index;

    let remaining_accounts_iter = &mut ctx.remaining_accounts.iter().peekable();
    let mut maps: AccountMaps = load_maps(
        remaining_accounts_iter,
        &get_writable_perp_market_set(market_index),
        &MarketSet::new(),
        clock.slot,
        state.slot_clock(),
        Some(state.oracle_guard_rails),
    )?;
    let (makers, _maker_stats) = load_user_maps(remaining_accounts_iter, false)?;

    // Quoter section: the market's slab plus the union of the consulted
    // quoters' CPI accounts.
    let leftover: Vec<&'info AccountInfo<'info>> = remaining_accounts_iter.collect();
    let accounts: Vec<AccountInfo<'info>> = leftover.iter().map(|info| (*info).clone()).collect();
    let slab_loader = find_market_slab(&leftover, market_index)?;

    let mut buffer = ctx.accounts.quote_buffer.load_mut()?;
    buffer.begin(args.direction as u8, args.size, clock.slot);

    quote_externals(
        &args,
        slab_loader.as_ref(),
        &accounts,
        &makers,
        &mut maps,
        &mut buffer,
    )?;

    // ---- DLOB makers next: one level per crossing resting order. ----
    let (oracle_price, amm_snapshot) = {
        let market = maps.perp_market_map.get_ref(&market_index)?;
        let oracle_pd = *maps.oracle_map.get_price_data(&market.oracle_id())?;
        (oracle_pd, market.amm)
    };
    quote_dlob_makers(
        &args,
        &makers,
        &maps.perp_market_map,
        &state,
        oracle_price,
        clock.slot,
        &mut buffer,
    )?;

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
/// The slab is found by its discriminator rather than by position, because
/// the quoter section is a union of account lists whose order the caller
/// chooses. A slab for another market is refused: it would quote another
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
            ErrorCode::DefaultError,
            "quoter slab {} is for market {}, quote is for market {}",
            loader.key(),
            loader.load()?.market,
            market_index
        )?;
        return Ok(Some(loader));
    }
    Ok(None)
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
    /// Where the quoter wrote its ladder.
    located: crate::state::prop_amm::ResponseLocationV0<'info>,
}

/// Quote one slab slot and locate the response it wrote.
///
/// `None` when the slot quotes nothing: it is suspended or deactivated, or a
/// speed bump holds it back.
fn quote_one_slot<'info>(
    args: &QuoteRouterArgs,
    slab_loader: &AccountLoader<'info, QuoterSlabV0>,
    slot_index: usize,
    accounts: &[AccountInfo<'info>],
    scratch: &mut crate::state::prop_amm::QuoterCpiScratch<'info>,
) -> Result<Option<QuotedSlot<'info>>> {
    let market_index = args.market_index;
    let slots = slab_loader.slots()?;
    let slot = &slots[slot_index];
    if !slot.quotes() {
        return Ok(None);
    }
    let entry_key = slot.entry;
    // Maker priority, as the fill's route applies it: a book with a
    // speed bump quotes no depth to unprotected flow, so this view
    // must not show it any.
    if slot.config.quoter_type == QuoterType::Clob
        && !args.taker_served_window
        && slot.config.book_default_activation_delay_slots > 0
    {
        return Ok(None);
    }
    let located = slot
        .config
        .quote_in_place(
            market_index,
            QuoteArgsV0 {
                // The view settles nothing, so it constrains nothing:
                // it reports the book as it stands.
                caps: crate::state::prop_amm::QuoterUserCapsV0::EMPTY,
                // No budgets to price, so nothing reads this.
                reference_price: 0,
                direction: args.direction,
                size: args.size,
                // A view has no settlement, so no loaded-user
                // restriction: quote everything the book holds.
                users: &[],
                taker: None,
                // No taker, so no price to bound the ladder at. A
                // caller reads this view to decide what to route,
                // which needs the depth a bound would cut.
                limit_price: 0,
                taker_served_window: args.taker_served_window,
            },
            slab_loader,
            accounts,
            scratch,
        )
        .map_err(|e| {
            msg!("quoter {} quote failed: {}", entry_key, e);
            ErrorCode::DefaultError
        })?;
    Ok(Some(QuotedSlot {
        priority: slot.config.priority,
        quoter_type: slot.config.quoter_type,
        user: slot.config.user,
        entry: entry_key,
        located,
    }))
}

/// Quote every consulted external quoter into the buffer.
///
/// The externals run first because their books are the vAMM's last look. A
/// book is read straight out of the quoter's response account and copied
/// once, into the buffer. Nothing holds a second copy: velocity's heap is
/// 32 KB and never reclaims, and this runs once per quoter.
fn quote_externals<'info>(
    args: &QuoteRouterArgs,
    slab_loader: Option<&AccountLoader<'info, QuoterSlabV0>>,
    accounts: &[AccountInfo<'info>],
    makers: &crate::state::user_map::UserMap,
    maps: &mut AccountMaps,
    buffer: &mut RouterQuoteBufferV0,
) -> Result<()> {
    // A transaction that carries no slab consults no quoter.
    let Some(slab_loader) = slab_loader else {
        return Ok(());
    };
    let market_index = args.market_index;
    let taker_direction = args.direction.to_position_direction();
    // One set of CPI buffers for every entry this view quotes.
    let mut scratch = crate::state::prop_amm::QuoterCpiScratch::new();
    let consulted: Vec<usize> = {
        let slots = slab_loader.slots()?;
        occupied_slots(&slots)
            .filter(|(_, slot)| find_account(accounts, &slot.config.response_account).is_some())
            .map(|(index, _)| index)
            .collect()
    };
    for slot_index in consulted {
        let Some(quoted) = quote_one_slot(args, slab_loader, slot_index, accounts, &mut scratch)?
        else {
            continue;
        };
        let QuotedSlot {
            priority,
            quoter_type,
            user: quoter_user,
            entry: entry_key,
            located,
        } = quoted;

        // Verification: a Custom quoter's depth is never margin-reserved, so
        // clamp it to what its user can actually support. CLOB depth was
        // gated at placement, so it stands as quoted here. The cap is taken
        // before the response is borrowed, because it reads the maps.
        //
        // The fill cuts a CLOB book once more, at the first order resting
        // under a maker whose equity floor it cannot verify
        // (`clob_unverifiable_floor_depth`). This view does not reproduce
        // that cut: it would have to load every resting maker's `User`, and
        // this instruction carries only the Custom quoters' accounts. The
        // divergence is bounded — a floor is admin-set and only bites while
        // one of that maker's oracles is invalid — and it errs by showing
        // depth the fill routes elsewhere, not by hiding depth that exists.
        // Loading the makers is what it would take to close it.
        let cap = if quoter_type == QuoterType::Custom {
            margin_cap(
                makers,
                &quoter_user,
                market_index,
                taker_direction.opposite(),
                maps,
            )?
        } else {
            u64::MAX
        };

        // The borrow ends with this block, before the next quoter's CPI: a
        // live borrow of a response account would fail the CPI that writes
        // it.
        let admitted = {
            let data = located.borrow()?;
            let response = located.checked_quote_response(&data, args.direction)?;
            buffer.push_capped(
                QuotedSourceKind::Quoter,
                entry_key,
                priority,
                response.levels,
                cap,
            )?;
            buffer
                .levels_for(buffer.source_count as usize - 1)
                .iter()
                .map(|level| level.size)
                .fold(0u64, u64::saturating_add)
        };

        // Who the ladder stands on. A quoter that holds other people's orders
        // says so itself, through the optional third leg; every other quoter
        // fills from the one account the registry names, so its rows say that
        // instead. Either way a reader gets one shape and never has to decode
        // a quoter's account from outside.
        let rows_wanted = buffer.rows_remaining();
        if rows_wanted > 0 {
            // A Custom entry is bound to the user it registered for; a book
            // is not bound to anyone velocity can name, which is the same
            // split settlement makes.
            let bound_to = (quoter_type == QuoterType::Custom)
                .then(|| user_ref(makers, &quoter_user))
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
                &mut scratch,
                bound_to,
                buffer,
            )?;
            if !described {
                attribute_to_user(makers, &quoter_user, admitted, buffer)?;
            }
        }
    }
    Ok(())
}

/// Quote the DLOB makers this call carries: one level for every resting
/// order that crosses.
///
/// Uses the fill path's own discovery predicate, so a book never advertises
/// an order the fill would skip. The predicate refuses a wrong side, a wrong
/// type, an untriggered order and an order that is not open.
fn quote_dlob_makers(
    args: &QuoteRouterArgs,
    makers: &crate::state::user_map::UserMap,
    perp_market_map: &crate::state::perp_market_map::PerpMarketMap,
    state: &State,
    oracle_price: crate::state::oracle::OraclePriceData,
    slot: u64,
    buffer: &mut RouterQuoteBufferV0,
) -> Result<()> {
    let market_index = args.market_index;
    let clob_tier = QuoterType::Clob.default_priority();
    let order_tick_size = perp_market_map.get_ref(&market_index)?.order_tick_size;
    let taker_direction = args.direction.to_position_direction();
    // The fill path's own discovery predicate, so a book never advertises an
    // order the fill would skip (wrong side/type, untriggered, not open).
    let maker_direction = taker_direction.opposite();
    for (maker_key, _) in makers.0.iter() {
        let maker = makers.get_ref(maker_key)?;
        let position_base = maker
            .get_perp_position(market_index)
            .map(|p| p.base_asset_amount)
            .unwrap_or(0);
        let found = crate::math::orders::find_maker_orders(
            &maker,
            &maker_direction,
            &crate::state::user::MarketType::Perp,
            market_index,
            Some(oracle_price.price),
            slot,
            order_tick_size,
            state.slot_clock(),
        )?;
        for (order_index, price) in found {
            let size =
                maker.orders[order_index].get_base_asset_amount_unfilled(Some(position_base))?;
            if size == 0 {
                continue;
            }
            let levels = [PriceLevel { price, size }];
            buffer.push(QuotedSourceKind::DlobOrder, *maker_key, clob_tier, &levels)?;
            buffer.push_row(QuotedRowV0 {
                price,
                size,
                order_id: maker.orders[order_index].order_id.into(),
                // A DLOB order lives in the owner's own array, not an arena.
                node_index: 0,
                authority: maker.authority,
                sub_account_id: maker.sub_account_id,
                flags: 0,
                padding: [0; 1],
                placed_slot: maker.orders[order_index].slot,
            })?;
        }
    }
    Ok(())
}

/// Quote the vAMM into the buffer, with every book already in it as the
/// vAMM's rivals.
///
/// Only on the pass that asked for it. The shading reads every other book in
/// this call. A pass that carries a subset returns a vAMM shaded against a
/// subset, so a caller that reads a market in several passes gets a different
/// vAMM from each.
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
        // Quoted off a copy: `refresh` projects the curve, and this
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
        // the rivals are views onto it rather than copies of it. The two
        // level types are the same 16 bytes — one is the borsh wire's, one is
        // the buffer's Pod form — which is what makes the cast free.
        let rivals: Vec<QuoterBook> = (0..buffer.source_count as usize)
            .map(|index| QuoterBook {
                withheld: crate::state::prop_amm::PriceLevel::default(),
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
/// `false` when the entry declares no `quote_l3_v0` leg, which is every
/// quoter that fills from one account.
#[allow(clippy::too_many_arguments)]
fn quoter_rows<'info>(
    slab_loader: &AccountLoader<'info, QuoterSlabV0>,
    slot_index: usize,
    market_index: u16,
    direction: Direction,
    admitted: u64,
    rows_wanted: usize,
    entry: &Pubkey,
    accounts: &[AccountInfo<'info>],
    scratch: &mut crate::state::prop_amm::QuoterCpiScratch<'info>,
    // The one user a Custom entry may name, `None` for a book. The same rule
    // settlement applies, applied to what the entry says about itself.
    bound_to: Option<ClobUserRefV0>,
    buffer: &mut RouterQuoteBufferV0,
) -> Result<bool> {
    let located = {
        let slots = slab_loader.slots()?;
        slots[slot_index].config.quote_l3(
            market_index,
            L3ArgsV0 {
                direction,
                size: admitted,
                max_rows: rows_wanted.min(u16::MAX as usize) as u16,
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
    // Cut to what the ladder admitted: a book whose depth verification
    // clamped must not name makers whose orders that clamp took away.
    let mut remaining = admitted;
    for row in located.l3_response(&data)?.rows {
        if remaining == 0 {
            break;
        }
        // A quoter that fills from one account may only describe that
        // account. Settlement refuses anything else, so a row naming a
        // stranger is a quoter asking the caller to carry an account it
        // could never move — reported rather than quietly corrected, so the
        // health layer can hold it responsible.
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
fn user_ref(makers: &crate::state::user_map::UserMap, user: &Pubkey) -> Option<ClobUserRefV0> {
    let maker = makers.get_ref(user).ok()?;
    Some(maker.clob_user_ref())
}

/// Record a ladder as one row against the user the registry names for it.
///
/// The whole ladder, because that is what a quoter without orders means: it
/// fills from one account at whatever prices it quoted. The row carries no
/// order id for the same reason. Nothing is recorded when the user's account
/// did not ride the call — the identity a caller needs lives inside it.
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
            // A rung attributed to the quoter's user, not an order: it has no
            // handle and no placement of its own.
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

/// The base a maker can support on `direction` given its margin right now —
/// the same bound the fill's pre-execute clamp uses.
fn margin_cap(
    makers: &crate::state::user_map::UserMap,
    user: &Pubkey,
    market_index: u16,
    maker_direction: PositionDirection,
    maps: &mut AccountMaps,
) -> Result<u64> {
    let position_index = {
        let Ok(maker) = makers.get_ref(user) else {
            // Without the quoter's user account there is nothing to verify
            // against, so the book is unusable rather than trusted.
            msg!("custom quoter user {} not passed; book dropped", user);
            return Ok(0);
        };
        crate::controller::position::get_position_index(&maker.perp_positions, market_index)
    };
    let position_index = match position_index {
        Ok(index) => index,
        // The quoter's user has never traded this market. A fill opens the
        // position slot before clamping (`add_new_position`) — mirror it, or
        // a fresh maker's book quotes zero until its first fill. Requires
        // the user passed writable, as the fill also requires; a read-only
        // view degrades to the old zero-cap behavior.
        Err(_) => match makers.get_ref_mut(user) {
            Ok(mut maker) => {
                match crate::controller::position::add_new_position(
                    &mut maker.perp_positions,
                    market_index,
                ) {
                    Ok(index) => index,
                    Err(_) => return Ok(0), // position slots full
                }
            }
            Err(_) => {
                msg!(
                    "custom quoter user {} read-only with no position; book dropped",
                    user
                );
                return Ok(0);
            }
        },
    };
    let maker = makers.get_ref(user)?;
    Ok(calculate_max_perp_order_size(
        &maker,
        position_index,
        market_index,
        maker_direction,
        maps,
    )?)
}
