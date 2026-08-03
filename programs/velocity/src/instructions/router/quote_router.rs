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
//!   needs no clamp — it was margin-gated at placement.
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
//! quoter's user (the clamp needs its account), then `QuoterV0` entries, then
//! the union of their registered CPI accounts (response accounts, quoter
//! programs, the velocity signer).

use {
    crate::{
        controller::position::PositionDirection,
        error::ErrorCode,
        instructions::optional_accounts::{load_maps, AccountMaps},
        math::{orders::calculate_max_perp_order_size, router::QuoterBook},
        msg,
        state::{
            perp_market_map::{get_writable_perp_market_set, MarketSet},
            prop_amm::{Direction, PriceLevel, QuoteArgsV0, QuoterType, QuoterUserSetV0, QuoterV0},
            quoter::MarketQuoteInputs,
            router_quote::{QuotedSourceKind, RouterQuoteBufferV0},
            state::State,
            user_map::load_user_maps,
        },
        validate,
        vlp::amm::{quoter::AmmQuoter, router_adapter::vamm_quote_levels, AMM},
    },
    anchor_lang::prelude::*,
    std::collections::BTreeMap,
};

#[derive(Accounts)]
pub struct QuoteRouter<'info> {
    pub state: AccountLoader<'info, State>,
    pub authority: Signer<'info>,
    /// `has_one` pins the writer; the market is checked in the handler
    /// because it comes in as an argument, not an account.
    #[account(mut, has_one = authority)]
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
    /// `QuoterV0` entries at the head of the quoter section of
    /// `remaining_accounts`; the rest of that section is their CPI accounts.
    pub quoter_count: u8,
}

pub fn handle_quote_router<'c: 'info, 'info>(
    ctx: Context<'info, QuoteRouter<'info>>,
    args: QuoteRouterArgs,
) -> Result<()> {
    let clock = Clock::get()?;
    let state = ctx.accounts.state.load()?;
    let market_index = args.market_index;

    let remaining_accounts_iter = &mut ctx.remaining_accounts.iter().peekable();
    let AccountMaps {
        perp_market_map,
        spot_market_map,
        mut oracle_map,
    } = load_maps(
        remaining_accounts_iter,
        &get_writable_perp_market_set(market_index),
        &MarketSet::new(),
        clock.slot,
        Some(state.oracle_guard_rails),
    )?;
    let (makers, _maker_stats) = load_user_maps(remaining_accounts_iter, false)?;

    // Quoter section: entries first, then the union of their CPI accounts.
    let leftover: Vec<&AccountInfo<'info>> = remaining_accounts_iter.collect();
    let quoter_count = args.quoter_count as usize;
    validate!(
        quoter_count <= leftover.len(),
        ErrorCode::DefaultError,
        "quoter_count {} exceeds the quoter section",
        quoter_count
    )?;
    let account_map: BTreeMap<Pubkey, AccountInfo<'info>> = leftover[quoter_count..]
        .iter()
        .map(|info| (*info.key, (*info).clone()))
        .collect();
    let quoters: Vec<AccountLoader<QuoterV0>> = leftover[..quoter_count]
        .iter()
        .map(|info| AccountLoader::try_from(info))
        .collect::<Result<_>>()?;

    let taker_direction = args.direction.to_position_direction();
    let mut buffer = ctx.accounts.quote_buffer.load_mut()?;
    validate!(
        buffer.market == market_index,
        ErrorCode::DefaultError,
        "quote buffer is for market {}, quote is for market {}",
        buffer.market,
        market_index
    )?;
    buffer.begin(args.direction as u8, args.size, clock.slot);

    // ---- Externals first: their books are the vAMM's last look. ----
    // Books are held as owned levels so they can be handed to the ladder as
    // rivals after the CPI borrow ends.
    let mut books: Vec<(u8, Vec<PriceLevel>)> = Vec::with_capacity(quoter_count);
    for loader in &quoters {
        let (priority, quoter_type, quoter_user, mut levels) = {
            let quoter = loader.load()?;
            validate!(
                quoter.market == market_index,
                ErrorCode::DefaultError,
                "quoter entry {} is for market {}",
                loader.key(),
                quoter.market
            )?;
            if !(quoter.is_active && quoter.is_approved) {
                continue;
            }
            let levels = quoter
                .quote(
                    market_index,
                    QuoteArgsV0 {
                        direction: args.direction,
                        size: args.size,
                        // A view has no settlement, so no loaded-user
                        // restriction: quote everything the book holds.
                        users: QuoterUserSetV0::EMPTY,
                        taker: None,
                    },
                    &state.signer,
                    state.signer_nonce,
                    &account_map,
                )
                .map_err(|e| {
                    msg!("quoter {} quote failed: {}", loader.key(), e);
                    ErrorCode::DefaultError
                })?;
            (quoter.priority, quoter.quoter_type, quoter.user, levels)
        };

        // Verification: a Custom quoter's depth is never margin-reserved, so
        // clamp it to what its user can actually support. CLOB depth was
        // gated at placement, so it stands as quoted.
        let mut clamped = false;
        if quoter_type == QuoterType::Custom {
            let cap = margin_cap(
                &makers,
                &quoter_user,
                market_index,
                taker_direction.opposite(),
                &perp_market_map,
                &spot_market_map,
                &mut oracle_map,
            )?;
            clamped = truncate_to(&mut levels, cap);
        }
        buffer.push(
            QuotedSourceKind::Quoter,
            loader.key(),
            priority,
            clamped,
            &levels,
        )?;
        books.push((priority, levels));
    }

    // ---- DLOB makers next: one level per crossing resting order. ----
    let clob_tier = QuoterType::Clob.default_priority();
    let order_tick_size = perp_market_map.get_ref(&market_index)?.order_tick_size;
    let (oracle_price, amm_snapshot) = {
        let market = perp_market_map.get_ref(&market_index)?;
        let oracle_pd = *oracle_map.get_price_data(&market.oracle_id())?;
        (oracle_pd, market.amm)
    };
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
            clock.slot,
            order_tick_size,
        )?;
        for (order_index, price) in found {
            let size =
                maker.orders[order_index].get_base_asset_amount_unfilled(Some(position_base))?;
            if size == 0 {
                continue;
            }
            let levels = [PriceLevel { price, size }];
            buffer.push(
                QuotedSourceKind::DlobOrder,
                *maker_key,
                clob_tier,
                false,
                &levels,
            )?;
            books.push((clob_tier, levels.to_vec()));
        }
    }

    // ---- vAMM last, with everything above as its rivals (last look). ----
    // Quoted off a copy: `refresh` projects the curve, and this instruction
    // must not move the market's AMM.
    let mut amm: AMM = amm_snapshot;
    let inputs = {
        let market = perp_market_map.get_ref(&market_index)?;
        MarketQuoteInputs::load(
            &market,
            oracle_price,
            clock.slot,
            &state.oracle_guard_rails.validity,
        )?
    };
    let rivals: Vec<QuoterBook> = books
        .iter()
        .map(|(priority, levels)| QuoterBook {
            priority: *priority,
            levels,
        })
        .collect();
    let ctx = inputs.ctx(clock.slot);
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
        false,
        &amm_levels,
    )?;

    msg!(
        "quoted {} sources for market {} at size {}",
        buffer.source_count,
        market_index,
        args.size
    );
    Ok(())
}

/// The base a maker can support on `direction` given its margin right now —
/// the same bound the fill's pre-execute clamp uses.
fn margin_cap(
    makers: &crate::state::user_map::UserMap,
    user: &Pubkey,
    market_index: u16,
    maker_direction: PositionDirection,
    perp_market_map: &crate::state::perp_market_map::PerpMarketMap,
    spot_market_map: &crate::state::spot_market_map::SpotMarketMap,
    oracle_map: &mut crate::state::oracle_map::OracleMap,
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
        perp_market_map,
        spot_market_map,
        oracle_map,
    )?)
}

/// Truncate a book to `cap` total base, best levels first. Returns whether it
/// bit — the caller records that on the source so a consumer can tell a thin
/// quoter from a clamped one.
fn truncate_to(levels: &mut Vec<PriceLevel>, cap: u64) -> bool {
    let total: u64 = levels.iter().map(|l| l.size).fold(0, u64::saturating_add);
    if total <= cap {
        return false;
    }
    let mut remaining = cap;
    levels.retain_mut(|level| {
        let take = level.size.min(remaining);
        remaining -= take;
        level.size = take;
        take > 0
    });
    true
}
