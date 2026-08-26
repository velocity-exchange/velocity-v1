//! The stack-buffer emit path must be byte-identical to anchor's
//! `Event::data()`.
//!
//! `emit!` allocates a `Vec` per event; [`crate::emit`] builds the same
//! `[discriminator][body]` field on the stack instead. That is only safe if
//! the bytes match exactly — decoders (velocity's `EventSubscriber`, the
//! TypeScript SDK) key on the discriminator and then decode the body as borsh.
//! Each test below encodes a record both ways and compares.

use {
    crate::{
        emit::{
            pod_log_bytes, write_execute_record, CancelAllRecord, LogBuf, DISCRIMINATOR_BYTES,
            EXECUTE_RECORD_LOG_BYTES,
        },
        events::{
            ExecuteRecordV0, FillSlimV0, OrderCancelRecordV0, OrderEvictRecordV0,
            OrderExpireRecordV0, OrderPlaceRecordV0, OrdersCancelRecordV0,
        },
        state::{CancelAllOutcome, CANCEL_ALL_ORDERS_CEILING, EXECUTE_FILLS_CEILING},
    },
    anchor_lang_v2::prelude::*,
};

fn authority() -> Address {
    Address::new_from_array([0xAB; 32])
}

/// `pod_log_bytes` with the width the `emit_pod!` macro computes.
macro_rules! assert_pod_matches_event {
    ($ty:ident { $($field:tt)* }) => {{
        const LOG_BYTES: usize = DISCRIMINATOR_BYTES + core::mem::size_of::<$ty>();
        let record = $ty { $($field)* };
        assert_eq!(
            pod_log_bytes::<$ty, LOG_BYTES>(&record).as_slice(),
            Event::data(&record).as_slice(),
            concat!(stringify!($ty), " emit path diverged from Event::data()"),
        );
    }};
}

#[test]
fn the_lifecycle_records_emit_the_bytes_the_event_impl_would() {
    assert_pod_matches_event!(OrderPlaceRecordV0 {
        authority: authority(),
        ts: -7,
        slot: 9,
        order_id: 11,
        activation_slot: 13,
        max_ts: 15,
        price: 17,
        base_asset_amount: 19,
        node_index: 21,
        client_order_id: 27,
        market_index: 23,
        sub_account_id: 25,
        side: 1,
        _pad: [0; 3],
    });
    assert_pod_matches_event!(OrderCancelRecordV0 {
        authority: authority(),
        ts: -7,
        order_id: 11,
        price: 17,
        base_asset_amount: 19,
        market_index: 23,
        sub_account_id: 25,
        client_order_id: 27,
    });
    assert_pod_matches_event!(OrderEvictRecordV0 {
        authority: authority(),
        ts: -7,
        order_id: 11,
        price: 17,
        base_asset_amount: 19,
        market_index: 23,
        sub_account_id: 25,
        client_order_id: 27,
    });
    assert_pod_matches_event!(OrderExpireRecordV0 {
        authority: authority(),
        ts: -7,
        order_id: 11,
        price: 17,
        base_asset_amount: 19,
        market_index: 23,
        sub_account_id: 25,
        client_order_id: 27,
    });
}

/// The cancel-all record is written prefix-first with its totals reserved and
/// patched after the sweep, so this pins the patched result against
/// `Event::data()` at the shapes that vary: an empty sweep, one side, both
/// sides, and the id list at the per-call ceiling.
#[test]
fn the_cancel_all_record_emits_the_bytes_the_event_impl_would() {
    let shapes: [(CancelAllOutcome, u8); 4] = [
        (
            CancelAllOutcome {
                exhaustive: true,
                ..Default::default()
            },
            2,
        ),
        (
            CancelAllOutcome {
                bid_base_asset_amount: 500,
                bid_orders: 2,
                exhaustive: true,
                ..Default::default()
            },
            0,
        ),
        (
            CancelAllOutcome {
                bid_base_asset_amount: 500,
                ask_base_asset_amount: 700,
                bid_orders: 2,
                ask_orders: 3,
                exhaustive: true,
            },
            2,
        ),
        (
            CancelAllOutcome {
                bid_base_asset_amount: u64::MAX,
                ask_base_asset_amount: 1,
                bid_orders: CANCEL_ALL_ORDERS_CEILING as u32 - 1,
                ask_orders: 1,
                exhaustive: false,
            },
            2,
        ),
    ];
    for (outcome, sides) in shapes {
        let client_order_ids: Vec<u32> = (0..outcome.orders()).map(|i| 1_000 + i).collect();
        // Boxed for the same reason as the execute record's buffer: the program
        // gives it its own frame, and the test needn't put ~1KB on the host
        // stack.
        let mut record = Box::new(CancelAllRecord::new(&authority(), -7, 23, 25, sides).unwrap());
        client_order_ids
            .iter()
            .try_for_each(|client_order_id| record.push_id(*client_order_id))
            .unwrap();
        let expected = Event::data(&OrdersCancelRecordV0 {
            authority: authority(),
            ts: -7,
            bid_base_asset_amount: outcome.bid_base_asset_amount,
            ask_base_asset_amount: outcome.ask_base_asset_amount,
            market_index: 23,
            sub_account_id: 25,
            sides,
            exhaustive: outcome.exhaustive,
            client_order_ids: client_order_ids.clone(),
        });
        assert_eq!(
            record.log_bytes(&outcome).unwrap(),
            expected.as_slice(),
            "cancel-all record with {} ids diverged from Event::data()",
            client_order_ids.len()
        );
    }
}

/// A sweep whose reported count disagrees with the ids pushed would log a
/// record an indexer can't reconcile, so the record refuses to be built.
#[test]
fn the_cancel_all_record_refuses_a_count_that_disagrees_with_its_ids() {
    let mut record = Box::new(CancelAllRecord::new(&authority(), 0, 0, 0, 2).unwrap());
    record.push_id(1).unwrap();
    let outcome = CancelAllOutcome {
        bid_orders: 2,
        exhaustive: true,
        ..Default::default()
    };
    assert!(record.log_bytes(&outcome).is_err());
}

/// The execute record is the variable-length one, so its hand-written encoder
/// is checked at the shapes that vary: no fills, a cull with no fills, and the
/// widest payload a market config can drive.
#[test]
fn the_execute_record_emits_the_bytes_the_event_impl_would() {
    let fill = |i: u64| FillSlimV0 {
        order_id: i,
        base_size: 100 + i,
        client_order_id: 1_000 + i as u32,
    };
    let shapes: [(Vec<FillSlimV0>, Option<u32>); 4] = [
        (vec![], None),
        (vec![], Some(77)),
        (vec![fill(1), fill(2), fill(3)], Some(77)),
        (
            (0..EXECUTE_FILLS_CEILING as u64).map(fill).collect(),
            Some(u32::MAX),
        ),
    ];
    for (fills, cancelled_client_order_id) in shapes {
        // Boxed: the program keeps this buffer in its own stack frame, and the
        // test has no reason to put ~2KB on the host stack either.
        let mut log = Box::new(LogBuf::<EXECUTE_RECORD_LOG_BYTES>::new());
        write_execute_record(&mut log, -7, 9, 23, 1, &fills, cancelled_client_order_id).unwrap();
        let record = ExecuteRecordV0 {
            ts: -7,
            slot: 9,
            market_index: 23,
            direction: 1,
            fills: fills.clone(),
            cancelled_client_order_ids: cancelled_client_order_id.into_iter().collect(),
        };
        assert_eq!(
            log.as_slice(),
            Event::data(&record).as_slice(),
            "execute record with {} fills diverged from Event::data()",
            fills.len()
        );
    }
}
