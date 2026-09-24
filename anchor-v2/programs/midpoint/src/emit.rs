//! Event emission without a heap allocation.
//!
//! The mechanism lives in `quoter-emit-v2`, which both anchor v2 quoters
//! share. This module re-exports it so call sites read `crate::emit`, and the
//! test below pins this program's own record against `Event::data()`.

pub use quoter_emit::{assert_pod_matches_event, emit_pod, pod_log_bytes, DISCRIMINATOR_BYTES};

#[cfg(test)]
mod tests {
    use {
        super::*,
        crate::events::{MidpointConfigRecordV0, MidpointExecuteRecordV0, MIDPOINT_EVENT_VERSION},
        anchor_lang::prelude::Address,
    };

    /// The stack-built bytes must equal anchor's. A decoder keys on the
    /// discriminator and then reads the body as the record's raw bytes.
    #[test]
    fn the_execute_record_emits_the_bytes_the_event_impl_would() {
        assert_pod_matches_event!(MidpointExecuteRecordV0 {
            user_authority: Address::new_from_array([0xAB; 32]),
            ts: -7,
            slot: 9,
            mid_price: 100_000_000,
            base_size: 11,
            quote_size: 13,
            configured_market_index: 17,
            sub_account_id: 19,
            direction: 1,
            version: MIDPOINT_EVENT_VERSION,
            _pad: [0; 2],
        });
    }

    #[test]
    fn the_config_record_emits_the_bytes_the_event_impl_would() {
        assert_pod_matches_event!(MidpointConfigRecordV0 {
            authority: Address::new_from_array([0x11; 32]),
            hot_authority: Address::new_from_array([0x22; 32]),
            pending_authority: Address::new_from_array([0x33; 32]),
            ts: -7,
            max_mid_staleness_slots: 25,
            price_tick_size: 100,
            size_step: 1_000,
            min_quote_size: 10_000,
            max_mid_deviation_ppm: 10_000,
            mid_sequence: 3,
            market_index: 1,
            sub_account_id: 2,
            is_paused: 0,
            require_attested_flow: 1,
            version: MIDPOINT_EVENT_VERSION,
            _pad: [0; 1],
        });
    }
}
