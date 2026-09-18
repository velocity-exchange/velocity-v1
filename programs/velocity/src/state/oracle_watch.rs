//! Where a relay watch reads an oracle's price, and how to state a
//! protocol-precision threshold in that oracle's own raw units.
//!
//! A [`WakeKind::OnValueCross`] condition carries no oracle knowledge. It
//! names an account, a byte offset, and a width. The turner compares the
//! signed little-endian integer it finds there against a threshold. That keeps
//! the watch cheap, with no CPI, no deserialization, and no oracle-specific
//! code in the turner. It moves the work here instead. The threshold must be
//! written in whatever units that oracle account stores, so the byte layout
//! and the precision conversion must agree for each source.
//!
//! The trigger-order sync and the liquidation sync both need this, so it lives
//! here rather than in either of them. A mistake here is quiet. The watch
//! fires at the wrong price, or never fires.
//!
//! A source with no entry here returns `None`. Every caller supports that
//! outcome and falls back to its periodic sync condition and the keeper path,
//! which is slower but not wrong. `None` is the right answer for any price
//! that is not an affine function of a fixed-width field:
//!
//! - A stablecoin source snaps the price to exactly `PRICE_PRECISION` when it
//!   lands within 5bps of parity. See `get_pyth_stable_coin_price`. That step
//!   is not monotonic in the raw field, so no single raw threshold expresses a
//!   protocol-price crossing.
//! - Prelaunch and the deprecated switchboard variants store prices already in
//!   protocol precision, or in layouts this module does not track.

use {
    crate::{
        math::constants::PRICE_PRECISION_I128,
        state::{oracle::OracleSource, pyth_lazer_oracle::PythLazerOracle},
    },
    anchor_lang::{prelude::*, Discriminator},
    std::convert::TryInto,
};

/// Pyth push (`pyth_client::Price`) holds a 112-byte header, then `prod`,
/// `next`, and `agg_pub` at 32 bytes each, then `agg: PriceInfo` whose first
/// field is `price: i64`. `expo: i32` is the sixth word of the header.
const PYTH_PUSH_PRICE_OFFSET: u32 = 208;
const PYTH_PUSH_EXPONENT_OFFSET: usize = 20;

/// PythLazer holds `price: i64` immediately past the anchor discriminator and
/// `exponent: i32` at offset 32.
const PYTH_LAZER_PRICE_OFFSET: u32 = 8;
const PYTH_LAZER_EXPONENT_OFFSET: usize = 32;

/// Every source below stores its price as an `i64`.
const PRICE_LEN: u32 = 8;

/// The largest decimal exponent the raw conversion attempts. It sits well
/// past any real oracle, because pyth publishes about 8. The bound exists so
/// that the `pow` cannot overflow. It states no policy.
const MAX_DECIMALS: u32 = 12;

/// A registered raw-price watch: where the value lives, and what it means.
#[derive(Clone, Copy, Debug)]
pub struct OracleWatchV0 {
    /// Byte offset of the price within the oracle account.
    pub price_offset: u32,
    /// Width of the price field, in bytes.
    pub price_len: u32,
    /// The price currently at that offset, in the oracle's raw units.
    pub raw_price: i64,
    /// Raw units per whole unit, as a power of ten.
    decimals: u32,
    /// The source's price multiplier (1, 1e3, or 1e6 for the 1K/1M feeds).
    multiple: u128,
}

impl OracleWatchV0 {
    /// The oracle's current price in `PRICE_PRECISION`, matching what `get_pyth_price` would report.
    /// A caller doing margin arithmetic against a watched market needs this rather than
    /// [`Self::raw_price`], whose units are the oracle's own and match the protocol's only when the
    /// feed publishes six decimals.
    pub fn protocol_price(&self) -> Option<i128> {
        if self.decimals > MAX_DECIMALS {
            return None;
        }

        i128::from(self.raw_price)
            .checked_mul((self.multiple as i128).checked_mul(PRICE_PRECISION_I128)?)?
            .checked_div(10i128.checked_pow(self.decimals)?)
    }

    /// A `PRICE_PRECISION` price in this oracle's raw units, or `None` when it does not survive the
    /// conversion. The conversion inverts `get_pyth_price`'s scaling, so a raw crossing and a
    /// protocol crossing are the same event.
    ///
    /// Rounding follows `direction` and always moves toward firing early. The resolver re-derives
    /// everything from the real oracle code, so an early wake costs one simulation. A late wake is a
    /// missed trigger or a missed liquidation.
    ///
    /// `None` means the caller must not arm this watch. A threshold that overflows, or that lands at
    /// or below zero, is not a price the oracle can report.
    pub fn raw_threshold(&self, price: i128, direction: WatchDirection) -> Option<i64> {
        if self.decimals > MAX_DECIMALS {
            return None;
        }

        let numerator = price.checked_mul(10i128.checked_pow(self.decimals)?)?;
        let denominator = (self.multiple as i128).checked_mul(PRICE_PRECISION_I128)?;
        if numerator <= 0 || denominator <= 0 {
            return None;
        }

        let raw = match direction {
            WatchDirection::AtOrAbove => numerator.checked_div(denominator)?,
            // Ceiling division. `int_roundings` is unstable on this
            // toolchain. Both operands are positive by the check above.
            WatchDirection::AtOrBelow => numerator
                .checked_add(denominator.checked_sub(1)?)?
                .checked_div(denominator)?,
        };

        if raw <= 0 || raw > i64::MAX as i128 {
            return None;
        }

        Some(raw as i64)
    }
}

/// Which way a watch fires. It is also the rounding direction for the
/// threshold. One type carries both, so the two can never disagree.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WatchDirection {
    /// Fires once the watched value is at or above the threshold.
    AtOrAbove,
    /// Fires once the watched value is at or below the threshold.
    AtOrBelow,
}

impl WatchDirection {
    /// The comparison byte `ConditionV0::on_value_cross` expects.
    pub fn cmp(self) -> u8 {
        match self {
            WatchDirection::AtOrAbove => 0,
            WatchDirection::AtOrBelow => 1,
        }
    }
}

/// The watch layout for an oracle account, given the source its market is
/// configured with, or `None` when the source has no registered layout or
/// the account does not look like one.
///
/// The source comes from market config rather than from the account bytes,
/// which is how every other read of these accounts works. An oracle whose
/// bytes disagree with its configured source is an admin error.
pub fn oracle_watch(oracle: &AccountInfo, source: OracleSource) -> Option<OracleWatchV0> {
    let (price_offset, exponent_offset) = match source {
        OracleSource::Pyth | OracleSource::Pyth1K | OracleSource::Pyth1M => {
            (PYTH_PUSH_PRICE_OFFSET, PYTH_PUSH_EXPONENT_OFFSET)
        }
        OracleSource::PythLazer | OracleSource::PythLazer1K | OracleSource::PythLazer1M => {
            (PYTH_LAZER_PRICE_OFFSET, PYTH_LAZER_EXPONENT_OFFSET)
        }

        // See the module docs: no affine raw threshold exists for these.
        _ => return None,
    };

    let data = oracle.try_borrow_data().ok()?;
    if price_offset == PYTH_LAZER_PRICE_OFFSET {
        // Lazer accounts are anchor-owned, so the discriminator proves the
        // layout.
        if data.len() < 8 + core::mem::size_of::<PythLazerOracle>()
            || data.get(..8)? != PythLazerOracle::DISCRIMINATOR
        {
            return None;
        }
    }

    let price_at = price_offset as usize;
    let raw_price = i64::from_le_bytes(data.get(price_at..price_at + 8)?.try_into().ok()?);
    let exponent = i32::from_le_bytes(
        data.get(exponent_offset..exponent_offset + 4)?
            .try_into()
            .ok()?,
    );

    Some(OracleWatchV0 {
        price_offset,
        price_len: PRICE_LEN,
        raw_price,
        decimals: exponent.unsigned_abs(),
        multiple: source.get_pyth_multiple(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn watch(decimals: u32, multiple: u128) -> OracleWatchV0 {
        OracleWatchV0 {
            price_offset: 0,
            price_len: PRICE_LEN,
            raw_price: 0,
            decimals,
            multiple,
        }
    }

    /// The conversion must round-trip against `get_pyth_price`'s scaling,
    /// which is raw * 10^-decimals * multiple, in `PRICE_PRECISION`.
    #[test]
    fn raw_threshold_inverts_the_price_scaling() {
        let up = WatchDirection::AtOrAbove;
        // $150.00 on an 8-decimal feed.
        assert_eq!(
            watch(8, 1).raw_threshold(150_000_000, up),
            Some(15_000_000_000)
        );

        // The same feed as a 1K source. The raw field carries a thousandth of
        // the protocol price, so the threshold is 1000 times smaller.
        assert_eq!(
            watch(8, 1000).raw_threshold(150_000_000, up),
            Some(15_000_000)
        );

        // Fewer decimals than PRICE_PRECISION divides down.
        assert_eq!(watch(4, 1).raw_threshold(150_000_000, up), Some(1_500_000));
    }

    /// Rounding always moves the threshold toward firing early.
    #[test]
    fn raw_threshold_rounds_toward_firing_early() {
        // $1.500005 on a 5-decimal feed is exactly half a raw unit, so the
        // two directions must land on different integers.
        let price = 1_500_005i128;
        assert_eq!(
            watch(5, 1).raw_threshold(price, WatchDirection::AtOrAbove),
            Some(150_000)
        );
        assert_eq!(
            watch(5, 1).raw_threshold(price, WatchDirection::AtOrBelow),
            Some(150_001)
        );

        // An exact conversion rounds nowhere.
        for direction in [WatchDirection::AtOrAbove, WatchDirection::AtOrBelow] {
            assert_eq!(
                watch(6, 1).raw_threshold(150_000_000, direction),
                Some(150_000_000)
            );
            assert_eq!(
                watch(5, 1).raw_threshold(1_500_000, direction),
                Some(150_000)
            );
        }
    }

    #[test]
    fn raw_threshold_declines_what_it_cannot_represent() {
        let up = WatchDirection::AtOrAbove;
        assert_eq!(watch(8, 1).raw_threshold(0, up), None);
        assert_eq!(watch(8, 1).raw_threshold(-1, up), None);
        // Rounds to zero rather than arming a watch at price 0.
        assert_eq!(watch(0, 1).raw_threshold(1, up), None);
        assert_eq!(
            watch(MAX_DECIMALS + 1, 1).raw_threshold(150_000_000, up),
            None
        );
        assert_eq!(watch(12, 1).raw_threshold(i128::MAX / 2, up), None);
    }

    /// `protocol_price` is the forward direction of `raw_threshold`, so a
    /// price converted out and back has to land where it started.
    #[test]
    fn protocol_price_round_trips_through_raw_threshold() {
        for (decimals, multiple) in [(8u32, 1u128), (8, 1000), (6, 1), (4, 1), (9, 1_000_000)] {
            let mut w = watch(decimals, multiple);
            w.raw_price = 15_000_000_000;
            let protocol = w.protocol_price().unwrap();
            assert_eq!(
                w.raw_threshold(protocol, WatchDirection::AtOrAbove),
                Some(w.raw_price),
                "decimals={decimals} multiple={multiple}"
            );
        }
    }

    /// An 8-decimal feed's raw field is 100 times the protocol price. This
    /// module exists so that no caller reads the raw field as a price.
    #[test]
    fn protocol_price_is_not_the_raw_field() {
        let mut w = watch(8, 1);
        w.raw_price = 15_000_000_000; // $150.00
        assert_eq!(w.protocol_price(), Some(150_000_000));
    }

    #[test]
    fn direction_matches_the_condition_encoding() {
        assert_eq!(WatchDirection::AtOrAbove.cmp(), 0);
        assert_eq!(WatchDirection::AtOrBelow.cmp(), 1);
    }
}
