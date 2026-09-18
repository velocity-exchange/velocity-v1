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
            FillMode::Fill | FillMode::Liquidation => {
                order.get_limit_price(valid_oracle_price, None, slot, tick_size, slot_clock)
            }
            FillMode::PlaceAndTake(_, auction_duration_percentage) => {
                if order.has_auction() {
                    // The auction is priced at the requested fraction of its
                    // wall-clock length rather than at a synthetic chain slot.
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

    /// The worst price a quoter may stop its ladder at, or zero for no bound.
    /// This is [`Self::get_limit_price`] resolved without the oracle. The
    /// oracle-relative branches need a price not held this early, so they
    /// return no bound rather than guess. Every other branch gives the real
    /// bound the fill applies later. Nothing is estimated, since a bound
    /// tighter than the fill's would hide depth the fill would have taken.
    pub fn quote_limit_price(
        &self,
        order: &Order,
        slot: u64,
        tick_size: u64,
        slot_clock: SlotClock,
    ) -> u64 {
        self.get_limit_price(order, None, slot, tick_size, slot_clock)
            .ok()
            .flatten()
            .unwrap_or(0)
    }

    pub fn is_liquidation(&self) -> bool {
        self == &FillMode::Liquidation
    }

    pub fn is_ioc(&self) -> bool {
        matches!(self, FillMode::PlaceAndTake(true, _))
    }
}
