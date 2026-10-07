use crate::{error::VelocityResult, state::user::Order};

#[cfg(test)]
mod tests;

/// How a fill was asked to fill. A liquidation answers differently in the fee
/// record, the margin gate and the AMM gate. Every other fill is `Fill`.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum FillMode {
    Fill,
    Liquidation,
}

impl FillMode {
    /// The worst price the fill is held to. Every mode reads the order's own
    /// bound.
    pub fn get_limit_price(
        &self,
        order: &Order,
        valid_oracle_price: Option<i64>,
        tick_size: u64,
    ) -> VelocityResult<Option<u64>> {
        order.get_limit_price(valid_oracle_price, None, tick_size)
    }

    /// The worst price a quoter may stop its ladder at, or zero for no bound.
    /// This is [`Self::get_limit_price`] resolved without the oracle. The
    /// oracle-relative branch needs a price not held this early, so it
    /// returns no bound rather than guess. Nothing is estimated, since a
    /// bound tighter than the fill's would hide depth the fill would take.
    pub fn quote_limit_price(&self, order: &Order, tick_size: u64) -> u64 {
        self.get_limit_price(order, None, tick_size)
            .ok()
            .flatten()
            .unwrap_or(0)
    }

    pub fn is_liquidation(&self) -> bool {
        self == &FillMode::Liquidation
    }
}
