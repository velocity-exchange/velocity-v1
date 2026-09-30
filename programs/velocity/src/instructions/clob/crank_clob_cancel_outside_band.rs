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
//! the flat removal reward, a placed trigger's shadow is freed, and
//! program-keeper mode pays reservoir lamports for a crank that collected the
//! reward. The book refuses to cancel a taker remainder whose claim still
//! holds, so a crossing remainder keeps its window for the cross crank.
//!
//! No relay condition wakes this crank. A resolver needs the market oracle,
//! and the resolver account list does not carry it, so signed keepers run it.

use {
    super::helpers::crank_common::{crank_clob_removal, ClobRemoval, RemovalAccounts},
    crate::{
        controller::{
            orders::{crank_oracle_preflight, maker_band_oracle_price},
            position::PositionDirection,
        },
        error::ErrorCode,
        instructions::{constraints::*, optional_accounts::AccountMaps},
        load_mut,
        math::orders::limit_price_breaches_maker_oracle_price_bands,
        state::{
            clob_crank::{ClobCrankConditionsV0, CLOB_CRANK_CONDITIONS_PDA_SEED},
            oracle_map::OracleMap,
            perp_market::PerpMarket,
            prop_amm::{
                CancelOrderArgsV0, ClobOrderRefV0, ClobReader, QuoterSlabExt, QuoterSlabV0,
            },
            state::State,
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
    )
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

        crank_oracle_preflight(market, &state, &mut oracle_map, clock, "band cancel")?;

        let oracle_price_data = *oracle_map.get_price_data(&market.oracle_id())?;
        Ok(Self {
            oracle_price: maker_band_oracle_price(market, &state, &oracle_price_data, clock.slot)?,
            oracle_band: accounts
                .quoter_slab
                .clob_slot(market_index)?
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
    use {super::MakerBand, crate::controller::position::PositionDirection};

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
