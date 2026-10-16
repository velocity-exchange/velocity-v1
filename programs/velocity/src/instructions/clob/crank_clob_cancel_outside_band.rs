//! `crank_clob_cancel_outside_band`. The crank cancels a book order whose price
//! breaches the maker oracle band.
//!
//! The router fills a book best price first and cannot skip a level, so it
//! drops the whole book from a fill when a quoted level breaches the band. One
//! such order takes its side of the book out of every router fill. The crank
//! judges the order by the router's own rule: the
//! `limit_price_breaches_maker_oracle_price_bands` predicate, at the MM oracle
//! price the fill uses and the band of the book's quoter entry. The oracle must pass
//! the gates a crossed-book crank passes.
//!
//! Maker placement and modify refuse prices outside the same band. A
//! taker-origin order, which is a taker remainder or a fired trigger-limit,
//! is clamped just inside the band before it rests. An order is therefore
//! outside the band only after the oracle moves away from it.
//!
//! The crank removes the order the way the expiry crank does. The maker pays
//! the removal fee, and a placed trigger's shadow is freed. Program-keeper mode
//! raises the fee to the value of the reservoir payment at the SOL TWAP, and
//! pays the lamports only when that TWAP priced them. The book refuses to cancel a taker remainder whose claim still
//! holds, so a crossing remainder keeps its window for the cross crank.
//!
//! The market's cross fallback poll wakes this crank. When the poll finds no
//! cross, [`stage_band_cancel`] reads the top rows of each side and stages the
//! cancel of the first order outside the band, in program-keeper mode. The
//! oracle is not in the resolver list, so the resolver asks relay for it.

use {
    super::helpers::crank_common::{
        book_l3_sides, crank_clob_removal, payment_sol_price, program_keeper_mode,
        sol_spot_market_ref, ClobRemoval, RemovalAccounts, ResolveClobCrank,
    },
    crate::{
        controller::{
            orders::{crank_oracle_preflight, maker_band_oracle_price},
            position::PositionDirection,
        },
        error::ErrorCode,
        instructions::{
            constraints::*,
            liq_relay::resolve_liquidate_perp_with_fill::heap_zeroed,
            optional_accounts::AccountMaps,
            relay_harness::{Resolution, StagedCall},
        },
        load_mut,
        math::orders::limit_price_breaches_maker_oracle_price_bands,
        state::{
            clob_crank::{ClobCrankConditionsV0, CLOB_CRANK_CONDITIONS_PDA_SEED},
            oracle_map::OracleMap,
            pdas,
            perp_market::PerpMarket,
            prop_amm::{
                find_account, CancelOrderArgsV0, ClobOrderRefV0, ClobReader, L3RowV0,
                QuoterCpiScratch, QuoterSlabExt, QuoterSlabV0, SideV0, L3_ROW_FLAG_TAKER_ORIGIN,
            },
            spot_market::SpotMarket,
            state::{ExchangeStatus, State},
            user::{User, UserStats},
        },
        validate,
    },
    anchor_lang::prelude::*,
};

#[derive(Clone, Copy, AnchorSerialize, AnchorDeserialize)]
pub struct CrankClobCancelOutsideBandArgs {
    pub market_index: u16,
    /// The order outside the band. The crank reads it back from the book.
    pub order_ref: ClobOrderRefV0,
}

#[derive(Accounts)]
#[instruction(args: CrankClobCancelOutsideBandArgs)]
pub struct CrankClobCancelOutsideBand<'info> {
    pub state: AccountLoader<'info, State>,
    /// CHECK: in signed-keeper mode this must sign for `filler`, which the
    /// constraint below enforces. In program-keeper mode it is only the lamport
    /// payout target, and no signature is required.
    #[account(mut)]
    pub authority: UncheckedAccount<'info>,
    #[account(
        mut,
        constraint = can_crank_for_filler(&filler, &authority, &state)?
    )]
    pub filler: AccountLoader<'info, User>,
    #[account(
        mut,
        constraint = is_stats_for_user(&filler, &filler_stats)?
    )]
    pub filler_stats: AccountLoader<'info, UserStats>,
    /// The owner of the order. The book's report of the cancel is checked
    /// against this account.
    #[account(mut)]
    pub user: AccountLoader<'info, User>,
    #[account(mut, has_one = quoter_slab, has_one = clob_market, has_one = oracle)]
    pub perp_market: AccountLoader<'info, PerpMarket>,
    /// CHECK: the perp market's `has_one` binds it to the market oracle.
    pub oracle: UncheckedAccount<'info>,
    pub quoter_slab: AccountLoader<'info, QuoterSlabV0>,
    /// CHECK: the perp market's `has_one` binds it to the book the market
    /// designated.
    #[account(mut)]
    pub clob_market: UncheckedAccount<'info>,
    /// CHECK: a Clob slot's program is pinned to velocity's CLOB at
    /// registration. The handler checks it again through the slot.
    #[account(address = crate::ids::clob_program::id())]
    pub clob_program: UncheckedAccount<'info>,
    /// The market's lamport reservoir. Program-keeper mode requires it.
    #[account(
        mut,
        seeds = [
            CLOB_CRANK_CONDITIONS_PDA_SEED,
            args.market_index.to_le_bytes().as_ref(),
        ],

        bump
    )]
    pub crank_conditions: Option<AccountLoader<'info, ClobCrankConditionsV0>>,
    /// The SOL spot market, whose TWAP values the reservoir payment in quote.
    /// Program-keeper mode requires it when `State` names a SOL market.
    #[account(
        seeds = [
            b"spot_market",
            state.load()?.sol_spot_market_index.to_le_bytes().as_ref(),
        ],

        bump
    )]
    pub sol_spot_market: Option<AccountLoader<'info, SpotMarket>>,
}

/// A taker-origin order passes the owner's signed-message record as the one
/// remaining account, so the cancel releases its entry.
#[access_control(
    fill_not_paused(&ctx.accounts.state)
)]
pub fn handle_crank_clob_cancel_outside_band<'info>(
    ctx: Context<'info, CrankClobCancelOutsideBand<'info>>,
    args: CrankClobCancelOutsideBandArgs,
) -> Result<()> {
    let CrankClobCancelOutsideBandArgs {
        market_index,
        order_ref,
    } = args;
    let clock = Clock::get()?;
    let band = MakerBand::read(ctx.accounts, market_index, &clock)?;

    let order = ClobReader {
        market: &ctx.accounts.clob_market,
        program: &ctx.accounts.clob_program,
    }
    .orders(vec![order_ref])?
    .first()
    .copied()
    .filter(|order| order.found())
    .ok_or(ErrorCode::OrderDoesNotExist)?;

    validate!(
        band.refuses(order.price, PositionDirection::from(order.side))?,
        ErrorCode::ClobOrderInsideOracleBand,
        "order {} at {} is inside the band around oracle {}",
        order_ref.order_id,
        order.price,
        band.oracle_price
    )?;

    let accounts = &*ctx.accounts;
    let sol_price = payment_sol_price(
        &*accounts.state.load()?,
        program_keeper_mode(
            &accounts.filler,
            &accounts.state,
            accounts.crank_conditions.is_some(),
        )?,
        accounts.sol_spot_market.as_ref(),
    )?;
    crank_clob_removal(
        &RemovalAccounts {
            state: &accounts.state,
            authority: &accounts.authority,
            filler: &accounts.filler,
            user: &accounts.user,
            perp_market: &accounts.perp_market,
            quoter_slab: &accounts.quoter_slab,
            clob_market: &accounts.clob_market,
            clob_program: &accounts.clob_program,
            crank_conditions: &accounts.crank_conditions,
            signed_msg_record: ctx.remaining_accounts.first(),
            trigger_conditions: None,
        },
        market_index,
        ClobRemoval::OutsideBand(CancelOrderArgsV0 {
            order_ref,
            user: order.user,
            force: false,
        }),
        sol_price,
    )
}

/// Rows read off each side to find an order outside the band. A bid outside
/// the band is above the oracle and an ask is below it, so such orders lead
/// their side.
const BAND_SCAN_ROWS: u16 = 32;

/// The cross fallback poll's band answer, after the poll found no cross: the
/// cancel of the first order on either side outside the band, in program-keeper
/// mode. The market oracle is not in the resolver list, so the first round asks
/// relay for it. An oracle that fails the crank's gates is no work.
pub(super) fn stage_band_cancel(ctx: &Context<ResolveClobCrank>) -> Result<Resolution> {
    let (market_index, oracle) = {
        let conditions = ctx.accounts.crank_conditions.load()?;
        (conditions.market_index, conditions.oracle)
    };
    let market_key = pdas::perp_market(market_index);
    let (Some(market_info), Some(oracle_info)) = (
        find_account(ctx.remaining_accounts, &market_key),
        find_account(ctx.remaining_accounts, &oracle),
    ) else {
        let missing = [market_key, oracle]
            .iter()
            .copied()
            .filter(|key| find_account(ctx.remaining_accounts, key).is_none())
            .collect();
        return Ok(Resolution::MoreAccounts(missing));
    };

    let state = ctx.accounts.state.load()?;
    if state
        .get_exchange_status()?
        .contains(ExchangeStatus::FillPaused)
    {
        return Ok(Resolution::NoWork);
    }

    let Some(band) = resolver_band(ctx, &state, market_info, oracle_info)? else {
        return Ok(Resolution::NoWork);
    };

    let book_slot = ctx.accounts.quoter_slab.clob_slot(market_index)?;
    if !book_slot.quotes() {
        return Ok(Resolution::NoWork);
    }

    let accounts = [
        ctx.accounts.clob_market.to_account_info(),
        ctx.accounts.clob_program.to_account_info(),
    ];
    let mut cpi_scratch = QuoterCpiScratch::new();
    let Some(sides) = book_l3_sides(
        &book_slot,
        &ctx.accounts.quoter_slab,
        market_index,
        BAND_SCAN_ROWS,
        &accounts,
        &mut cpi_scratch,
        false,
        |row| *row,
    )?
    else {
        return Ok(Resolution::NoWork);
    };

    let found = match first_outside_band(&band, SideV0::Bid, &sides.bids)? {
        Some(row) => Some(row),
        None => first_outside_band(&band, SideV0::Ask, &sides.asks)?,
    };
    let Some(row) = found else {
        return Ok(Resolution::NoWork);
    };

    stage_cancel(ctx, &state, market_index, oracle, &row).map(Resolution::Call)
}

/// The band at the resolver's view of the oracle, or `None` when the oracle
/// fails the gates the crank applies. The resolver holds the market read-only,
/// so the gates run on a copy and a prelaunch oracle is judged as it stands.
fn resolver_band(
    ctx: &Context<ResolveClobCrank>,
    state: &State,
    market_info: &AccountInfo,
    oracle_info: &AccountInfo,
) -> Result<Option<MakerBand>> {
    let clock = Clock::get()?;
    let mut market = heap_zeroed::<PerpMarket>();
    {
        let data = market_info.try_borrow_data()?;
        let body = data.get(8..8 + std::mem::size_of::<PerpMarket>());
        let Some(body) = body.filter(|_| {
            market_info.owner == &crate::ID && data.get(..8) == Some(PerpMarket::DISCRIMINATOR)
        }) else {
            msg!("{} is not a velocity perp market", market_info.key);
            return Err(ErrorCode::PerpMarketAccountMismatch.into());
        };

        bytemuck::bytes_of_mut(&mut *market).copy_from_slice(body);
    }

    let Ok(mut oracle_map) = OracleMap::load_one(
        oracle_info,
        clock.slot,
        state.slot_clock(),
        Some(state.oracle_guard_rails),
    ) else {
        return Ok(None);
    };

    match MakerBand::gated(
        &mut market,
        state,
        &mut oracle_map,
        &clock,
        &ctx.accounts.quoter_slab,
    ) {
        Ok(band) => Ok(Some(band)),
        Err(error) => {
            msg!("band cancel oracle gates refused: {:?}", error);
            Ok(None)
        }
    }
}

/// The first row on `side` that the band refuses and the book lets a keeper
/// cancel. The book refuses a plain cancel of a taker remainder whose claim
/// still holds, so such a row is skipped.
fn first_outside_band(band: &MakerBand, side: SideV0, rows: &[L3RowV0]) -> Result<Option<L3RowV0>> {
    for row in rows {
        let bound_remainder = row.flags & L3_ROW_FLAG_TAKER_ORIGIN != 0
            && row.flags & quoter_spec::L3_ROW_FLAG_CLAIM_LAPSED == 0;
        if !bound_remainder && band.refuses(row.price, PositionDirection::from(side))? {
            return Ok(Some(*row));
        }
    }

    Ok(None)
}

/// The crank in program-keeper mode. A taker-origin order adds its owner's
/// signed-message record, so the cancel releases the entry.
fn stage_cancel(
    ctx: &Context<ResolveClobCrank>,
    state: &State,
    market_index: u16,
    oracle: Pubkey,
    row: &L3RowV0,
) -> Result<StagedCall> {
    let (protocol_user, protocol_user_stats) = pdas::protocol_user_pair();
    let mut call = StagedCall::new::<crate::instruction::CrankClobCancelOutsideBand>(
        crate::accounts::CrankClobCancelOutsideBand {
            state: ctx.accounts.state.key(),
            authority: pdas::keeper_placeholder(),
            filler: protocol_user,
            filler_stats: protocol_user_stats,
            user: pdas::user(&row.user.authority, row.user.sub_account_id),
            perp_market: pdas::perp_market(market_index),
            oracle,
            quoter_slab: ctx.accounts.quoter_slab.key(),
            clob_market: ctx.accounts.clob_market.key(),
            clob_program: crate::ids::clob_program::id(),
            crank_conditions: Some(ctx.accounts.crank_conditions.key()),
            sol_spot_market: sol_spot_market_ref(state),
        },
    );
    if row.flags & L3_ROW_FLAG_TAKER_ORIGIN != 0 {
        call = call.account(pdas::signed_msg_user_orders(&row.user.authority), true);
    }

    call.arg(CrankClobCancelOutsideBandArgs {
        market_index,
        order_ref: ClobOrderRefV0 {
            node_index: row.node_index,
            order_id: row.order_id,
        },
    })
}

/// The band the router holds a book's makers to, as it stands this slot.
pub(super) struct MakerBand {
    /// The MM oracle price the router measures the band from.
    oracle_price: i64,
    /// The book entry's band, in MARGIN_PRECISION units.
    oracle_band: u32,
}

impl MakerBand {
    /// A band no price breaches, for tests that exercise other gates.
    #[cfg(test)]
    pub(super) const UNBOUNDED: Self = Self {
        oracle_price: i64::MAX,
        oracle_band: u32::MAX,
    };

    /// Read the band behind the oracle gates a crossed-book crank passes. A
    /// cancel is irreversible, so a stale or divergent oracle must not decide
    /// that an order is out of band.
    fn read(
        accounts: &CrankClobCancelOutsideBand,
        market_index: u16,
        clock: &Clock,
    ) -> Result<Self> {
        let state = accounts.state.load()?;
        let mut oracle_map = OracleMap::load_one(
            &accounts.oracle,
            clock.slot,
            state.slot_clock(),
            Some(state.oracle_guard_rails),
        )?;
        let market = &mut load_mut!(accounts.perp_market)?;
        validate!(
            market.market_index == market_index,
            ErrorCode::PerpMarketAccountMismatch,
            "perp market {} passed for market {}",
            market.market_index,
            market_index
        )?;

        crate::instructions::optional_accounts::update_prelaunch_oracle(
            market,
            &oracle_map,
            clock.slot,
        )?;

        Self::gated(
            market,
            &state,
            &mut oracle_map,
            clock,
            &accounts.quoter_slab,
        )
    }

    /// The band behind the oracle gates, for a market whose prelaunch oracle
    /// the caller already updated or chose not to update.
    pub(super) fn gated(
        market: &mut PerpMarket,
        state: &State,
        oracle_map: &mut OracleMap,
        clock: &Clock,
        quoter_slab: &AccountLoader<QuoterSlabV0>,
    ) -> Result<Self> {
        crank_oracle_preflight(market, state, oracle_map, clock, "band cancel")?;

        let oracle_price_data = *oracle_map.get_price_data(&market.oracle_id())?;
        Ok(Self {
            oracle_price: maker_band_oracle_price(market, state, &oracle_price_data, clock.slot)?,
            oracle_band: quoter_slab
                .clob_slot(market.market_index)?
                .config
                .oracle_band(market.margin_ratio_initial),
        })
    }

    /// Read the band a new maker order must rest inside.
    ///
    /// A refused placement costs the maker nothing, so this reads the oracle
    /// that the placement already loaded and skips the crank's oracle gates.
    pub(super) fn at_placement(
        state: &State,
        maps: &mut AccountMaps,
        quoter_slab: &AccountLoader<QuoterSlabV0>,
        market_index: u16,
        slot: u64,
    ) -> Result<Self> {
        let market = maps.perp_market_map.get_ref(&market_index)?;
        let oracle_price_data = *maps.oracle_map.get_price_data(&market.oracle_id())?;
        Ok(Self {
            oracle_price: maker_band_oracle_price(&market, state, &oracle_price_data, slot)?,
            oracle_band: quoter_slab
                .clob_slot(market_index)?
                .config
                .oracle_band(market.margin_ratio_initial),
        })
    }

    /// Refuse a maker order that would rest outside the band.
    ///
    /// The router fills a book best price first and cannot skip a level. One
    /// order outside the band therefore takes its side of the book out of
    /// every router fill until this crank cancels it.
    pub(super) fn validate_rest(
        &self,
        price: u64,
        maker_direction: PositionDirection,
    ) -> Result<()> {
        validate!(
            !self.refuses(price, maker_direction)?,
            ErrorCode::PriceBandsBreached,
            "a maker order at {} rests outside the band around oracle {}",
            price,
            self.oracle_price
        )?;

        Ok(())
    }

    /// Clamp a taker-origin order to the nearest price strictly inside the
    /// maker band. The later book-grid alignment moves bids down and asks
    /// up, so it can only move the result farther inside the band.
    pub(super) fn clamp_rest(&self, price: u64, direction: PositionDirection) -> Result<u64> {
        use crate::math::constants::MARGIN_PRECISION_U128;

        let oracle = u128::from(self.oracle_price.unsigned_abs());
        let distance = oracle
            .checked_mul(u128::from(self.oracle_band))
            .and_then(|value| value.checked_div(MARGIN_PRECISION_U128))
            .ok_or(ErrorCode::MathError)?;
        let edge = match direction {
            PositionDirection::Long => oracle.saturating_add(distance).saturating_sub(1),
            PositionDirection::Short => oracle.saturating_sub(distance).saturating_add(1),
        }
        .min(u128::from(u64::MAX)) as u64;

        Ok(match direction {
            PositionDirection::Long => price.min(edge),
            PositionDirection::Short => price.max(edge),
        })
    }

    /// Whether a maker order at `price` on the `maker_direction` side is one
    /// the router refuses.
    pub(super) fn refuses(&self, price: u64, maker_direction: PositionDirection) -> Result<bool> {
        Ok(limit_price_breaches_maker_oracle_price_bands(
            price,
            maker_direction,
            self.oracle_price,
            self.oracle_band,
        )?)
    }
}

#[cfg(test)]
mod tests {
    use {
        super::{first_outside_band, MakerBand},
        crate::{
            controller::position::PositionDirection,
            state::prop_amm::{L3RowV0, SideV0, L3_ROW_FLAG_TAKER_ORIGIN},
        },
    };

    /// Ten percent around an oracle of 100.
    const BAND: MakerBand = MakerBand {
        oracle_price: 100_000_000,
        oracle_band: 1_000,
    };

    #[test]
    fn a_bid_at_the_band_edge_is_refused() {
        assert!(BAND.refuses(110_000_000, PositionDirection::Long).unwrap());
        assert!(!BAND.refuses(109_999_999, PositionDirection::Long).unwrap());
    }

    #[test]
    fn an_ask_at_the_band_edge_is_refused() {
        assert!(BAND.refuses(90_000_000, PositionDirection::Short).unwrap());
        assert!(!BAND.refuses(90_000_001, PositionDirection::Short).unwrap());
    }

    /// A bid under the oracle gives the maker a better price than the oracle,
    /// which the band never refuses.
    #[test]
    fn a_bid_under_the_oracle_is_inside_the_band() {
        assert!(!BAND.refuses(50_000_000, PositionDirection::Long).unwrap());
    }

    /// A maker cannot rest an ask at half the oracle, so one small order
    /// cannot take the ask side out of every router fill.
    #[test]
    fn a_maker_placement_outside_the_band_is_refused() {
        assert_eq!(
            BAND.validate_rest(50_000_000, PositionDirection::Short),
            Err(crate::error::ErrorCode::PriceBandsBreached.into())
        );
        assert!(BAND
            .validate_rest(90_000_001, PositionDirection::Short)
            .is_ok());
    }

    fn row(price: u64, order_id: u64, flags: u8) -> L3RowV0 {
        L3RowV0 {
            price,
            order_id,
            flags,
            ..L3RowV0::default()
        }
    }

    #[test]
    fn the_first_bid_above_the_band_is_selected() {
        let bids = [
            row(115_000_000, 1, 0),
            row(110_000_000, 2, 0),
            row(100_000_000, 3, 0),
        ];
        let found = first_outside_band(&BAND, SideV0::Bid, &bids).unwrap();
        assert_eq!(found.map(|row| row.order_id), Some(1));

        let inside = [row(109_999_999, 4, 0)];
        assert_eq!(
            first_outside_band(&BAND, SideV0::Bid, &inside).unwrap(),
            None
        );
    }

    #[test]
    fn an_ask_below_the_band_is_selected() {
        let asks = [row(90_000_000, 5, 0)];
        let found = first_outside_band(&BAND, SideV0::Ask, &asks).unwrap();
        assert_eq!(found.map(|row| row.order_id), Some(5));
    }

    /// The book refuses a plain cancel of a remainder whose claim holds, so
    /// the resolver stages the next row instead.
    #[test]
    fn a_bound_taker_remainder_is_skipped() {
        let bids = [
            row(120_000_000, 6, L3_ROW_FLAG_TAKER_ORIGIN),
            row(
                115_000_000,
                7,
                L3_ROW_FLAG_TAKER_ORIGIN | quoter_spec::L3_ROW_FLAG_CLAIM_LAPSED,
            ),
        ];
        let found = first_outside_band(&BAND, SideV0::Bid, &bids).unwrap();
        assert_eq!(found.map(|row| row.order_id), Some(7));
    }

    #[test]
    fn a_remainder_is_clamped_strictly_inside_the_band() {
        assert_eq!(
            BAND.clamp_rest(120_000_000, PositionDirection::Long)
                .unwrap(),
            109_999_999
        );
        assert_eq!(
            BAND.clamp_rest(80_000_000, PositionDirection::Short)
                .unwrap(),
            90_000_001
        );
    }
}
