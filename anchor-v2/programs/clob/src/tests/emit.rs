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
            assert_pod_matches_event, write_execute_record, write_fill_record, CancelAllRecord,
            LogBuf, EXECUTE_RECORD_LOG_BYTES, FILL_RECORD_LOG_BYTES,
        },
        events::{
            CrankConditionsRecordV0, ExecuteRecordV0, FillEntryV0, FillRecordV0, FillSlimV0,
            MarketAuthorityAcceptedRecordV0, MarketAuthorityProposedRecordV0, MarketCloseRecordV0,
            MarketResizeRecordV0, OrderCancelRecordV0, OrderEvictRecordV0, OrderExpireRecordV0,
            OrderPlaceRecordV0, OrdersCancelRecordV0,
        },
        state::{
            CancelAllOutcome, CANCEL_ALL_ORDERS_CEILING, EXECUTE_FILLS_CEILING, FILL_BATCH_CEILING,
        },
    },
    anchor_lang::prelude::*,
};

fn authority() -> Address {
    Address::new_from_array([0xAB; 32])
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
        flags: 5,
        _pad: [0; 2],
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

#[test]
fn the_authority_records_emit_the_bytes_the_event_impl_would() {
    let key = |seed: u8| Address::new_from_array([seed; 32]);
    assert_pod_matches_event!(MarketAuthorityProposedRecordV0 {
        market: key(1),
        authority: key(2),
        proposed_authority: key(3),
        ts: -7,
    });
    assert_pod_matches_event!(MarketAuthorityAcceptedRecordV0 {
        market: key(1),
        previous_authority: key(2),
        authority: key(3),
        ts: -7,
    });
}

#[test]
fn the_market_admin_records_emit_the_bytes_the_event_impl_would() {
    let key = |seed: u8| Address::new_from_array([seed; 32]);
    assert_pod_matches_event!(MarketResizeRecordV0 {
        market: key(1),
        authority: key(2),
        ts: -7,
        previous_capacity: 9,
        capacity: 11,
    });
    assert_pod_matches_event!(MarketCloseRecordV0 {
        market: key(1),
        authority: key(2),
        rent_recipient: key(3),
        ts: -7,
    });
    assert_pod_matches_event!(CrankConditionsRecordV0 {
        market: key(1),
        place_authority: key(2),
        expiry_program: key(3),
        activation_program: key(4),
        capacity_program: key(5),
        cross_program: key(6),
        ts: -7,
        account_count: 8,
        _pad: [0; 4],
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
                bid_reduce_only_orders: 1,
                ask_reduce_only_orders: 0,
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
                bid_reduce_only_orders: 0,
                ask_reduce_only_orders: 0,
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
        let cancelled: Vec<u32> = cancelled_client_order_id.into_iter().collect();
        write_execute_record(&mut log, -7, 9, 23, 1, &fills, &cancelled).unwrap();
        let record = ExecuteRecordV0 {
            ts: -7,
            slot: 9,
            market_index: 23,
            direction: 1,
            fills: fills.clone(),
            cancelled_client_order_ids: cancelled.clone(),
        };

        assert_eq!(
            log.as_slice(),
            Event::data(&record).as_slice(),
            "execute record with {} fills diverged from Event::data()",
            fills.len()
        );
    }
}

/// `fill_v0`'s record is also variable-length, checked at the same shapes:
/// no fills, a cull with no fills, and the batch ceiling.
#[test]
fn the_fill_record_emits_the_bytes_the_event_impl_would() {
    let fill = |i: u64| FillEntryV0 {
        order_id: i,
        owner: authority(),
        price: 200 + i,
        base_size: 100 + i,
        client_order_id: 1_000 + i as u32,
    };
    let shapes: [(Vec<FillEntryV0>, Option<u32>); 4] = [
        (vec![], None),
        (vec![], Some(77)),
        (vec![fill(1), fill(2), fill(3)], Some(77)),
        (
            (0..FILL_BATCH_CEILING as u64).map(fill).collect(),
            Some(u32::MAX),
        ),
    ];

    for (fills, cancelled_client_order_id) in shapes {
        let mut log = Box::new(LogBuf::<FILL_RECORD_LOG_BYTES>::new());
        let cancelled: Vec<u32> = cancelled_client_order_id.into_iter().collect();
        write_fill_record(&mut log, -7, 9, 23, &fills, &cancelled).unwrap();
        let record = FillRecordV0 {
            ts: -7,
            slot: 9,
            market_index: 23,
            fills: fills.clone(),
            cancelled_client_order_ids: cancelled.clone(),
        };

        assert_eq!(
            log.as_slice(),
            Event::data(&record).as_slice(),
            "fill record with {} fills diverged from Event::data()",
            fills.len()
        );
    }
}
