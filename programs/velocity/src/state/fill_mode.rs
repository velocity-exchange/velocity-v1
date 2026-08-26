use crate::{
    error::VelocityResult,
    math::{
        auction::{auction_progress_at_fraction, calculate_auction_price_with_progress},
        casting::Cast,
        time::SlotClock,
    },
    state::user::Order,
};

#[cfg(test)]
mod tests;

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum FillMode {
    Fill,
    PlaceAndMake,
    PlaceAndTake(bool, u8),
    Liquidation,
}

impl FillMode {
    pub fn get_limit_price(
        &self,
        order: &Order,
        valid_oracle_price: Option<i64>,
        slot: u64,
        tick_size: u64,
        slot_clock: SlotClock,
    ) -> VelocityResult<Option<u64>> {
        match self {
            FillMode::Fill | FillMode::PlaceAndMake | FillMode::Liquidation => {
                order.get_limit_price(valid_oracle_price, None, slot, tick_size, slot_clock)
            }
            FillMode::PlaceAndTake(_, auction_duration_percentage) => {
                if order.has_auction() {
                    // price the auction at the requested fraction of its
                    // wall clock length, not at a synthetic chain slot
                    let progress = auction_progress_at_fraction(
                        order,
                        auction_duration_percentage.min(&100).cast()?,
                    )?;
                    calculate_auction_price_with_progress(
                        order,
                        progress,
                        tick_size,
                        valid_oracle_price,
                    )
                    .map(Some)
                } else {
                    order.get_limit_price(valid_oracle_price, None, slot, tick_size, slot_clock)
                }
            }
        }
    }

    pub fn is_liquidation(&self) -> bool {
        self == &FillMode::Liquidation
    }

    pub fn is_ioc(&self) -> bool {
        matches!(self, FillMode::PlaceAndTake(true, _))
    }
}
