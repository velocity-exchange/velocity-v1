use crate::error::{ErrorCode, VelocityResult};
use crate::msg;
use anchor_lang::prelude::*;
use std::panic::Location;

#[cfg(test)]
mod tests;

#[derive(Clone, Copy, AnchorSerialize, AnchorDeserialize, PartialEq, Debug, Eq)]
pub enum SettlePnlMode {
    MustSettle,
    TrySettle,
}

impl SettlePnlMode {
    /// Resolve a bail-out from `settle_pnl`. In `MustSettle` mode the error
    /// propagates; in `TrySettle` mode it is soft-skipped into `Ok(false)`.
    /// The `bool` is the "did settlement actually happen" signal — reaching
    /// this method always means it did NOT, so the `Ok` arm carries `false`
    /// (a truly-settled call returns `Ok(true)` from `settle_pnl` directly).
    /// Callers gate follow-on side effects (e.g. the revenue-share sweep) on
    /// this signal so a soft-skip does not move funds out of the pnl pool.
    #[track_caller]
    #[inline(always)]
    pub fn result(
        self,
        error_code: ErrorCode,
        market_index: u16,
        msg: &str,
    ) -> VelocityResult<bool> {
        let caller = Location::caller();
        msg!(msg);
        msg!(
            "Error {:?} for market {} at {}:{}",
            error_code,
            market_index,
            caller.file(),
            caller.line()
        );
        match self {
            SettlePnlMode::MustSettle => Err(error_code),
            SettlePnlMode::TrySettle => Ok(false),
        }
    }
}
