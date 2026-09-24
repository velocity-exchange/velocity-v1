//! Tests for the trigger watch threshold.

use {super::earliest_oracle_trigger_price, crate::state::oracle_watch::WatchDirection};

const TRIGGER_PRICE: u64 = 100_000;

/// Every clamp divisor `PerpMarket::trigger_price_clamp_divisor` returns.
const CLAMP_DIVISORS: [u64; 3] = [500, 100, 40];

/// The median trigger price can sit anywhere in `oracle ± oracle / divisor`,
/// so the watch must fire at every oracle price where the band reaches the
/// trigger.
#[test]
fn the_watch_fires_wherever_the_median_can_reach_the_trigger() {
    for divisor in CLAMP_DIVISORS {
        let above =
            earliest_oracle_trigger_price(TRIGGER_PRICE, divisor, WatchDirection::AtOrAbove)
                .unwrap();
        let below =
            earliest_oracle_trigger_price(TRIGGER_PRICE, divisor, WatchDirection::AtOrBelow)
                .unwrap();

        for oracle in 90_000..110_000u64 {
            let band = oracle / divisor;
            if oracle + band >= TRIGGER_PRICE {
                assert!(oracle >= above, "divisor {} oracle {}", divisor, oracle);
            }

            if oracle - band <= TRIGGER_PRICE {
                assert!(oracle <= below, "divisor {} oracle {}", divisor, oracle);
            }
        }
    }
}

/// The widening is at most the clamp band, so the watch stays near the
/// trigger.
#[test]
fn the_watch_moves_the_threshold_by_the_band_at_most() {
    let above = earliest_oracle_trigger_price(TRIGGER_PRICE, 40, WatchDirection::AtOrAbove);
    let below = earliest_oracle_trigger_price(TRIGGER_PRICE, 40, WatchDirection::AtOrBelow);

    assert_eq!(above, Some(97_560));
    assert_eq!(below, Some(102_564));
}
