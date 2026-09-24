//! One router fill: quote the route, then fill the order against it.
//!
//! Every entrypoint that fills through the router runs [`RouteFill::run`].
//! Callers differ only in data: the order they fill, the route it signed, and
//! what the filler answers for. The quote, the books, the router leg and the
//! perp fill happen in one order everywhere.

use {
    super::quoted_route::{quote_route, FillerStanding, QuoteInputs, RouteClaim},
    crate::{
        controller::{
            self,
            orders::{FillAmounts, FillParties, FillRequest, PerpFillAccounts},
            position::PositionDirection,
        },
        error::ErrorCode,
        instructions::optional_accounts::{
            get_referrer_accelerated_status, get_revenue_share_escrow_account,
            tx_writable_lock_count, AccountMaps,
        },
        load,
        math::router::FillerObligation,
        state::{
            fill_mode::FillMode,
            prop_amm::{quoter_wire_users, DirectionV0, QuoterCpiScratch, UserRefV0},
            revenue_share::RevenueShareEscrowZeroCopyMut,
            state::State,
            user::{Order, User},
            user_map::{load_user_maps, UserMap, UserStatsMap},
        },
    },
    anchor_lang::prelude::*,
    std::{iter::Peekable, slice::Iter},
};

/// The accounts a router fill reads after the market maps, in the order every
/// fill lays them out: the maker and referrer set, the taker's revenue-share
/// escrow, the referrer's accelerated status, and then the quoter tail.
pub struct RouteFillAccounts<'info> {
    pub makers_and_referrer: UserMap<'info>,
    pub makers_and_referrer_stats: UserStatsMap<'info>,
    pub escrow: Option<RevenueShareEscrowZeroCopyMut<'info>>,
    pub referrer_is_accelerated: bool,
    /// The quoter section: everything the sections above left behind.
    pub quoters: &'info [AccountInfo<'info>],
}

impl<'info> RouteFillAccounts<'info> {
    /// Read the sections from `iter`, which has already consumed the maps.
    pub fn read(
        remaining_accounts: &'info [AccountInfo<'info>],
        iter: &mut Peekable<Iter<'info, AccountInfo<'info>>>,
        state: &State,
        taker: &AccountLoader<'info, User>,
    ) -> Result<Self> {
        let (makers_and_referrer, makers_and_referrer_stats) = load_user_maps(iter, true)?;
        let escrow = if state.builder_codes_enabled() {
            get_revenue_share_escrow_account(iter, &load!(taker)?.authority)?
        } else {
            None
        };
        let referrer_is_accelerated = get_referrer_accelerated_status(iter, escrow.as_ref())?;

        Ok(Self {
            makers_and_referrer,
            makers_and_referrer_stats,
            escrow,
            referrer_is_accelerated,
            quoters: &remaining_accounts[remaining_accounts.len() - iter.len()..],
        })
    }
}

/// The loaded users a quoter may fill against, in the form the wire carries.
pub fn loaded_wire_users(makers_and_referrer: &UserMap) -> Result<Vec<UserRefV0>> {
    Ok(quoter_wire_users(
        makers_and_referrer
            .user_ref_index()?
            .into_keys()
            .map(|(authority, sub_account_id)| UserRefV0 {
                authority,
                sub_account_id,
            }),
    )?)
}

/// The market facts a route is priced against.
pub struct RouteMark {
    /// The mark a quoter prices a capped maker's loss against.
    pub reference_price: i64,
    /// The market's initial margin ratio, which a quoter's oracle band
    /// defaults to.
    pub margin_ratio_initial: u32,
}

impl RouteMark {
    pub fn read(maps: &mut AccountMaps, market_index: u16) -> Result<Self> {
        let (oracle_id, margin_ratio_initial) = {
            let market = maps.perp_market_map.get_ref(&market_index)?;
            (market.oracle_id(), market.margin_ratio_initial)
        };

        Ok(Self {
            reference_price: maps.oracle_map.get_price_data(&oracle_id)?.price,
            margin_ratio_initial,
        })
    }
}

/// What the router needs to know about the taker order it fills.
pub struct RoutedOrder {
    pub direction: DirectionV0,
    /// Base the order still has to fill.
    pub unfilled: u64,
    pub taker: UserRefV0,
    /// The worst price this fill accepts, or zero for no bound.
    pub limit_price: u64,
    pub mark: RouteMark,
}

impl RoutedOrder {
    /// Read the route facts off one taker order of `user`.
    ///
    /// The order is passed in rather than looked up, because no order the
    /// router fills lives in `user.orders`.
    pub fn read(
        user: &User,
        order: &Order,
        maps: &mut AccountMaps,
        mode: FillMode,
    ) -> Result<Self> {
        let position_base = user
            .get_perp_position(order.market_index)
            .map(|position| position.base_asset_amount)
            .ok();
        let tick_size = maps
            .perp_market_map
            .get_ref(&order.market_index)?
            .order_tick_size;

        Ok(Self {
            direction: route_direction(order.direction),
            unfilled: order.get_base_asset_amount_unfilled(position_base)?,
            taker: user.clob_user_ref(),
            limit_price: mode.quote_limit_price(order, tick_size),
            mark: RouteMark::read(maps, order.market_index)?,
        })
    }
}

/// The router's name for a taker's side.
pub fn route_direction(direction: PositionDirection) -> DirectionV0 {
    match direction {
        PositionDirection::Long => DirectionV0::Long,
        PositionDirection::Short => DirectionV0::Short,
    }
}

/// What the party that built the transaction answers for.
pub struct FillerTerms {
    /// The taker's own authority or delegate signs this transaction.
    pub taker_signed: bool,
    /// Distinct accounts the transaction locks. `None` when the caller passed
    /// no instructions sysvar, so the fill cannot count them.
    pub tx_accounts: Option<usize>,
    /// Whether the caller opens and closes the taker's whole exposure inside
    /// one instruction and asserts the end state itself.
    pub taker_exposure_closed_by_caller: bool,
}

impl FillerTerms {
    /// The taker signed and chose the account list, so no obligation applies.
    pub const TAKER_SIGNED: Self = Self {
        taker_signed: true,
        tx_accounts: None,
        taker_exposure_closed_by_caller: false,
    };

    /// The taker did not sign, so the caller answers for what its account
    /// list left out.
    pub fn keeper(instructions_sysvar: Option<&AccountInfo>) -> Result<Self> {
        Ok(Self {
            taker_signed: false,
            tx_accounts: instructions_sysvar
                .map(tx_writable_lock_count)
                .transpose()?,
            taker_exposure_closed_by_caller: false,
        })
    }
}

/// How the order is routed: what the quoters are told, and what the filler
/// answers for.
pub struct RouteRequest<'a> {
    pub order: RoutedOrder,
    pub taker_served_window: bool,
    /// See [`QuoteInputs::include_taker_origin_reservations`].
    pub include_taker_origin_reservations: bool,
    pub claim: Option<RouteClaim<'a>>,
    pub filler: FillerTerms,
}

/// What a router fill moved.
pub struct RoutedFill {
    pub amounts: FillAmounts,
    /// The worst price any leg filled at, or `None` when nothing filled.
    pub worst_fill_price: Option<u64>,
}

/// The facts every router fill reads besides the order and its parties.
pub struct RouteFill<'a, 'info> {
    pub state: &'a State,
    pub clock: &'a Clock,
    /// The quoter section of the account list: the market's `QuoterSlabV0`
    /// and the consulted quoters' registered CPI accounts.
    pub tail: &'info [AccountInfo<'info>],
    /// One set of CPI buffers for the quote and the execute legs. Velocity's
    /// heap never gives a freed buffer back.
    pub scratch: &'a mut QuoterCpiScratch<'info>,
}

impl<'info> RouteFill<'_, 'info> {
    /// Size and quote the route, then fill the order against it.
    pub fn run(
        self,
        route: RouteRequest<'_>,
        fill: FillRequest<'_>,
        accounts: PerpFillAccounts<'_, '_, 'info>,
        parties: &mut FillParties<'_, 'info, 'info, 'info>,
    ) -> Result<RoutedFill> {
        let users = loaded_wire_users(parties.makers_and_referrer)?;
        let quoted = quote_route(
            self.tail,
            QuoteInputs {
                market_index: fill.order.market_index,
                direction: route.order.direction,
                size: route.order.unfilled,
                users: &users,
                reference_price: route.order.mark.reference_price,
                taker: route.order.taker,
                limit_price: route.order.limit_price,
                taker_served_window: route.taker_served_window,
                margin_ratio_initial: route.order.mark.margin_ratio_initial,
                include_taker_origin_reservations: route.include_taker_origin_reservations,
            },
            route.claim,
            &mut super::user_caps::CapInputs {
                taker_key: &accounts.user.key(),
                makers_and_referrer: parties.makers_and_referrer,
                makers_and_referrer_stats: parties.makers_and_referrer_stats,
                maps: parties.maps,
                slot: self.clock.slot,
                now: self.clock.unix_timestamp,
            },
            self.scratch,
        )?;

        let mut books = quoted.books(self.clock, self.scratch)?;
        let mut router = books.for_fill(FillerStanding {
            protocol_authority: self.state.signer,
            obligation: FillerObligation {
                taker_signed: route.filler.taker_signed,
                tx_accounts: route.filler.tx_accounts,
                unrouted_quoters: quoted.unrouted_quoters,
            },
            taker_exposure_closed_by_caller: route.filler.taker_exposure_closed_by_caller,
        });

        let amounts = controller::orders::fill_perp_order(
            fill,
            self.state,
            self.clock,
            accounts,
            parties,
            &mut router,
        )?;

        Ok(RoutedFill {
            amounts,
            worst_fill_price: router.worst_fill_price,
        })
    }
}
